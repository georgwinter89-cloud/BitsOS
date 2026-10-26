//! Spin locks: `SpinLock` (short sections, never across a wait), `IrqSpinLock` (also masks interrupts on this
//! CPU; nesting-safe in any release order), `RwSpinLock` (readers share, a waiting writer stops new readers).
//!
//! Memory ordering: taking a lock is an Acquire RMW on its word, releasing a Release store/RMW, so everything
//! written inside one critical section is visible to the next holder on any CPU. Waiting spins on plain
//! (Relaxed) loads and retries the RMW only when the word looks free (test-and-test-and-set), which keeps the
//! cache line shared while it is held. Guards are `!Send`: a lock is released by the thread that took it
//! (the held-lock list of the order checker and `Mutex`'s owner rely on that).
//!
//! SC-3: every spin lock counts in this CPU's preempt count from before it is taken until after it is
//! released (`preempt_count`), so an interrupt-exit preemption never switches away a spin-lock holder.

use crate::cpu::{IrqSave, irq_restore, irq_save, preempt_dec, preempt_inc};
use crate::lockdep::LockKind;
use crate::raw::{Acq, Meta, wait_since, wait_start};
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Makes a guard `!Send` and `!Sync`.
pub(crate) type NotSend = PhantomData<*const ()>;

pub struct SpinLock<T: ?Sized> {
    meta: Meta,
    locked: AtomicBool,
    val: UnsafeCell<T>,
}

// SAFETY: the lock word serialises every access to `val` (Acquire on take, Release on release), so sharing
// the lock between CPUs only ever gives one CPU access to `T` at a time; `T: Send` because that CPU may differ.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the `T` inside, which is fine for `T: Send`.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
    acq: Acq,
    _not_send: NotSend,
}

// SAFETY: a shared guard only hands out `&T`; sharing it is sharing `&T`, fine for `T: Sync`.
unsafe impl<T: ?Sized + Sync> Sync for SpinLockGuard<'_, T> {}

impl<T> SpinLock<T> {
    pub const fn new(v: T) -> Self {
        SpinLock { meta: Meta::new(""), locked: AtomicBool::new(false), val: UnsafeCell::new(v) }
    }
    /// A lock of class `name` (its place in the lock order and its statistics, `order.rs`).
    pub const fn named(name: &'static str, v: T) -> Self {
        SpinLock { meta: Meta::new(name), locked: AtomicBool::new(false), val: UnsafeCell::new(v) }
    }
    pub fn into_inner(self) -> T {
        self.val.into_inner()
    }
}

impl<T: ?Sized> SpinLock<T> {
    pub fn name(&self) -> &'static str {
        self.meta.name()
    }
    fn addr(&self) -> usize {
        self as *const Self as *const u8 as usize
    }
    fn try_word(&self) -> bool {
        self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }
    fn spin_until_taken(&self) -> Option<u64> {
        if self.try_word() {
            return None;
        }
        let t0 = wait_start();
        loop {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
            if self.try_word() {
                return Some(wait_since(t0));
            }
        }
    }
    pub(crate) fn lock_as(&self, kind: LockKind) -> SpinLockGuard<'_, T> {
        preempt_inc();
        let c = self.meta.before(self.addr(), kind);
        let wait = self.spin_until_taken();
        let acq = self.meta.acquired(c, self.addr(), kind, wait);
        SpinLockGuard { lock: self, acq, _not_send: PhantomData }
    }
    pub(crate) fn try_lock_as(&self, kind: LockKind) -> Option<SpinLockGuard<'_, T>> {
        preempt_inc();
        let c = self.meta.before_try();
        if !self.try_word() {
            self.meta.abandoned(c, false);
            preempt_dec();
            return None;
        }
        let acq = self.meta.acquired(c, self.addr(), kind, None);
        Some(SpinLockGuard { lock: self, acq, _not_send: PhantomData })
    }
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        self.lock_as(LockKind::Spin)
    }
    /// Takes the lock only if it is free. A failed try leaves the lock untouched.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.try_lock_as(LockKind::Spin)
    }
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
    pub fn get_mut(&mut self) -> &mut T {
        self.val.get_mut()
    }
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard exists only while this thread holds the lock word.
        unsafe { &*self.lock.val.get() }
    }
}
impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` makes the access exclusive.
        unsafe { &mut *self.lock.val.get() }
    }
}
impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        let l = self.lock;
        l.meta.release(self.acq, l.addr(), || l.locked.store(false, Ordering::Release));
        preempt_dec();
    }
}

// ---------------------------------------------------------------------------------------------------------

/// A spin lock that also masks interrupts on this CPU while held (state shared with interrupt handlers).
/// Nesting-safe: interrupts come back on only when the last `IrqSpinLock` of this CPU is released, whatever
/// the release order, and only if they were on before the first (also when an NMI takes one in between).
/// The depth follows the thread across a switch (`take_irq_state`/`put_irq_state`, called by `sched`).
pub struct IrqSpinLock<T: ?Sized> {
    inner: SpinLock<T>,
}

pub struct IrqSpinLockGuard<'a, T: ?Sized> {
    guard: ManuallyDrop<SpinLockGuard<'a, T>>,
    irq: IrqSave,
}

impl<T> IrqSpinLock<T> {
    pub const fn new(v: T) -> Self {
        IrqSpinLock { inner: SpinLock::new(v) }
    }
    pub const fn named(name: &'static str, v: T) -> Self {
        IrqSpinLock { inner: SpinLock::named(name, v) }
    }
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T: ?Sized> IrqSpinLock<T> {
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        let irq = irq_save();
        let guard = self.inner.lock_as(LockKind::IrqSpin);
        IrqSpinLockGuard { guard: ManuallyDrop::new(guard), irq }
    }
    /// Takes the lock only if it is free (fault and panic paths that must not wait).
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T>> {
        let irq = irq_save();
        match self.inner.try_lock_as(LockKind::IrqSpin) {
            Some(guard) => Some(IrqSpinLockGuard { guard: ManuallyDrop::new(guard), irq }),
            None => {
                irq_restore(irq);
                None
            }
        }
    }
    pub fn name(&self) -> &'static str {
        self.inner.name()
    }
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}
impl<T: ?Sized> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}
impl<T: ?Sized> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}
impl<T: ?Sized> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // Release the lock before interrupts may come back.
        // SAFETY: `guard` is dropped exactly once, here, and never used afterwards.
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        irq_restore(self.irq);
    }
}

// ---------------------------------------------------------------------------------------------------------

/// Reader-writer spin lock (short sections only). A writer that waits sets `WAITING`, which keeps new readers
/// out, so a stream of readers cannot starve it. A thread must not take the read side twice (a writer waiting
/// in between would deadlock it; the checker reports it as `Recursive`). Never take it in an interrupt
/// handler (nor with interrupts on if a handler could take it): unlike Linux's queued rwlock there is no
/// reader bypass for interrupt context, so a handler reading while a writer waits on the same CPU deadlocks.
/// A reader in interrupt context needs an IRQ-safe variant first (the checker reports any `RwSpinLock` taken in
/// a hard-interrupt handler as `irq_unsafe`).
pub struct RwSpinLock<T: ?Sized> {
    meta: Meta,
    state: AtomicUsize,
    val: UnsafeCell<T>,
}
const WRITER: usize = 1 << (usize::BITS - 1);
const WAITING: usize = 1 << (usize::BITS - 2);
const READERS: usize = WAITING - 1;

// SAFETY: readers share `&T` (needs `T: Sync`), the writer has it exclusively and may be on another CPU (needs
// `T: Send`); the state word orders them (Acquire on entry, Release on exit).
unsafe impl<T: ?Sized + Send + Sync> Sync for RwSpinLock<T> {}
// SAFETY: moving the lock moves the `T`.
unsafe impl<T: ?Sized + Send> Send for RwSpinLock<T> {}

pub struct ReadGuard<'a, T: ?Sized> {
    lock: &'a RwSpinLock<T>,
    acq: Acq,
    _not_send: NotSend,
}
pub struct WriteGuard<'a, T: ?Sized> {
    lock: &'a RwSpinLock<T>,
    acq: Acq,
    _not_send: NotSend,
}

impl<T> RwSpinLock<T> {
    pub const fn new(v: T) -> Self {
        RwSpinLock { meta: Meta::new(""), state: AtomicUsize::new(0), val: UnsafeCell::new(v) }
    }
    pub const fn named(name: &'static str, v: T) -> Self {
        RwSpinLock { meta: Meta::new(name), state: AtomicUsize::new(0), val: UnsafeCell::new(v) }
    }
    pub fn into_inner(self) -> T {
        self.val.into_inner()
    }
}

impl<T: ?Sized> RwSpinLock<T> {
    pub fn name(&self) -> &'static str {
        self.meta.name()
    }
    fn addr(&self) -> usize {
        self as *const Self as *const u8 as usize
    }
    fn try_read_word(&self) -> bool {
        let s = self.state.load(Ordering::Relaxed);
        s & (WRITER | WAITING) == 0
            && s & READERS != READERS
            && self.state.compare_exchange_weak(s, s + 1, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }
    fn try_write_word(&self) -> bool {
        let s = self.state.load(Ordering::Relaxed);
        // Free apart from a waiting mark (ours or another writer's): take it and clear the mark; any other
        // waiting writer sets it again.
        s & !WAITING == 0 && self.state.compare_exchange(s, WRITER, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }
    pub fn read(&self) -> ReadGuard<'_, T> {
        preempt_inc();
        let c = self.meta.before(self.addr(), LockKind::Read);
        let mut wait = None;
        if !self.try_read_word() {
            let t0 = wait_start();
            while !self.try_read_word() {
                core::hint::spin_loop();
            }
            wait = Some(wait_since(t0));
        }
        let acq = self.meta.acquired(c, self.addr(), LockKind::Read, wait);
        ReadGuard { lock: self, acq, _not_send: PhantomData }
    }
    pub fn write(&self) -> WriteGuard<'_, T> {
        preempt_inc();
        let c = self.meta.before(self.addr(), LockKind::Write);
        let mut wait = None;
        if !self.try_write_word() {
            let t0 = wait_start();
            loop {
                let s = self.state.load(Ordering::Relaxed);
                if s & !WAITING == 0 {
                    if self.try_write_word() {
                        break;
                    }
                } else if s & WAITING == 0 {
                    self.state.fetch_or(WAITING, Ordering::Relaxed);
                }
                core::hint::spin_loop();
            }
            wait = Some(wait_since(t0));
        }
        let acq = self.meta.acquired(c, self.addr(), LockKind::Write, wait);
        WriteGuard { lock: self, acq, _not_send: PhantomData }
    }
    pub fn try_read(&self) -> Option<ReadGuard<'_, T>> {
        preempt_inc();
        let c = self.meta.before_try();
        if !self.try_read_word() {
            self.meta.abandoned(c, false);
            preempt_dec();
            return None;
        }
        let acq = self.meta.acquired(c, self.addr(), LockKind::Read, None);
        Some(ReadGuard { lock: self, acq, _not_send: PhantomData })
    }
    pub fn try_write(&self) -> Option<WriteGuard<'_, T>> {
        preempt_inc();
        let c = self.meta.before_try();
        if !self.try_write_word() {
            self.meta.abandoned(c, false);
            preempt_dec();
            return None;
        }
        let acq = self.meta.acquired(c, self.addr(), LockKind::Write, None);
        Some(WriteGuard { lock: self, acq, _not_send: PhantomData })
    }
    /// Readers holding it now (0 when free or write-locked).
    pub fn readers(&self) -> usize {
        self.state.load(Ordering::Relaxed) & READERS
    }
    pub fn is_write_locked(&self) -> bool {
        self.state.load(Ordering::Relaxed) & WRITER != 0
    }
    pub fn get_mut(&mut self) -> &mut T {
        self.val.get_mut()
    }
}
impl<T: ?Sized> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: shared access while this reader is counted (no writer can enter).
        unsafe { &*self.lock.val.get() }
    }
}
impl<T: ?Sized> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        let l = self.lock;
        l.meta.release(self.acq, l.addr(), || {
            l.state.fetch_sub(1, Ordering::Release);
        });
        preempt_dec();
    }
}
impl<T: ?Sized> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: exclusive while WRITER is set by this guard.
        unsafe { &*self.lock.val.get() }
    }
}
impl<T: ?Sized> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: exclusive while WRITER is set by this guard, and `&mut self`.
        unsafe { &mut *self.lock.val.get() }
    }
}
impl<T: ?Sized> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        let l = self.lock;
        // Keeps a WAITING mark another writer set meanwhile.
        l.meta.release(self.acq, l.addr(), || {
            l.state.fetch_and(!WRITER, Ordering::Release);
        });
        preempt_dec();
    }
}
