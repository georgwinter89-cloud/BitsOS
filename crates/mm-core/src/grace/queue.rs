//! PF-5: the deferred drops of `grace` (a type-erased queue in cookie order), the drains, the bound and the
//! back-pressure on the freer, the numbers.

use super::{cookie, in_section, open_sections, passed, synchronize, try_advance, EPOCH, SLOTS};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering::*};
use ksync::{OnceRef, SpinLock};

pub(super) struct Deferred {
    pub(super) cookie: u64,
    pub(super) ptr: *mut (),
    pub(super) drop: unsafe fn(*mut ()),
    pub(super) bytes: u64,
    /// Heap bytes the item keeps (its own size, about): counted against the bound with `bytes`, so items
    /// that free no frames (index nodes, references) cannot pile up without limit either.
    pub(super) held: u64,
}

// SAFETY: `ptr` is an owned Box<T> or Arc<T> of a `T: Send + Sync` (`defer_*` require it); it is only
// dropped once, by whoever pops the entry.
unsafe impl Send for Deferred {}

impl Deferred {
    /// SAFETY: the caller owns the entry (popped from the queue or never queued) and drops it once.
    unsafe fn release(self) {
        // SAFETY: see the function's contract; `drop` matches the pointer's type (`defer_*`).
        unsafe { (self.drop)(self.ptr) }
    }
}

static QUEUE: SpinLock<VecDeque<Deferred>> = SpinLock::named("mm.grace", VecDeque::new());
static PENDING: AtomicU64 = AtomicU64::new(0);
static HELD: AtomicU64 = AtomicU64::new(0);
static WAITING: AtomicU64 = AtomicU64::new(0);
static DRAINED: AtomicU64 = AtomicU64::new(0);
static EXPEDITES: AtomicU64 = AtomicU64::new(0);
static THROTTLED: AtomicU64 = AtomicU64::new(0);
pub(super) static TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static LEAKED: AtomicU64 = AtomicU64::new(0);
static BOUND_OVERRIDE: AtomicU64 = AtomicU64::new(0);

unsafe fn drop_box<T>(p: *mut ()) {
    // SAFETY: `p` came from `Box::into_raw` of a Box<T> (`defer_box`), dropped once.
    drop(unsafe { Box::from_raw(p as *mut T) });
}

/// What an `Arc<T>` keeps on the heap (its value and the two counts).
pub(super) const fn arc_held<T>() -> u64 {
    (core::mem::size_of::<T>() + 2 * core::mem::size_of::<usize>()) as u64
}

pub(super) unsafe fn drop_arc<T>(p: *mut ()) {
    // SAFETY: `p` came from `Arc::into_raw` of an Arc<T> (one strong count, `defer_arc`/`Retire`), dropped once.
    drop(unsafe { Arc::from_raw(p as *const T) });
}

/// `wait`: the freer may pay the back-pressure (`after_push`); false for freers that run under another
/// cache's lock (reclaim: the allocation that called it waits instead).
pub(super) fn push(d: Deferred, wait: bool) {
    {
        let mut q = QUEUE.lock();
        if q.try_reserve(1).is_ok() {
            let d = Deferred { cookie: cookie(), ..d };
            PENDING.fetch_add(d.bytes, Relaxed);
            HELD.fetch_add(d.held, Relaxed);
            WAITING.fetch_add(1, Relaxed);
            let first = q.is_empty();
            q.push_back(d);
            drop(q);
            if wait {
                after_push(first);
            }
            return;
        }
    }
    // No room in the queue. INT-W28 (PF-6 CR): a freer under a spin lock (rcu_index's writers under
    // vfs.fds.view or vfs.inode.index) never waits here; the item is leaked, counted, rather than freed early.
    if ksync::preempt_count() != 0 || hal::percpu::in_hard_irq() {
        LEAKED.fetch_add(1, Relaxed);
        klog::event!(Warn, "mm", "grace.leak_no_room", held = d.held, bytes = d.bytes);
        return;
    }
    // Otherwise wait for the grace period here; if a section outlives the bound, the item is leaked rather than
    // freed early.
    if synchronize() {
        // SAFETY: never queued, so owned here; its grace period passed.
        unsafe { d.release() };
    } else {
        LEAKED.fetch_add(1, Relaxed);
    }
}

/// Drops `b` after a grace period. `bytes`: frames its drop frees (counted as pending meanwhile).
pub fn defer_box<T: Send + Sync + 'static>(b: Box<T>, bytes: u64) {
    let held = core::mem::size_of::<T>() as u64;
    push(Deferred { cookie: 0, ptr: Box::into_raw(b) as *mut (), drop: drop_box::<T>, bytes, held }, true);
}

/// Drops this reference after a grace period. `bytes`: frames its drop frees, if it is the last one.
pub fn defer_arc<T: Send + Sync + 'static>(a: Arc<T>, bytes: u64) {
    push(Deferred { cookie: 0, ptr: Arc::into_raw(a) as *mut (), drop: drop_arc::<T>, bytes, held: arc_held::<T>() }, true);
}

/// Frees what is past its grace period (advancing the epoch once if it can); never waits. Returns the bytes
/// freed. Thread context only (the drops free heap memory and frames): an interrupt handler gets 0.
pub fn drain() -> u64 {
    drain_with(false)
}

/// `drain`; `wait_lock`: wait for the queue lock instead of giving up when a freer holds it (only `expedite`,
/// which may wait anyway; the queue lock is held only to push or pop one item).
fn drain_with(wait_lock: bool) -> u64 {
    if hal::percpu::in_hard_irq() || WAITING.load(Relaxed) == 0 {
        return 0;
    }
    // Two advances pass what was deferred in the epoch before (when no section of it is open, both succeed).
    if try_advance() {
        try_advance();
    }
    let mut freed = 0u64;
    loop {
        let d = {
            let q = if wait_lock { Some(QUEUE.lock()) } else { QUEUE.try_lock() };
            let Some(mut q) = q else { break };
            // Cookies grow along the queue (stamped under its lock): the first that has not passed ends it.
            match q.front() {
                Some(d) if passed(d.cookie) => q.pop_front(),
                _ => None,
            }
        };
        let Some(d) = d else { break };
        PENDING.fetch_sub(d.bytes, Relaxed);
        HELD.fetch_sub(d.held, Relaxed);
        WAITING.fetch_sub(1, Relaxed);
        freed += d.bytes;
        // SAFETY: popped, so owned here; its grace period passed (checked under the queue lock).
        unsafe { d.release() };
    }
    DRAINED.fetch_add(freed, Relaxed);
    freed
}

/// One caller's right to wait for grace periods (an allocation's steps share one): once a wait ran out (a
/// section held the grace period past `WAIT_NS`, a bug the reaper reports), the caller's later steps only drain,
/// so one allocation spins at most `WAIT_NS` in all (B4), however many reclaim rounds it makes.
#[derive(Clone, Copy, Debug, Default)]
pub struct WaitBudget {
    spent: bool,
}

/// The allocator's last step before out-of-memory, and the freer's back-pressure: waits (bounded) for the
/// grace period of everything deferred so far, then drains. Returns the bytes freed.
pub fn expedite() -> u64 {
    expedite_with(&mut WaitBudget::default())
}

/// `expedite` within `b`. It never waits inside a section on this CPU (that section would hold its own grace
/// period: drain only) or in an interrupt handler (nothing); with a spin lock held (maybe the queue's own lock:
/// the heap grows under it) the drain only tries the queue lock.
pub fn expedite_with(b: &mut WaitBudget) -> u64 {
    if hal::percpu::in_hard_irq() || WAITING.load(Relaxed) == 0 {
        return 0;
    }
    if b.spent || in_section() {
        return drain();
    }
    EXPEDITES.fetch_add(1, Relaxed);
    if !synchronize() {
        b.spent = true;
    }
    drain_with(ksync::preempt_count() == 0)
}

/// Back-pressure: above `bound_bytes` (pending frames plus the heap bytes the items keep) the freeing thread waits for its own grace period and drains; below it,
/// the first item of a batch wakes the reaper (which drains in the background). Both only when the freer holds
/// no spin lock (a waiting holder would delay others, a wake-up takes the scheduler's lock; a mutex holder may
/// do both); under a spin lock the item just waits for the reaper's next pass or the next allocation's drain.
fn after_push(first: bool) {
    if ksync::preempt_count() != 0 || hal::percpu::in_hard_irq() {
        return;
    }
    if PENDING.load(Relaxed).saturating_add(HELD.load(Relaxed)) > bound_bytes() {
        THROTTLED.fetch_add(1, Relaxed);
        expedite();
    } else if first {
        kick();
    }
}

/// Frames waiting for their grace period (bytes).
pub fn pending_bytes() -> u64 {
    PENDING.load(Relaxed)
}

/// What may wait before the freer pays (Linux expedites above 4 MiB per CPU): 4 MiB per CPU, at least 16 MiB,
/// at most 1/16 of memory (so R2's POOL_KEEP always covers it).
pub fn bound_bytes() -> u64 {
    let o = BOUND_OVERRIDE.load(Relaxed);
    if o != 0 {
        return o;
    }
    let per_cpu = (hal::percpu::cpu_count().max(1) as u64).saturating_mul(4 << 20).max(16 << 20);
    match crate::frames().map(|f| f.stats().total_bytes) {
        Some(t) if t > 0 => per_cpu.min(t / 16).max(crate::PAGE * 64),
        _ => per_cpu,
    }
}

/// Tests: a smaller bound while the guard lives (its drop restores the computed one, also when the test fails
/// in between).
pub fn bound_for_test(bytes: u64) -> BoundOverride {
    BOUND_OVERRIDE.store(bytes, Relaxed);
    BoundOverride(())
}

/// The guard of `bound_for_test`.
pub struct BoundOverride(());

impl Drop for BoundOverride {
    fn drop(&mut self) {
        BOUND_OVERRIDE.store(0, Relaxed);
    }
}

/// The numbers of the grace machinery (metrics, `hw_info`, tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraceStats {
    pub epoch: u64,
    pub pending_bytes: u64,
    /// Heap bytes the waiting items keep (index nodes, page structs), counted against the bound too.
    pub held_bytes: u64,
    /// Items waiting (pages, index nodes).
    pub waiting: u64,
    pub drained_bytes: u64,
    pub expedites: u64,
    /// Freers that waited because pending was above the bound.
    pub throttled: u64,
    /// Bounded waits that ran out (a section held the grace period longer than `WAIT_NS`).
    pub timeouts: u64,
    /// Items leaked because neither the queue nor a wait could take them (never freed early).
    pub leaked: u64,
    pub open_sections: u64,
    pub slots: u64,
    pub bound_bytes: u64,
}

pub fn stats() -> GraceStats {
    GraceStats {
        epoch: EPOCH.load(Relaxed),
        pending_bytes: PENDING.load(Relaxed),
        held_bytes: HELD.load(Relaxed),
        waiting: WAITING.load(Relaxed),
        drained_bytes: DRAINED.load(Relaxed),
        expedites: EXPEDITES.load(Relaxed),
        throttled: THROTTLED.load(Relaxed),
        timeouts: TIMEOUTS.load(Relaxed),
        leaked: LEAKED.load(Relaxed),
        open_sections: open_sections() as u64,
        slots: SLOTS.load(Relaxed),
        bound_bytes: bound_bytes(),
    }
}

/// Whether deferred items wait (the reaper keeps passing while they do).
pub fn waiting() -> bool {
    WAITING.load(Relaxed) > 0
}

static KICK: OnceRef<dyn Fn() + Sync> = OnceRef::new();

/// The reaper installs its wake-up (sched/reap.rs); `kick` calls it.
pub fn set_kick(f: &'static (dyn Fn() + Sync)) {
    let _ = KICK.set(f);
}

/// Wakes the background driver (never from inside a lock the scheduler takes).
pub fn kick() {
    if let Some(f) = KICK.get() {
        f();
    }
}
