//! Locks, wait queues and completions (owner: SC, trusted base; contracts `SpinLock`, `IrqSpinLock`, `Mutex`,
//! `RwSpinLock`, `WaitQueue`, `Completion`, `Once`, `OnceRef`, `SchedHooks`; spec/core.md §9.2, R5).
//!
//! The scheduler plugs in through `SchedHooks` (dependency inversion: `sched` depends on `ksync`, not the other
//! way round). Before a scheduler is installed every wait spins, which is what early boot needs.
//!
//! Rules (CONTRACTS.md §4 R5): `SpinLock` for short sections, never across a wait; `IrqSpinLock` when an
//! interrupt handler takes the same lock; `Mutex` (or a `WaitQueue`) for anything that may wait. B4: a thread
//! of class KernelRt/Interactive never waits for user space or a disk without a deadline (`_timeout` variants).
//!
//! SC-2:
//! - Every lock has a **class** = its name (`named`); `order.rs` is the desk's lock order. In debug builds the
//!   **checker** (`lockdep`) checks every acquisition against the locks the thread holds and records
//!   violations (never a panic); the scheduler reports new ones as `klog` events (`ksync lock.violation`) and
//!   ktests fail on unexpected ones.
//! - **Statistics** per class in every build (`stats`): taken, contended, wait and hold times; the `locks` MCP
//!   tool shows them (`stats::report`).
//! - SMP: lock words use Acquire/Release; the bookkeeping is per CPU (`cpu`) and per thread, lock-free and
//!   allocation-free on the lock path. CP-4 calls `prepare_cpus(n)` before the APs take locks.
//!
//! Interrupt rule (SC-3): a hard-IRQ handler may **wake** threads: `WaitQueue::wake_one`/`wake_all`,
//! `Completion::complete` and the scheduler's wake hook are IRQ-safe (their locks are `IrqSpinLock`s under which
//! nothing allocates, and the scheduler queues an interrupt-time wake-up on a lock-free per-CPU list). A handler
//! still takes no other lock than an `IrqSpinLock` (the checker reports others as `irq_unsafe`), never a `Mutex`,
//! never waits, never allocates, and never emits `klog` events. Locks a handler takes are checked against a
//! per-CPU interrupt held list, not against the interrupted thread's.
//!
//! Preemption (SC-3): every spin lock counts in this CPU's `preempt_count` while held; `PreemptGuard` does the
//! same for per-CPU work without a lock. The scheduler's interrupt-exit hook preempts only at count 0.
#![cfg_attr(target_os = "none", no_std)]
#![allow(clippy::new_without_default)]

extern crate alloc;

mod class;
mod cpu;
pub mod lockdep;
mod mutex;
pub mod order;
mod raw;
mod spin;
pub mod stats;
pub mod user_word;
mod wait;

pub use class::LockClass;
pub use cpu::{IrqState, PreemptGuard, heap_ready, irq_lock_depth, preempt_count, prepare_cpus, put_irq_state, take_irq_state};
pub use lockdep::{LockKind, Violation, ViolationKind};
pub use mutex::{Mutex, MutexGuard};
pub use spin::{IrqSpinLock, IrqSpinLockGuard, ReadGuard, RwSpinLock, SpinLock, SpinLockGuard, WriteGuard};
pub use raw::set_slow_wait_hook;
pub use stats::set_tsc_hz;
pub use wait::{Completion, WaitQueue, clockless_timeouts};

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Opaque thread identity the scheduler hands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadRef(pub u64);

pub trait SchedHooks: Sync {
    /// The calling thread.
    fn current(&self) -> ThreadRef;
    /// Marks the calling thread as about to sleep. A `wake` after this and before `block` makes `block`
    /// return at once (no lost wake-ups).
    fn prepare_to_block(&self);
    /// Sleeps until woken (or until `deadline_ns` on the monotonic clock, if given). May return spuriously.
    fn block(&self, deadline_ns: Option<u64>);
    fn wake(&self, t: ThreadRef);
    fn yield_now(&self);
    /// Monotonic nanoseconds.
    fn now_ns(&self) -> u64;
    /// SC-7b priority passing (B4). The calling thread is about to sleep for the `Mutex` at `lock`; `owner` is
    /// the mutex's owner word (`u64::MAX`: none), read by the scheduler under its priority-passing lock, so a
    /// holder that changes at the same moment is either seen there or reports itself with `pi_acquired`.
    /// Returns whether the waiter was recorded (then `pi_acquired(lock, true)` or `pi_gave_up` follows).
    fn pi_wait(&self, _lock: usize, _owner: &core::sync::atomic::AtomicU64) -> bool {
        false
    }
    /// The calling thread took the mutex at `lock` while recorded waiters exist (`waited`: it was one of them).
    fn pi_acquired(&self, _lock: usize, _waited: bool) {}
    /// A recorded waiter stopped waiting without the mutex (`lock_timeout` expired).
    fn pi_gave_up(&self, _lock: usize) {}
    /// The calling thread released the mutex at `lock` while recorded waiters exist.
    fn pi_released(&self, _lock: usize) {}
    /// How urgent `t` is (lower first; `WaitQueue::wake_best`). Asked by `t` itself just before it queues on a
    /// `WaitQueue` (no queue lock held). The default ranks all equal (FIFO).
    fn rank(&self, _t: ThreadRef) -> u8 {
        0
    }
    /// INT-W7 (MM-tlb2 CR 1 to SC): optimistic spinning for a held `Mutex` (linux-6.6/kernel/locking/mutex.c:441
    /// `mutex_spin_on_owner`, :517 `mutex_optimistic_spin`). Spins while the thread in `owner` runs on a CPU,
    /// calling `take` (one try to acquire) between checks; stops when the owner sleeps or is off its CPU, the
    /// caller is asked to reschedule, or `deadline_ns` passes (B4). Returns whether `take` succeeded. The
    /// default never spins (the caller then sleeps).
    fn spin_on_owner(&self, _owner: &core::sync::atomic::AtomicU64, _deadline_ns: u64, _take: &dyn Fn() -> bool) -> bool {
        false
    }
}

static HOOKS_SET: AtomicBool = AtomicBool::new(false);
static HOOKS: OnceRef<dyn SchedHooks> = OnceRef::new();

/// Installs the scheduler (once, by `sched` at init).
pub fn set_sched_hooks(h: &'static dyn SchedHooks) {
    if HOOKS.set(h) {
        HOOKS_SET.store(true, Ordering::Release);
    }
}

pub(crate) fn hooks() -> Option<&'static dyn SchedHooks> {
    if HOOKS_SET.load(Ordering::Acquire) { HOOKS.get() } else { None }
}

/// Monotonic time if a scheduler/clock is installed.
pub fn now_ns() -> Option<u64> {
    hooks().map(|h| h.now_ns())
}

/// Monotonic time for timeouts: the scheduler's clock, else the TSC once its frequency is known (early boot),
/// else (host) the process clock. None: no clock at all (a timeout then never expires).
pub(crate) fn clock_ns() -> Option<u64> {
    if let Some(n) = now_ns() {
        return Some(n);
    }
    #[cfg(target_os = "none")]
    {
        stats::ticks_to_ns(hal::cpu::rdtsc())
    }
    #[cfg(not(target_os = "none"))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        Some(START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64)
    }
}

/// Gives the CPU to another thread if a scheduler exists, else spins once.
pub fn yield_now() {
    match hooks() {
        Some(h) => h.yield_now(),
        None => core::hint::spin_loop(),
    }
}

/// The calling thread's id as `Mutex::owner` and the checker report it: in the kernel the scheduler's id
/// (`hal::percpu::current`, 0 before the scheduler), on the host a per-thread number.
pub fn thread_id() -> u64 {
    #[cfg(target_os = "none")]
    {
        hal::percpu::current()
    }
    #[cfg(not(target_os = "none"))]
    {
        use core::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        std::thread_local! {
            static ID: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
        }
        ID.try_with(|i| *i).unwrap_or(0)
    }
}

/// A `&'static T` set once.
pub struct OnceRef<T: ?Sized + 'static> {
    state: AtomicU8,
    val: UnsafeCell<Option<&'static T>>,
}
// SAFETY: written once under the state handshake (0 -> 1 by one winner, 2 published with Release), then only
// read; the referent is shared across CPUs, hence `T: Sync`.
unsafe impl<T: ?Sized + Sync> Sync for OnceRef<T> {}

impl<T: ?Sized> OnceRef<T> {
    pub const fn new() -> Self {
        OnceRef { state: AtomicU8::new(0), val: UnsafeCell::new(None) }
    }
    /// Returns false if it was already set (or is being set).
    pub fn set(&self, v: &'static T) -> bool {
        if self.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        // SAFETY: we won the 0->1 transition; nobody reads before state == 2.
        unsafe { *self.val.get() = Some(v) };
        self.state.store(2, Ordering::Release);
        true
    }
    pub fn get(&self) -> Option<&'static T> {
        if self.state.load(Ordering::Acquire) == 2 {
            // SAFETY: published (Acquire pairs with set's Release), never written again.
            unsafe { *self.val.get() }
        } else {
            None
        }
    }
}

/// A value initialised once on first use. A second caller during the initialisation spins until it is done
/// (initialisers are short; they must not take a lock the caller of `get_or_init` may hold).
pub struct Once<T> {
    state: AtomicU8,
    val: UnsafeCell<Option<T>>,
}
// SAFETY: as OnceRef; T must be Send (initialised on one CPU, used on others) + Sync (shared) to be shared.
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Once { state: AtomicU8::new(0), val: UnsafeCell::new(None) }
    }
    pub fn get_or_init(&self, f: impl FnOnce() -> T) -> &T {
        if self.state.load(Ordering::Acquire) != 2 {
            if self.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let v = f();
                // SAFETY: exclusive by the 0->1 transition; readers wait for 2.
                unsafe { *self.val.get() = Some(v) };
                self.state.store(2, Ordering::Release);
            } else {
                while self.state.load(Ordering::Acquire) != 2 {
                    core::hint::spin_loop();
                }
            }
        }
        // SAFETY: state 2 means Some and immutable from now on.
        unsafe { (*self.val.get()).as_ref().unwrap_unchecked() }
    }
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == 2 {
            // SAFETY: as above.
            unsafe { (*self.val.get()).as_ref() }
        } else {
            None
        }
    }
    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == 2
    }
}

#[cfg(test)]
mod tests;
