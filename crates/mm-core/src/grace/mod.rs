//! PF-5: grace periods for lockless readers (Linux RCU's role for the page cache after 4c0f369, built the
//! epoch way: crossbeam-epoch / FreeBSD SMR, not Linux's code).
//!
//! A reader enters a `Section` (non-sleeping, preemption off, short: one look-up and one copy step), finds data
//! through `rcu_index::RcuIndex` without a lock or a reference and may use it until the section ends. A writer
//! that takes something out hands it to `defer_*`; it is dropped only after a grace period: once every section
//! that could have seen it has ended.
//!
//! The mechanism: a global epoch E and one word per section slot (0: free; `E << 1 | 1`: a section entered in
//! epoch E). E may advance to E+1 only when no slot holds a section of an older epoch; something unlinked while
//! E was current (its cookie) is safe once E reached cookie + 2. So there is no TLB flush, no IPI and no
//! per-CPU acknowledgement: an idle CPU, a CPU in user space and a CPU in a long syscall without a section all
//! hold no slot and never delay anything; only a CPU inside a section does, for the length of that section.
//! `hal::switch`'s sync generations (sched/reap.rs's quiescent points) cannot serve here: they flush every TLB
//! per generation, count IPI'd idle CPUs and pass a CPU in the resched IPI even inside a reader's critical
//! section (plan/notes/pf-scout-memory.md section 3). The reaper thread is still the background driver: its
//! passes call `drain` (sched/reap.rs).
//!
//! Waiting memory is a third state of a frame (Linus's list on 4c0f369, plan/notes/pf-scout-memory.md section 4):
//! - counted: `pending_bytes` (part of the owner's `used_by_kind`, also in `FrameStats::pending_bytes`), counted
//!   as free by MemAvailable and kswapd;
//! - drained by the allocator before it answers out-of-memory (`Buddy::alloc`, `alloc_reclaiming`,
//!   `alloc_below`): passed batches at once, and as the last step a bounded wait for the current grace period
//!   (`expedite`), at most one wait that runs out per allocation (`WaitBudget`, B4); the heap does the same
//!   after it let go of its magazine lock (`KernelHeap::alloc`, never inside the buddy for `Kind::Heap`); an
//!   interrupt handler never drains and a CPU inside a section never waits (`in_section`);
//! - bounded: above `bound_bytes` (4 MiB per CPU, at least 16 MiB, at most 1/16 of memory; pending frames plus
//!   the heap bytes the waiting items keep, so index nodes count too) the freeing thread itself waits for its
//!   grace period (`defer_*` when it holds no spin lock): back-pressure on the freer instead of an IPI storm.
//!
//! The unsafe parts: the section slots, the type-erased deferred drops, the raw pointers of `rcu_index`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::marker::PhantomData;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::compiler_fence;
use core::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use ksync::PreemptGuard;

mod queue;
mod retire;

pub use queue::{
    bound_bytes, bound_for_test, defer_arc, defer_arc_nowait, defer_box, drain, expedite, expedite_with, kick, pending_bytes, set_kick, stats, waiting,
    BoundOverride, GraceStats, WaitBudget,
};
pub use retire::Retire;
use queue::TIMEOUTS;

/// The global epoch. Starts at 2 so that `cookie + 2` never wraps from the start.
pub(super) static EPOCH: AtomicU64 = AtomicU64::new(2);

/// One section slot on its own cache line (each CPU writes its own; the advance only reads them). `depth`:
/// sections open on the CPU whose hint slot this is (slot i is CPU i's), whatever slot they took
/// (`in_section`).
#[repr(align(64))]
struct Slot {
    word: AtomicU64,
    depth: AtomicU32,
}

/// A run of slots; chunks are only ever added (never freed): a slot stays valid for the kernel's life.
struct Chunk {
    slots: Box<[Slot]>,
    next: AtomicPtr<Chunk>,
}

static HEAD: AtomicPtr<Chunk> = AtomicPtr::new(core::ptr::null_mut());
static GROWING: AtomicBool = AtomicBool::new(false);
pub(super) static SLOTS: AtomicU64 = AtomicU64::new(0);

fn chunks() -> impl Iterator<Item = &'static Chunk> {
    let mut p = HEAD.load(Acquire);
    core::iter::from_fn(move || {
        if p.is_null() {
            return None;
        }
        // SAFETY: chunks are fully built before they are published (Release) and never freed.
        let c = unsafe { &*p };
        p = c.next.load(Acquire);
        Some(c)
    })
}

/// Adds a chunk of `n` free slots (fallible: None when the heap has no room). Interrupts stay on; a racing
/// grower makes the caller look again instead of waiting.
fn grow(n: usize) -> bool {
    if GROWING.compare_exchange(false, true, Acquire, Relaxed).is_err() {
        return true;
    }
    let mut v: Vec<Slot> = Vec::new();
    let ok = v.try_reserve_exact(n).is_ok();
    if ok {
        v.extend((0..n).map(|_| Slot { word: AtomicU64::new(0), depth: AtomicU32::new(0) }));
    }
    let c = ok.then(|| try_box(Chunk { slots: v.into_boxed_slice(), next: AtomicPtr::new(core::ptr::null_mut()) })).flatten();
    let done = match c {
        Some(c) => {
            let c = Box::into_raw(c);
            let mut tail = &HEAD;
            loop {
                let p = tail.load(Acquire);
                if p.is_null() {
                    break;
                }
                // SAFETY: as in `chunks`.
                tail = unsafe { &(*p).next };
            }
            tail.store(c, Release);
            SLOTS.fetch_add(n as u64, Relaxed);
            true
        }
        None => false,
    };
    GROWING.store(false, Release);
    done
}

/// `Box::new` that answers None instead of aborting when the heap is out of room (R2).
pub fn try_box<T>(v: T) -> Option<Box<T>> {
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        return Some(Box::new(v));
    }
    // SAFETY: the layout is T's and non-zero sized; on success the memory is initialised with `v` before the
    // Box takes it, and a Box of T frees it with the same layout.
    unsafe {
        let p = alloc::alloc::alloc(layout) as *mut T;
        if p.is_null() {
            return None;
        }
        p.write(v);
        Some(Box::from_raw(p))
    }
}

/// A read section. While it lives, nothing a reader found through `rcu_index` is freed. It is not `Send`
/// (it holds this CPU's slot with preemption off): a section must not sleep, yield or wait for anything a
/// writer may hold, and stays short (one batch of look-ups and copies).
pub struct Section {
    slot: &'static Slot,
    /// This CPU's hint slot, whose `depth` counts the section (None when the CPU has no slot of its own, or
    /// before CPUs have numbers).
    home: Option<&'static Slot>,
    _cpu: PreemptGuard,
    _not_send: PhantomData<*mut ()>,
}

const fn active(e: u64) -> u64 {
    (e << 1) | 1
}

/// Enters a read section; None only when no slot is free and the heap has no room for more (the caller then
/// takes its locked path). A nested section takes a second slot.
pub fn enter() -> Option<Section> {
    let cpu = PreemptGuard::new();
    let me = hal::percpu::this_cpu();
    let slot = claim(me)?;
    let home = if per_cpu_ready() { slot_at(me) } else { None };
    if let Some(h) = home {
        depth_add(h, 1);
    }
    Some(Section { slot, home, _cpu: cpu, _not_send: PhantomData })
}

/// PF-7: `depth` is written only by its CPU, with preemption off. An interrupt between the load and the store has
/// closed its own section before it returns, so the store writes what the word holds: no locked instruction (a
/// section is part of every read syscall, twice).
fn depth_add(h: &Slot, d: u32) {
    h.depth.store(h.depth.load(Relaxed).wrapping_add(d), Relaxed);
}

/// Takes a free slot (`hint` first: this CPU's, always free unless sections nest) and pins it at the current
/// epoch.
fn claim(hint: usize) -> Option<&'static Slot> {
    loop {
        let mut e = EPOCH.load(SeqCst);
        let take = |s: &'static Slot| s.word.compare_exchange(0, active(e), SeqCst, Relaxed).is_ok();
        let own = slot_at(hint).filter(|s| take(s));
        let found = own.or_else(|| chunks().flat_map(|c| c.slots.iter()).find(|s| take(s)));
        let Some(slot) = found else {
            let total = SLOTS.load(Relaxed) as usize;
            if !grow(hal::percpu::possible_cpus().max(4).max(total)) {
                return None;
            }
            continue;
        };
        // Pinned at the epoch read before the store; make it the epoch after the store, so that what the
        // section loads afterwards is never older than the epoch it is counted in. PF-7: the store and the loads
        // after it need the order store-then-load. The locked compare-exchange (and `xchg`, the SeqCst store) is
        // already that barrier on x86-64: Linux's `smp_mb__after_atomic` is empty for the same reason
        // (linux-6.6/arch/x86/include/asm/barrier.h); an `mfence` on top cost 33 cycles per section. A host of
        // another architecture keeps the fence.
        loop {
            #[cfg(target_arch = "x86_64")]
            compiler_fence(SeqCst);
            #[cfg(not(target_arch = "x86_64"))]
            fence(SeqCst);
            let now = EPOCH.load(SeqCst);
            if now == e {
                break;
            }
            e = now;
            slot.word.store(active(e), SeqCst);
        }
        return Some(slot);
    }
}

fn slot_at(i: usize) -> Option<&'static Slot> {
    let mut base = 0usize;
    for c in chunks() {
        if i < base + c.slots.len() {
            return Some(&c.slots[i - base]);
        }
        base += c.slots.len();
    }
    None
}

impl Drop for Section {
    fn drop(&mut self) {
        self.slot.word.store(0, Release);
        // Preemption is still off (the guard drops after this): the same CPU as at `enter`.
        if let Some(h) = self.home {
            depth_add(h, u32::MAX);
        }
    }
}

/// Whether a section is open on this CPU (the caller's own, or the thread an interrupt handler interrupted):
/// a wait for a grace period here would wait for that section, so it is not started (`synchronize`).
pub fn in_section() -> bool {
    let _cpu = PreemptGuard::new();
    per_cpu_ready() && slot_at(hal::percpu::this_cpu()).is_some_and(|s| s.depth.load(Relaxed) != 0)
}

/// Whether CPUs have their own number yet (before `percpu::init_bsp`, and in host tests, every thread is "CPU
/// 0": no depth is counted then, and `in_section` answers false).
fn per_cpu_ready() -> bool {
    hal::percpu::gs_self() != 0
}

/// Tests only (ktests of the waiting memory): holds the grace period like an open section, but without
/// keeping preemption off, so the holder may sleep and take locks meanwhile. Everything deferred while it lives
/// waits; bounded waits run out (`synchronize` answers false). Never in kernel code paths.
pub struct TestHold {
    slot: &'static Slot,
}

pub fn hold_for_test() -> Option<TestHold> {
    claim(usize::MAX).map(|slot| TestHold { slot })
}

impl Drop for TestHold {
    fn drop(&mut self) {
        self.slot.word.store(0, Release);
    }
}

/// Advances the epoch once if no section of an older epoch is open. True when the epoch moved (by this call
/// or a racing one).
pub fn try_advance() -> bool {
    let e = EPOCH.load(SeqCst);
    fence(SeqCst);
    for c in chunks() {
        for s in c.slots.iter() {
            let v = s.word.load(SeqCst);
            if v & 1 != 0 && v >> 1 != e {
                return false;
            }
        }
    }
    let _ = EPOCH.compare_exchange(e, e + 1, SeqCst, SeqCst);
    true
}

/// The epoch now, after everything this thread unlinked before (a deferred item's stamp).
pub(super) fn cookie() -> u64 {
    fence(SeqCst);
    EPOCH.load(SeqCst)
}

pub(super) fn passed(c: u64) -> bool {
    EPOCH.load(SeqCst) >= c + 2
}

/// The current epoch (tests, the `grace` numbers).
pub fn epoch() -> u64 {
    EPOCH.load(Relaxed)
}

/// Sections open now (the stall report: who holds the grace period).
pub fn open_sections() -> usize {
    chunks().map(|c| c.slots.iter().filter(|s| s.word.load(Relaxed) & 1 != 0).count()).sum()
}

/// How long a bounded wait for a grace period may spin (B4): far above any read section (one batch of copies).
pub const WAIT_NS: u64 = 10_000_000;
/// Spins of a wait when no clock is installed (early boot, host tests without a scheduler).
const WAIT_SPINS: u64 = 1 << 24;

/// Waits until everything deferred before this call is past its grace period: true; false when sections kept
/// it longer than `WAIT_NS` (a reader stuck in a section: a bug, reported by the reaper). Spins (it may run
/// with spin locks held). Inside a section (`in_section`) it answers false at once: it would wait for that
/// section itself until the bound.
pub fn synchronize() -> bool {
    if in_section() {
        return false;
    }
    let target = cookie();
    let t0 = ksync::now_ns();
    let mut spins = 0u64;
    loop {
        if passed(target) {
            return true;
        }
        try_advance();
        if passed(target) {
            return true;
        }
        spins += 1;
        match (t0, ksync::now_ns()) {
            (Some(a), Some(b)) if b.saturating_sub(a) > WAIT_NS => break,
            (None, _) | (_, None) if spins > WAIT_SPINS => break,
            _ => {}
        }
        core::hint::spin_loop();
    }
    TIMEOUTS.fetch_add(1, Relaxed);
    false
}
