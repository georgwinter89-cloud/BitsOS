//! Wait queues and completions.
//!
//! No lost wake-ups: a waiter marks itself as about to sleep (`SchedHooks::prepare_to_block`), puts itself on
//! the queue under the queue lock, checks the condition once more and only then blocks; a waker changes the
//! condition first and then takes the queue lock to find waiters. Whichever takes the queue lock second sees
//! the other's work (the lock's Release/Acquire orders the condition's store before the waiter's re-check),
//! and a wake that lands between `prepare_to_block` and `block` makes `block` return at once (scheduler rule).
//! Interrupts may be on throughout.
//!
//! SC-3: `wake_one`, `wake_all` (and `Completion::complete`) may be called from a hard-interrupt handler. The
//! queue lock is an `IrqSpinLock`, nothing allocates or frees memory under it, and the wakers only pop entries
//! (the queue's memory is freed by thread-context code alone).
//!
//! A waiter that a waker took off the queue but that leaves without its condition (its deadline passed)
//! passes the wake on to the next waiter, so a `wake_one` meant for waiters with other conditions on the same
//! queue is not swallowed (with one shared condition the final re-check already uses it).
//!
//! A waker with nobody queued skips the queue lock (`queued`, the hot path of every `Mutex` release). That
//! stays free of lost wake-ups by two SeqCst fences: the waiter fences between enqueueing and re-checking, the
//! waker between changing the condition and reading `queued`; one of them always sees the other's write.
//!
//! Waiting never allocates inside the queue lock: the queue grows outside it first, and a waiter that cannot
//! get room (no memory) yields and polls instead of sleeping, so it never panics on a full heap.
//!
//! Timeouts: by the scheduler's clock, else the TSC (`set_tsc_hz`). With neither (very early boot) a timed wait
//! is bounded by a spin count instead (`CLOCKLESS_SPINS_PER_US`): longer than asked on a fast CPU, but bounded
//! (B4); `clockless_timeouts()` counts such waits.

use crate::{ThreadRef, clock_ns, hooks};
use crate::spin::IrqSpinLock;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};

/// Spin iterations per microsecond of timeout when there is no clock. A `pause` takes ~10 to ~150 cycles, so
/// this is at least ~1 µs per µs asked on any CPU up to ~15 GHz, and at most a few dozen µs.
const CLOCKLESS_SPINS_PER_US: u64 = 100;

static CLOCKLESS: AtomicU64 = AtomicU64::new(0);

/// Timed waits that ran without any clock and were bounded by spinning (very early boot).
pub fn clockless_timeouts() -> u64 {
    CLOCKLESS.load(Ordering::Relaxed)
}

#[cfg(test)]
std::thread_local! {
    /// Host tests: pretend there is no clock on this thread.
    pub(crate) static NO_CLOCK: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

fn wait_clock() -> Option<u64> {
    #[cfg(test)]
    if NO_CLOCK.with(|c| c.get()) {
        return None;
    }
    clock_ns()
}

/// When a timed wait ends.
#[derive(Clone, Copy)]
enum Limit {
    None,
    At(u64),
    /// No clock: this many more polls.
    Spins(u64),
}

pub struct WaitQueue {
    /// The sleepers with their rank as they queued (`SchedHooks::rank`, for `wake_best`).
    waiters: IrqSpinLock<VecDeque<(ThreadRef, u8)>>,
    /// `waiters.len()`, kept under the lock; read without it by wakers (fast path).
    queued: AtomicUsize,
}

impl WaitQueue {
    pub const fn new() -> Self {
        WaitQueue { waiters: IrqSpinLock::named("waitqueue", VecDeque::new()), queued: AtomicUsize::new(0) }
    }

    /// Sleeps until `cond()` is true. `cond` is called with no lock held and must be cheap. B4: KernelRt and
    /// Interactive threads use `wait_until_timeout` when the condition depends on a program or a disk.
    pub fn wait_until(&self, mut cond: impl FnMut() -> bool) {
        self.wait_inner(&mut cond, Limit::None);
    }

    /// As `wait_until`, but gives up after `timeout_ns`. Returns whether `cond()` became true.
    pub fn wait_until_timeout(&self, mut cond: impl FnMut() -> bool, timeout_ns: u64) -> bool {
        let limit = match wait_clock() {
            Some(n) => Limit::At(n.saturating_add(timeout_ns)),
            None => {
                CLOCKLESS.fetch_add(1, Ordering::Relaxed);
                Limit::Spins((timeout_ns / 1000).saturating_mul(CLOCKLESS_SPINS_PER_US).max(1))
            }
        };
        self.wait_inner(&mut cond, limit)
    }

    fn wait_inner(&self, cond: &mut dyn FnMut() -> bool, mut limit: Limit) -> bool {
        // Set when a waker took us off the queue (we hold a wake we have not used yet).
        let mut woken = false;
        loop {
            if cond() {
                return true;
            }
            let expired = match &mut limit {
                Limit::None => false,
                Limit::At(d) => wait_clock().is_some_and(|now| now >= *d),
                Limit::Spins(n) => {
                    *n = n.saturating_sub(1);
                    *n == 0
                }
            };
            if expired {
                let ok = cond();
                if !ok && woken {
                    self.wake_one();
                }
                return ok;
            }
            let deadline = match limit {
                Limit::At(d) => Some(d),
                _ => None,
            };
            match hooks() {
                None => core::hint::spin_loop(),
                Some(_) if matches!(limit, Limit::Spins(_)) => core::hint::spin_loop(),
                Some(h) => {
                    crate::lockdep::might_sleep();
                    let me = h.current();
                    if !self.reserve() {
                        // No memory for a queue slot: poll instead of sleeping (no wake could find us).
                        h.yield_now();
                        continue;
                    }
                    // Ranked before any queue lock is taken: `wake_best` then reads no thread table.
                    let rank = h.rank(me);
                    h.prepare_to_block();
                    if !self.enqueue(me, rank) {
                        h.yield_now();
                        continue;
                    }
                    // Pairs with the waker's fence (module doc): our `queued` store before our re-check.
                    fence(Ordering::SeqCst);
                    if cond() {
                        // `prepare_to_block` without `block`: the scheduler keeps a thread in that state
                        // running (sched: Blocking counts as running), so returning here is fine.
                        self.remove(me);
                        return true;
                    }
                    h.block(deadline);
                    woken = !self.remove(me);
                }
            }
        }
    }

    /// Makes room for one more waiter (allocating outside the queue lock). False: no memory.
    fn reserve(&self) -> bool {
        let need = {
            let q = self.waiters.lock();
            if q.len() < q.capacity() {
                return true;
            }
            q.capacity().max(4) * 2
        };
        let mut fresh: VecDeque<(ThreadRef, u8)> = VecDeque::new();
        if fresh.try_reserve(need).is_err() {
            return false;
        }
        let mut q = self.waiters.lock();
        if q.len() < q.capacity() {
            return true;
        }
        fresh.extend(q.drain(..));
        let old = core::mem::replace(&mut *q, fresh);
        drop(q);
        drop(old);
        true
    }

    /// Puts `me` on the queue unless it is there; false if there was no room (`reserve` raced with others).
    fn enqueue(&self, me: ThreadRef, rank: u8) -> bool {
        let mut q = self.waiters.lock();
        if !q.iter().any(|w| w.0 == me) {
            if q.len() == q.capacity() {
                return false;
            }
            q.push_back((me, rank));
            self.queued.store(q.len(), Ordering::Relaxed);
        }
        true
    }

    /// Takes `t` off the queue; false if it was not on it any more (a waker took it).
    fn remove(&self, t: ThreadRef) -> bool {
        let mut q = self.waiters.lock();
        let before = q.len();
        q.retain(|w| w.0 != t);
        self.queued.store(q.len(), Ordering::Relaxed);
        q.len() != before
    }

    /// True if no waiter can be queued; the caller changed the condition before (module doc).
    fn nobody_waits(&self) -> bool {
        fence(Ordering::SeqCst);
        self.queued.load(Ordering::Relaxed) == 0
    }

    pub fn wake_one(&self) {
        if self.nobody_waits() {
            return;
        }
        let t = {
            let mut q = self.waiters.lock();
            let t = q.pop_front().map(|w| w.0);
            self.queued.store(q.len(), Ordering::Relaxed);
            t
        };
        if let (Some(t), Some(h)) = (t, hooks()) {
            h.wake(t);
        }
    }

    /// As `wake_one`, but wakes the most urgent waiter (`SchedHooks::rank`, the earliest among equals): a mutex
    /// release wakes an Interactive waiter before a Normal one (SC-7b). IRQ-safe as `wake_one`: the ranks were
    /// taken when the waiters queued, so the scan under the queue lock only compares bytes (a class change while
    /// asleep is seen at the next sleep; the order is a preference, the woken thread re-checks its condition).
    pub fn wake_best(&self) {
        if self.nobody_waits() {
            return;
        }
        let t = {
            let mut q = self.waiters.lock();
            let i = q.iter().enumerate().min_by_key(|&(i, w)| (w.1, i)).map_or(0, |(i, _)| i);
            let t = q.remove(i).map(|w| w.0);
            self.queued.store(q.len(), Ordering::Relaxed);
            t
        };
        if let (Some(t), Some(h)) = (t, hooks()) {
            h.wake(t);
        }
    }

    /// Wakes every thread queued now. Pops them one by one (the queue keeps its memory, so this never frees
    /// anything and is safe in an interrupt handler); a waiter that queues itself meanwhile lands behind them.
    pub fn wake_all(&self) {
        if self.nobody_waits() {
            return;
        }
        let n = self.waiters.lock().len();
        for _ in 0..n {
            let t = {
                let mut q = self.waiters.lock();
                let t = q.pop_front().map(|w| w.0);
                self.queued.store(q.len(), Ordering::Relaxed);
                t
            };
            match (t, hooks()) {
                (Some(t), Some(h)) => h.wake(t),
                (Some(_), None) => {}
                (None, _) => break,
            }
        }
    }

    pub fn waiter_count(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A one-shot event (or counting, with `complete` called several times). `complete` wakes every waiter;
/// `wait` does not consume (use `reset` to re-arm).
pub struct Completion {
    done: AtomicUsize,
    wq: WaitQueue,
}

impl Completion {
    pub const fn new() -> Self {
        Completion { done: AtomicUsize::new(0), wq: WaitQueue::new() }
    }
    pub fn complete(&self) {
        self.done.fetch_add(1, Ordering::AcqRel);
        self.wq.wake_all();
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire) > 0
    }
    /// How often `complete` was called since the last `reset`.
    pub fn count(&self) -> usize {
        self.done.load(Ordering::Acquire)
    }
    pub fn wait(&self) {
        self.wq.wait_until(|| self.is_done());
    }
    pub fn wait_timeout(&self, timeout_ns: u64) -> bool {
        self.wq.wait_until_timeout(|| self.is_done(), timeout_ns)
    }
    pub fn reset(&self) {
        self.done.store(0, Ordering::Release);
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}
