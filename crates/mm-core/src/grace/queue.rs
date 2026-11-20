//! PF-5: the deferred drops of `grace` (a type-erased queue in cookie order), the drains, the bound and the
//! back-pressure on the freer, the numbers.

use super::{cookie, in_section, open_sections, passed, synchronize, try_advance, try_box, EPOCH, SLOTS};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use ksync::{OnceRef, SpinLock};

mod overflow;

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

/// Items in one block of the queue: one page of them.
const BLOCK: usize = crate::PAGE as usize / core::mem::size_of::<Deferred>();

/// One page of waiting items and the link to the next younger block.
struct Block {
    /// Made with room for `BLOCK` items; never grows.
    items: VecDeque<Deferred>,
    /// Null for the youngest block (and for a block outside the chain).
    next: *mut Block,
}

impl Block {
    /// A block with its page, or None without memory. Never call it under `mm.grace`.
    fn try_new() -> Option<Box<Block>> {
        // Without a block the item takes the old road (drain, wait): no never-failing loop in a freeing path.
        let _bounded = crate::oom::no_retry();
        let mut items = VecDeque::new();
        items.try_reserve_exact(BLOCK).ok()?;
        try_box(Block { items, next: core::ptr::null_mut() })
    }
}

/// The waiting items, oldest first, in blocks of one page chained oldest to youngest. One `VecDeque` of all of
/// them doubled at 2.6, 5 and 10 MiB in one run, exactly when memory is full of frames that wait here: the growth
/// failed under the queue's lock, where nothing could drain, and index nodes freed under a spin lock were leaked.
/// Thousands of leaked nodes, allocated all over memory while a file was cached, left no free block above 8 MiB
/// for the machine's whole life (PF-7 fixer: `mm.reclaim.dirty_pressure`, then ENOMEM for 9 MiB reads).
/// A list of the blocks still doubled under the lock (16 KiB at 26 000 items; ATOMIC-3):
/// chained, a queue of any length needs nothing but its blocks, which are made with the lock released.
struct Queue {
    /// The oldest block (from `Box::into_raw`, owned by the queue), null when empty. No block in the chain is
    /// empty.
    head: *mut Block,
    /// The youngest block, the last of the chain; null when empty.
    tail: *mut Block,
    /// The last block that ran empty: a queue that keeps going from empty to one item allocates nothing.
    spare: Option<Box<Block>>,
}

// SAFETY: the chain's blocks are owned by the queue alone (nothing else holds a pointer into it) and are only
// reached through `&mut Queue`/`&Queue`; the items are `Send` (`Deferred`).
unsafe impl Send for Queue {}

impl Queue {
    const fn new() -> Queue {
        Queue { head: core::ptr::null_mut(), tail: core::ptr::null_mut(), spare: None }
    }

    fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    fn tail_mut(&mut self) -> Option<&mut Block> {
        // SAFETY: null or the last block of the chain, owned by the queue; `&mut self` makes this the only
        // reference into it.
        unsafe { self.tail.as_mut() }
    }

    /// Whether `push_back` has room now, after linking a block if it must: the spare, or `fresh` (made by the
    /// caller with the lock released). Err: a new block is needed and `fresh` has none (the caller makes it with
    /// the lock released and comes again). Linking allocates nothing.
    fn room(&mut self, fresh: &mut Option<Box<Block>>) -> Result<(), ()> {
        if self.tail_mut().is_some_and(|b| b.items.len() < BLOCK) {
            return Ok(());
        }
        let Some(mut block) = self.spare.take().or_else(|| fresh.take()) else { return Err(()) };
        block.next = core::ptr::null_mut();
        let p = Box::into_raw(block);
        match self.tail_mut() {
            Some(t) => t.next = p,
            None => self.head = p,
        }
        self.tail = p;
        Ok(())
    }

    /// After `room` answered Ok: within the block's capacity, nothing allocates.
    fn push_back(&mut self, d: Deferred) {
        if let Some(b) = self.tail_mut() {
            b.items.push_back(d);
        }
    }

    fn front_cookie(&self) -> Option<u64> {
        // SAFETY: null or the chain's first block, owned by the queue, read through `&self`.
        unsafe { self.head.as_ref() }.and_then(|b| b.items.front()).map(|d| d.cookie)
    }

    /// The oldest item, and a block that ran empty and is not kept, for the caller to drop after the lock.
    fn pop_front(&mut self) -> (Option<Deferred>, Option<Box<Block>>) {
        // SAFETY: null or the chain's first block, owned by the queue; `&mut self` makes this the only reference.
        let Some(b) = (unsafe { self.head.as_mut() }) else { return (None, None) };
        let d = b.items.pop_front();
        if !b.items.is_empty() {
            return (d, None);
        }
        let first = self.head;
        self.head = b.next;
        if self.head.is_null() {
            self.tail = core::ptr::null_mut();
        }
        // SAFETY: `first` came from `Box::into_raw` (`room`) and is out of the chain now: nothing points to it.
        let mut empty = unsafe { Box::from_raw(first) };
        empty.next = core::ptr::null_mut();
        if self.spare.is_none() {
            self.spare = Some(empty);
            return (d, None);
        }
        (d, Some(empty))
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        let mut p = core::mem::replace(&mut self.head, core::ptr::null_mut());
        self.tail = core::ptr::null_mut();
        while !p.is_null() {
            // SAFETY: every block of the chain came from `Box::into_raw` and is freed once, here.
            let b = unsafe { Box::from_raw(p) };
            p = b.next;
        }
    }
}

static QUEUE: SpinLock<Queue> = SpinLock::named("mm.grace", Queue::new());
/// One drain for room at a time: a drop in that drain that defers and finds no room either takes the old road.
static ROOM_DRAIN: AtomicBool = AtomicBool::new(false);
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
/// cache's lock (reclaim: the allocation that called it waits instead). Those never wait for `mm.grace`: their
/// items go to this CPU's stash (`stash`), which the next drain hands over in one hold of the lock.
pub(super) fn push(d: Deferred, wait: bool) {
    count_in(&d);
    let d = if wait {
        d
    } else {
        match stash(d).or_else(spill) {
            Ok(()) => return,
            Err(d) => d,
        }
    };
    let Err(d) = enqueue(d, wait) else { return };
    // No page for a block with the queue's lock held, where nothing drains; what waits may be what the page would
    // come from. Drains already run under a freer's spin lock (the heap's last step before out-of-memory).
    let d = if ROOM_DRAIN.swap(true, Acquire) {
        d
    } else {
        drain();
        ROOM_DRAIN.store(false, Release);
        match enqueue(d, wait) {
            Ok(()) => return,
            Err(d) => d,
        }
    };
    count_out(&d);
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

/// An item's numbers join the waiting ones (once, in `push`; the drain that frees it takes them out).
fn count_in(d: &Deferred) {
    PENDING.fetch_add(d.bytes, Relaxed);
    HELD.fetch_add(d.held, Relaxed);
    WAITING.fetch_add(1, Relaxed);
}

fn count_out(d: &Deferred) {
    PENDING.fetch_sub(d.bytes, Relaxed);
    HELD.fetch_sub(d.held, Relaxed);
    WAITING.fetch_sub(1, Relaxed);
}

/// `d` into the queue, stamped under its lock; back to the caller when there is no room. `wait` false (reclaim):
/// a held lock is waited for only after a try, and that wait is counted (`mm.grace.reclaim_waits`, which the stash
/// keeps at 0 unless every stash of the CPU is full or busy).
fn enqueue(d: Deferred, wait: bool) -> Result<(), Deferred> {
    // A block's page is never allocated under `mm.grace` (FS-STALL review R1: an allocation there that reclaimed
    // would free into this queue and spin on its own lock).
    let mut fresh: Option<Box<Block>> = None;
    let mut q = loop {
        let mut q = match QUEUE.try_lock() {
            Some(q) => q,
            None => {
                if !wait {
                    RECLAIM_WAITS.inc();
                }
                QUEUE.lock()
            }
        };
        match q.room(&mut fresh) {
            Ok(()) => break q,
            Err(()) if fresh.is_none() => {
                drop(q);
                let Some(b) = Block::try_new() else { return Err(d) };
                fresh = Some(b);
            }
            Err(()) => return Err(d),
        }
    };
    // A block allocated for nothing (another freer made room meanwhile) is the spare, or goes after the lock.
    if q.spare.is_none() {
        q.spare = fresh.take();
    }
    let first = q.is_empty();
    q.push_back(Deferred { cookie: cookie(), ..d });
    drop(q);
    drop(fresh);
    if wait {
        after_push(first);
    }
    Ok(())
}

/// MEMCRASH (Mini-PC 13.11.2026): every page reclaim took and every cache its walk visited went into the queue
/// one by one under `mm.grace`, from all CPUs at once; the last 200 events before the crash were waits for that
/// lock, one of them inside a heap allocation's reclaim (B4: reclaim never waits for a lock). Reclaim's items now
/// go to a small per-CPU stash, stamped when they are handed over (a later stamp only delays the free), and the
/// next drain moves a whole stash into the queue in one hold (`flush_stashes`). When every stash is full or held
/// the item goes to the overflow pool (`spill`, no lock); only when that is full too does it go to the queue as
/// before, counted when that waited (`mm.grace.reclaim_waits`).
const STASHES: usize = 32;
const STASH_ITEMS: usize = 64;

struct Stash {
    items: [Option<Deferred>; STASH_ITEMS],
    n: usize,
}

impl Stash {
    const fn new() -> Stash {
        Stash { items: [const { None }; STASH_ITEMS], n: 0 }
    }
}

static STASH: [SpinLock<Stash>; STASHES] = [const { SpinLock::named("mm.grace.stash", Stash::new()) }; STASHES];
/// Items in the stashes now (a drain with none looks at no stash).
static STASHED: AtomicU64 = AtomicU64::new(0);
klog::counter!(HANDOVER, "mm.grace.reclaim_handover");
klog::counter!(RECLAIM_WAITS, "mm.grace.reclaim_waits");

/// `d` into this CPU's stash, stamped now. A full stash is handed over first if the queue's lock is free; when it
/// stays full or another holder has it (an interrupt on top of its holder, a drain moving it), the other CPUs'
/// stashes are tried. Back only when every stash is full or held: the caller then queues it itself.
fn stash(d: Deferred) -> Result<(), Deferred> {
    let d = Deferred { cookie: cookie(), ..d };
    let me = hal::percpu::this_cpu() % STASHES;
    let mut d = Some(d);
    for k in 0..STASHES {
        let i = (me + k) % STASHES;
        // Any full stash on the way is handed over when the queue's lock is free (not only this CPU's).
        if STASH[i].try_lock().is_some_and(|st| st.n == STASH_ITEMS) {
            flush_stash(i, false);
        }
        let Some(mut st) = STASH[i].try_lock() else { continue };
        if st.n == STASH_ITEMS {
            continue;
        }
        let n = st.n;
        st.items[n] = d.take();
        st.n += 1;
        let now_full = st.n == STASH_ITEMS;
        drop(st);
        let first = STASHED.fetch_add(1, Relaxed) == 0;
        HANDOVER.inc();
        // As `after_push`: the reaper hands the stashes over in the background, woken where the scheduler may be;
        // also on every stash that became full (a drain may have left others behind, held at the time).
        if (first || now_full) && ksync::preempt_count() == 0 && !hal::percpu::in_hard_irq() {
            kick();
        }
        return Ok(());
    }
    match d {
        Some(d) => Err(d),
        None => Ok(()),
    }
}

/// Where reclaim's items go when every stash is full or held (`spill`).
static OVERFLOW: overflow::Pool<1024> = overflow::Pool::new();
klog::counter!(SPILLED, "mm.grace.reclaim_overflow");

/// A stamped item that found no stash into the overflow pool, without a lock. Err: the pool is full too.
fn spill(d: Deferred) -> Result<(), Deferred> {
    let start = hal::percpu::this_cpu() * (OVERFLOW.len_max() / STASHES);
    OVERFLOW.put(d, start)?;
    SPILLED.inc();
    if ksync::preempt_count() == 0 && !hal::percpu::in_hard_irq() {
        kick();
    }
    Ok(())
}

/// The overflow pool into the queue, as `flush_stash` (stamps kept; without a block the rest stays).
fn flush_overflow(wait_lock: bool) {
    let mut fresh: Option<Box<Block>> = None;
    let mut i = 0;
    while i < OVERFLOW.len_max() && !OVERFLOW.is_empty() {
        let Some(mut q) = (if wait_lock { Some(QUEUE.lock()) } else { QUEUE.try_lock() }) else { break };
        let mut need_block = false;
        while i < OVERFLOW.len_max() {
            if OVERFLOW.is_full(i) {
                if q.room(&mut fresh).is_err() {
                    need_block = true;
                    break;
                }
                if let Some(d) = OVERFLOW.take(i) {
                    q.push_back(d);
                }
            }
            i += 1;
        }
        if q.spare.is_none() {
            q.spare = fresh.take();
        }
        drop(q);
        if !need_block {
            break;
        }
        match Block::try_new() {
            Some(b) => fresh = Some(b),
            None => break,
        }
    }
    drop(fresh);
}

/// Moves every stash and the overflow pool into the queue (stashes held elsewhere stay for the next drain).
/// `wait_lock`: the queue's lock may be waited for (as `drain_with`).
fn flush_stashes(wait_lock: bool) {
    if !OVERFLOW.is_empty() {
        flush_overflow(wait_lock);
    }
    if STASHED.load(Relaxed) == 0 {
        return;
    }
    for i in 0..STASHES {
        flush_stash(i, wait_lock);
    }
}

/// Stash `i` into the queue with its stamps, in one hold of `mm.grace` (a block the queue lacks is made with
/// that lock released; without memory for one the items stay stashed).
fn flush_stash(i: usize, wait_lock: bool) {
    let Some(mut st) = STASH[i].try_lock() else { return };
    let mut fresh: Option<Box<Block>> = None;
    while st.n > 0 {
        let q = if wait_lock { Some(QUEUE.lock()) } else { QUEUE.try_lock() };
        let Some(mut q) = q else { break };
        let mut moved = 0;
        while st.n > 0 && q.room(&mut fresh).is_ok() {
            st.n -= 1;
            let n = st.n;
            if let Some(d) = st.items[n].take() {
                q.push_back(d);
                moved += 1;
            }
        }
        if q.spare.is_none() {
            q.spare = fresh.take();
        }
        drop(q);
        STASHED.fetch_sub(moved, Relaxed);
        if st.n == 0 {
            break;
        }
        match Block::try_new() {
            Some(b) => fresh = Some(b),
            None => break,
        }
    }
    drop(st);
    drop(fresh);
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

/// `defer_arc` that never waits (reclaim, inside an allocation).
pub fn defer_arc_nowait<T: Send + Sync + 'static>(a: Arc<T>, bytes: u64) {
    push(Deferred { cookie: 0, ptr: Arc::into_raw(a) as *mut (), drop: drop_arc::<T>, bytes, held: arc_held::<T>() }, false);
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
    flush_stashes(wait_lock);
    // Two advances pass what was deferred in the epoch before (when no section of it is open, both succeed).
    if try_advance() {
        try_advance();
    }
    let mut freed = 0u64;
    loop {
        // Up to a batch of items per hold of the lock (one each made every CPU's drain take it thousands of times
        // a second under reclaim), dropped after it is released.
        let mut got: [Option<Deferred>; DRAIN_BATCH] = [const { None }; DRAIN_BATCH];
        let mut empty: [Option<Box<Block>>; 2] = [None, None];
        let mut n = 0;
        {
            let q = if wait_lock { Some(QUEUE.lock()) } else { QUEUE.try_lock() };
            let Some(mut q) = q else { break };
            // Stamps that are not passed end the batch: the items behind are younger (a stash's item may be
            // older than one before it, and only waits a little longer).
            while n < DRAIN_BATCH && empty[1].is_none() {
                let (d, e) = match q.front_cookie() {
                    Some(c) if passed(c) => q.pop_front(),
                    _ => break,
                };
                if e.is_some() {
                    let slot = usize::from(empty[0].is_some());
                    empty[slot] = e;
                }
                got[n] = d;
                n += 1;
            }
        }
        drop(empty);
        if n == 0 {
            break;
        }
        for d in got.iter_mut().take(n).filter_map(Option::take) {
            count_out(&d);
            freed += d.bytes;
            // SAFETY: popped, so owned here; its grace period passed (checked under the queue lock).
            unsafe { d.release() };
        }
    }
    DRAINED.fetch_add(freed, Relaxed);
    freed
}

/// Items one drain pops per hold of `mm.grace`.
const DRAIN_BATCH: usize = 32;

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
    // Stashed items join the queue first: the grace period waited for below then covers them too.
    flush_stashes(ksync::preempt_count() == 0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    unsafe fn nothing(_: *mut ()) {}

    fn item(cookie: u64) -> Deferred {
        Deferred { cookie, ptr: core::ptr::null_mut(), drop: nothing, bytes: 0, held: 0 }
    }

    fn blocks(q: &Queue) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        let mut p = q.head;
        // SAFETY: the test's own queue; the chain is not changed while it is walked.
        while let Some(x) = unsafe { p.as_ref() } {
            v.push((x.items.len(), x.items.capacity()));
            p = x.next;
        }
        v
    }

    /// `n` items in, each block made outside (as `enqueue` does); returns the blocks made.
    fn fill(q: &mut Queue, first: u64, n: u64) -> usize {
        let mut made = 0;
        for c in first..first + n {
            let mut fresh = None;
            if q.room(&mut fresh).is_err() {
                fresh = Block::try_new();
                made += 1;
                assert_eq!(q.room(&mut fresh), Ok(()), "a block at hand always links");
                assert!(fresh.is_none());
            }
            q.push_back(item(c));
        }
        made
    }

    /// Blocks of one page, in order; none of them grows; an empty queue keeps one block and frees the rest.
    #[test]
    fn the_queue_takes_a_page_at_a_time_in_order() {
        const _: () = assert!(BLOCK * core::mem::size_of::<Deferred>() <= crate::PAGE as usize && BLOCK > 1);
        let mut q = Queue::new();
        assert!(q.is_empty() && q.front_cookie().is_none());
        let n = BLOCK as u64 * 3 + 1;
        assert_eq!(fill(&mut q, 0, n), 4);
        assert_eq!(blocks(&q).len(), 4);
        assert!(blocks(&q).iter().all(|&(_, cap)| cap == BLOCK), "a block never grows");
        let mut freed = 0;
        for c in 0..n {
            assert_eq!(q.front_cookie(), Some(c));
            let (d, empty) = q.pop_front();
            assert_eq!(d.map(|d| d.cookie), Some(c));
            freed += usize::from(empty.is_some());
        }
        assert!(q.is_empty() && q.front_cookie().is_none() && q.tail.is_null());
        assert!(q.pop_front().0.is_none());
        assert_eq!(freed, 3, "one of the four stays as the spare");
        let spare = q.spare.as_ref().map(|b| b.items.as_slices().0.as_ptr());
        assert!(spare.is_some());
        assert_eq!(q.room(&mut None), Ok(()), "the spare serves without an allocation");
        q.push_back(item(1));
        // SAFETY: the test's own queue, not changed while it is read.
        assert_eq!(unsafe { q.head.as_ref() }.map(|b| b.items.as_slices().0.as_ptr()), spare, "from empty to one item allocates nothing");
    }

    /// ATOMIC-3: the list of blocks doubled under `mm.grace` (16 KiB at 512 blocks). Chained, a queue of 600 blocks
    /// asks for nothing but the 600 blocks, all made outside, and keeps its order while it is filled and drained
    /// at once (the tail moves on while the head is popped, also through an empty queue).
    #[test]
    fn a_long_queue_needs_no_list_of_its_blocks() {
        let mut q = Queue::new();
        let n = BLOCK as u64 * 600;
        assert_eq!(fill(&mut q, 0, n), 600);
        assert!(blocks(&q).iter().all(|&(len, cap)| len == BLOCK && cap == BLOCK));
        let (mut next_in, mut next_out) = (n, 0u64);
        for round in 0..2000u64 {
            for _ in 0..(round % 7) * 50 {
                let (d, _) = q.pop_front();
                let Some(d) = d else { break };
                assert_eq!(d.cookie, next_out);
                next_out += 1;
            }
            let k = (round % 5) * 40;
            fill(&mut q, next_in, k);
            next_in += k;
        }
        while let (Some(d), _) = q.pop_front() {
            assert_eq!(d.cookie, next_out);
            next_out += 1;
        }
        assert_eq!(next_out, next_in, "every item came out once, in order");
        assert!(q.is_empty() && q.tail.is_null() && blocks(&q).is_empty());
        fill(&mut q, 0, 1);
        assert_eq!(blocks(&q), [(1, BLOCK)], "an emptied queue links its spare again");
    }
}
