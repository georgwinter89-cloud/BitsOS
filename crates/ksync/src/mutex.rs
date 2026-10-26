//! The sleeping mutex: spins briefly, then sleeps on a wait queue (R5: anything that may wait uses this). It
//! records its owner (the thread id) for the `locks` tool, the checker and debugging; the guard is `!Send`, so
//! the owner is also the one that releases it. B4: KernelRt/Interactive threads waiting for a lock a program
//! or a disk path may hold use `lock_timeout`.
//!
//! SC-7b priority passing (B4): a thread that goes to sleep for a held mutex records itself with the scheduler
//! (`SchedHooks::pi_wait`), which lends the waiter's class to the holder (and along a chain of holders that wait
//! themselves) while it holds. `pi` counts the recorded waiters: only while it is not 0 do acquiring and
//! releasing threads report to the scheduler (`pi_acquired`, `pi_released`), so the uncontended path costs one
//! SeqCst load. Owner word and `pi` are SeqCst on both sides: a waiter that reads the owner after counting itself
//! either sees a new holder's store, or that holder sees the count and reports itself. A release wakes the most
//! urgent sleeper (`WaitQueue::wake_best`).

use crate::lockdep::LockKind;
use crate::raw::{Acq, Meta, wait_since, wait_start};
use crate::spin::NotSend;
use crate::wait::WaitQueue;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Spins before sleeping (the holder is likely to be done soon; a sleep costs two switches).
const SPINS_BEFORE_SLEEP: u32 = 64;
const NO_OWNER: u64 = u64::MAX;
/// INT-W7: the longest a waiter spins on a running owner before it sleeps (B4 bound; Linux has only
/// need_resched). Short holds (an address space's lock handed between a program's threads) end well within it.
const OWNER_SPIN_NS: u64 = 20_000;

pub struct Mutex<T: ?Sized> {
    meta: Meta,
    locked: AtomicBool,
    owner: AtomicU64,
    /// Waiters recorded for priority passing (SC-7b).
    pi: AtomicU32,
    wq: WaitQueue,
    val: UnsafeCell<T>,
}

// SAFETY: `locked` serialises access to `val` (Acquire on take, Release on release); the holder may be any
// thread on any CPU, hence `T: Send`.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
// SAFETY: moving the mutex moves the `T`.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

pub struct MutexGuard<'a, T: ?Sized> {
    m: &'a Mutex<T>,
    acq: Acq,
    _not_send: NotSend,
}

// SAFETY: a shared guard only hands out `&T`.
unsafe impl<T: ?Sized + Sync> Sync for MutexGuard<'_, T> {}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Self::named("", v)
    }
    pub const fn named(name: &'static str, v: T) -> Self {
        Mutex {
            meta: Meta::new(name),
            locked: AtomicBool::new(false),
            owner: AtomicU64::new(NO_OWNER),
            pi: AtomicU32::new(0),
            wq: WaitQueue::new(),
            val: UnsafeCell::new(v),
        }
    }
    pub fn into_inner(self) -> T {
        self.val.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn name(&self) -> &'static str {
        self.meta.name()
    }
    fn addr(&self) -> usize {
        self as *const Self as *const u8 as usize
    }
    fn try_acquire(&self) -> bool {
        self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }
    /// `reported`: the caller already told the scheduler (`pi_acquired` as a recorded waiter).
    fn guard_from(&self, c: &'static crate::class::LockClass, wait: Option<u64>, reported: bool) -> MutexGuard<'_, T> {
        self.owner.store(crate::thread_id(), Ordering::SeqCst);
        if !reported
            && self.pi.load(Ordering::SeqCst) != 0
            && let Some(h) = crate::hooks()
        {
            h.pi_acquired(self.addr(), false);
        }
        let acq = self.meta.acquired(c, self.addr(), LockKind::Mutex, wait);
        MutexGuard { m: self, acq, _not_send: PhantomData }
    }
    fn guard(&self, c: &'static crate::class::LockClass, wait: Option<u64>) -> MutexGuard<'_, T> {
        self.guard_from(c, wait, false)
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        let c = self.meta.before(self.addr(), LockKind::Mutex);
        if self.try_acquire() {
            return self.guard(c, None);
        }
        let t0 = wait_start();
        for _ in 0..SPINS_BEFORE_SLEEP {
            core::hint::spin_loop();
            if !self.locked.load(Ordering::Relaxed) && self.try_acquire() {
                return self.guard(c, Some(wait_since(t0)));
            }
        }
        // While the holder runs on another CPU it will likely release soon: spin on it instead of a sleep and a
        // wake (two switches and a reschedule IPI per hand-over), bounded by OWNER_SPIN_NS and need_resched.
        if let Some(h) = crate::hooks()
            && self.owner.load(Ordering::Relaxed) != crate::thread_id()
            && h.spin_on_owner(&self.owner, h.now_ns().saturating_add(OWNER_SPIN_NS), &|| !self.locked.load(Ordering::Relaxed) && self.try_acquire())
        {
            return self.guard(c, Some(wait_since(t0)));
        }
        let pi = self.pi_enter();
        self.wq.wait_until(|| self.try_acquire());
        self.pi_acquired(pi);
        self.guard_from(c, Some(wait_since(t0)), pi)
    }

    /// Records the caller as a waiter for priority passing (false: no scheduler, or it could not).
    fn pi_enter(&self) -> bool {
        let Some(h) = crate::hooks() else { return false };
        self.pi.fetch_add(1, Ordering::SeqCst);
        if h.pi_wait(self.addr(), &self.owner) {
            return true;
        }
        self.pi.fetch_sub(1, Ordering::SeqCst);
        false
    }
    /// A recorded waiter took the lock: it becomes the owner the remaining waiters lend to. The owner word is
    /// stored first (as `guard` does) so a concurrent `pi_wait` sees it.
    fn pi_acquired(&self, pi: bool) {
        if !pi {
            return;
        }
        self.owner.store(crate::thread_id(), Ordering::SeqCst);
        if let Some(h) = crate::hooks() {
            h.pi_acquired(self.addr(), true);
        }
        self.pi.fetch_sub(1, Ordering::SeqCst);
    }

    /// As `lock`, but gives up after `timeout_ns` (None).
    pub fn lock_timeout(&self, timeout_ns: u64) -> Option<MutexGuard<'_, T>> {
        let c = self.meta.before(self.addr(), LockKind::Mutex);
        if self.try_acquire() {
            return Some(self.guard(c, None));
        }
        let t0 = wait_start();
        let pi = self.pi_enter();
        if self.wq.wait_until_timeout(|| self.try_acquire(), timeout_ns) {
            self.pi_acquired(pi);
            return Some(self.guard_from(c, Some(wait_since(t0)), pi));
        }
        if pi {
            if let Some(h) = crate::hooks() {
                h.pi_gave_up(self.addr());
            }
            self.pi.fetch_sub(1, Ordering::SeqCst);
        }
        self.meta.abandoned(c, true);
        None
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let c = self.meta.before_try();
        if !self.try_acquire() {
            self.meta.abandoned(c, false);
            return None;
        }
        Some(self.guard(c, None))
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
    /// The holder's thread id (None when free; a racing read may be one step behind).
    pub fn owner(&self) -> Option<u64> {
        if !self.is_locked() {
            return None;
        }
        match self.owner.load(Ordering::Relaxed) {
            NO_OWNER => None,
            t => Some(t),
        }
    }
    /// Threads sleeping for it now.
    pub fn waiters(&self) -> usize {
        self.wq.waiter_count()
    }
    pub fn get_mut(&mut self) -> &mut T {
        self.val.get_mut()
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard exists only while this thread holds `locked`.
        unsafe { &*self.m.val.get() }
    }
}
impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, exclusively through `&mut self`.
        unsafe { &mut *self.m.val.get() }
    }
}
impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        let m = self.m;
        m.owner.store(NO_OWNER, Ordering::SeqCst);
        m.meta.release(self.acq, m.addr(), || m.locked.store(false, Ordering::Release));
        if m.pi.load(Ordering::SeqCst) != 0 {
            if let Some(h) = crate::hooks() {
                h.pi_released(m.addr());
            }
            m.wq.wake_best();
        } else {
            m.wq.wake_one();
        }
    }
}
