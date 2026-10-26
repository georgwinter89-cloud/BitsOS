//! Futexes (SC-5; spec/core.md §9.2, V2-PLAN §2.6; Linux linux-6.6/kernel/futex/).
//!
//! - **Hash buckets** (R5): `256 x CPUs` buckets (a power of two, Linux's `futex_init`), each a spin lock over
//!   its waiters in arrival order (`table`). A key is (address space, address) for a private futex,
//!   (address space, address) of another kind for a shared one on anonymous memory, (inode, page index,
//!   offset) for a shared one on a file or shmem page (`Key`, computed by the caller's `Space`).
//! - **Waiting** follows Linux's `futex_wait_setup`: lock the bucket, read the word (no page fault allowed under
//!   the lock: `ksync::user_word` through the direct map; on a fault the lock is dropped, the page faulted in and
//!   the whole setup repeated), compare, queue, unlock, sleep. A waker changes the word first and takes the
//!   bucket lock after, so it either finds the waiter queued or the waiter sees the new value: no lost wake-up.
//! - **Waking** takes waiters off under the bucket lock, marks them woken and wakes their threads
//!   (`run::wake_thread`; the scheduler lock sits inside every subsystem lock).
//! - A waiter's own state is in its `Thread` (`ThreadState`, no allocation per wait): `woken`, and per slot the
//!   bucket its entry is in now (a requeue moves entries between buckets under both locks and updates it, so the
//!   waiter always finds and takes off its own entry: Linux's `lock_ptr` retry in `futex_unqueue`).
//! - Two buckets (requeue, wake_op) are locked lower index first; the second with `try_lock` in a loop, so the
//!   lock checker does not see two locks of one class nested (the index order makes it deadlock-free).
//! - Timeouts are absolute on the monotonic clock or on CLOCK_REALTIME (converted to a monotonic deadline at
//!   each sleep); the sleep ends by the scheduler's HrTimer (SC-4).
//! - **Interruption**: a wait also ends when `Space::interrupted` says so (the process was killed; AB-6 adds
//!   deliverable signals). `kick_thread` wakes a sleeping thread, and the waiter checks `interrupted` after it
//!   is queued and before it sleeps (both sides with a full fence), so a kill is never missed.
//! - **Robust lists and thread exit** (`robust`): `exit_robust_list` walks the list at thread exit (OWNER_DIED,
//!   wake one), `clear_child_tid` zeroes the tid word and wakes one waiter (CLONE_CHILD_CLEARTID).
//!
//! - **PI futexes** (SC-7b, `pi`): FUTEX_LOCK_PI/LOCK_PI2/TRYLOCK_PI/UNLOCK_PI with the owner's TID in the word,
//!   hand-over by the unlocker, priority passing to the owner (`sched::pi`), and the owner's exit handing each
//!   PI futex it holds to its top waiter. FUTEX_WAIT_REQUEUE_PI/CMP_REQUEUE_PI: Linux's argument checks, then
//!   ENOSYS (not built; plan/notes/sc-SC-7b.md).
//!
//! Errors are positive errno values (`uapi::errno`); the syscall layer negates them.

mod ktests;
mod ops;
pub mod pi;
mod pi_ktests;
pub mod robust;
pub mod table;

use crate::{State, Thread};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering, fence};
use ksync::user_word::{WordError, WordOp};
use ksync::{SchedHooks, SpinLock, SpinLockGuard};
pub use ops::{WaitvItem, requeue, wait_multiple, wake_op};
pub use table::{Key, KeyKind};
use uapi::errno::{EAGAIN, EFAULT, EINVAL, ENOMEM, ETIMEDOUT};

/// FUTEX_BITSET_MATCH_ANY.
pub const BITSET_MATCH_ANY: u32 = u32::MAX;
/// Robust futex word bits (include/uapi/linux/futex.h).
pub const FUTEX_WAITERS: u32 = 0x8000_0000;
pub const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
pub const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

klog::counter!(WAITS, "futex.waits");
klog::counter!(WAKES, "futex.woken");
klog::counter!(TIMEOUTS, "futex.timeouts");
klog::counter!(FAULT_RETRIES, "futex.fault_retries");
klog::counter!(REQUEUED, "futex.requeued");

/// A key and what keeps its object alive while the operation runs (a file mapping's backing for an inode key:
/// the inode address in the key cannot be reused while a reference is held).
pub struct KeyRef {
    pub key: Key,
    pub hold: Option<Box<dyn Any + Send + Sync>>,
}

impl KeyRef {
    pub fn plain(key: Key) -> KeyRef {
        KeyRef { key, hold: None }
    }
}

/// When a wait gives up (absolute).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deadline {
    /// Nanoseconds of the monotonic clock (`time::now_ns`).
    Monotonic(u64),
    /// Nanoseconds since the epoch on CLOCK_REALTIME (FUTEX_CLOCK_REALTIME, futex_waitv with CLOCK_REALTIME).
    Realtime(u64),
}

/// The calling program's memory as the futex code needs it (implemented by the syscall layer for a process,
/// and by the ktests for kernel buffers). Errors are positive errno values.
pub trait Space {
    /// The key of the word at `addr` (Linux `get_futex_key`): EINVAL when misaligned, EFAULT when it is no user
    /// address or (shared) not mapped as needed; `write` for the word `futex_wake_op` changes.
    fn key(&self, addr: u64, shared: bool, write: bool) -> Result<KeyRef, i64>;
    /// `op` on the word without a page fault (called with bucket locks held).
    fn word(&self, addr: u64, op: WordOp) -> Result<u32, WordError>;
    /// Makes the page present (and writable for `write`) with no lock held; EFAULT when it cannot be.
    fn fault_in(&self, addr: u64, write: bool) -> Result<(), i64>;
    /// `get_user`/`put_user` (may fault pages in; no lock held).
    fn read_u32(&self, addr: u64) -> Result<u32, i64>;
    fn read_u64(&self, addr: u64) -> Result<u64, i64>;
    fn write_u32(&self, addr: u64, v: u32) -> Result<(), i64>;
    /// Why a sleeping wait must end now (the errno to return), if it must.
    fn interrupted(&self) -> Option<i64>;
    /// SC-7b: the calling thread's TID as its programs see it (the owner field of a PI futex word); None for a
    /// caller that has none (the PI operations answer EINVAL).
    fn tid(&self) -> Option<u32> {
        None
    }
    /// SC-7b: the scheduler id of the live thread with TID `tid` in any process (PI futexes may be shared), if any.
    fn thread_of(&self, _tid: u32) -> Option<u64> {
        None
    }
}

/// A thread's futex state (part of `Thread`).
pub(crate) struct ThreadState {
    /// A waker took one of this thread's entries off (since the start of its current wait).
    woken: AtomicBool,
    /// The bucket each slot's entry is in (index = slot). Changed under the bucket locks by requeue.
    locs: SpinLock<Vec<u32>>,
    /// SC-7b: a PI lock was handed to this waiter by its owner's exit: it writes its TID into the word itself.
    pi_fixup: AtomicBool,
    /// SC-7b: Linux's `futex_state` (`pi::RUNNING`, `EXITING`, `DEAD`), written under `sched::pi`'s lock.
    pi_exit: AtomicU8,
}

impl ThreadState {
    pub(crate) const fn new() -> Self {
        ThreadState {
            woken: AtomicBool::new(false),
            locs: SpinLock::named("futex.thread", Vec::new()),
            pi_fixup: AtomicBool::new(false),
            pi_exit: AtomicU8::new(pi::RUNNING),
        }
    }
    fn loc(&self, slot: u32) -> u32 {
        self.locs.lock().get(slot as usize).copied().unwrap_or(u32::MAX)
    }
    fn set_loc(&self, slot: u32, bucket: u32) {
        if let Some(l) = self.locs.lock().get_mut(slot as usize) {
            *l = bucket;
        }
    }
    /// Room for `n` slots (outside any bucket lock).
    fn reserve(&self, n: usize) -> Result<(), i64> {
        if self.locs.lock().len() >= n {
            return Ok(());
        }
        let mut fresh: Vec<u32> = Vec::new();
        fresh.try_reserve_exact(n).map_err(|_| ENOMEM)?;
        fresh.resize(n, u32::MAX);
        let old = core::mem::replace(&mut *self.locs.lock(), fresh);
        drop(old);
        Ok(())
    }
}

pub(crate) type FEntry = table::Entry<Arc<Thread>>;

/// The hash table.
pub(crate) struct Table {
    buckets: Vec<SpinLock<Vec<FEntry>>>,
    mask: usize,
}

static TABLE: ksync::OnceRef<Table> = ksync::OnceRef::new();
static TABLE_INIT: SpinLock<()> = SpinLock::named("futex.init", ());

pub(crate) fn buckets() -> Result<&'static Table, i64> {
    if let Some(t) = TABLE.get() {
        return Ok(t);
    }
    let _g = TABLE_INIT.lock();
    if let Some(t) = TABLE.get() {
        return Ok(t);
    }
    let cpus = hal::percpu::cpu_count().max(crate::online_cpus()).max(1);
    let n = (256 * cpus).next_power_of_two();
    let mut v: Vec<SpinLock<Vec<FEntry>>> = Vec::new();
    if v.try_reserve_exact(n).is_err() {
        klog::event!(Error, "futex", "table.no_memory", buckets = n; "no memory for the futex hash table (ENOMEM; retried at the next call)");
        return Err(ENOMEM);
    }
    for _ in 0..n {
        v.push(SpinLock::named("futex.bucket", Vec::new()));
    }
    let t: &'static Table = Box::leak(Box::new(Table { buckets: v, mask: n - 1 }));
    TABLE.set(t);
    klog::event!(Info, "futex", "table.ready", buckets = n, cpus = cpus);
    Ok(t)
}

impl Table {
    pub(crate) fn index(&self, k: &Key) -> u32 {
        (k.hash() as usize & self.mask) as u32
    }
    pub(crate) fn lock(&self, i: u32) -> SpinLockGuard<'_, Vec<FEntry>> {
        self.buckets[i as usize].lock()
    }
    /// Both buckets, lower index first (the second by `try_lock` so the checker sees no same-class nesting);
    /// None for the second when both are one bucket.
    pub(crate) fn lock2(&self, i: u32, j: u32) -> (SpinLockGuard<'_, Vec<FEntry>>, Option<SpinLockGuard<'_, Vec<FEntry>>>) {
        if i == j {
            return (self.lock(i), None);
        }
        let (lo, hi) = if i < j { (i, j) } else { (j, i) };
        let a = self.lock(lo);
        let b = loop {
            if let Some(g) = self.buckets[hi as usize].try_lock() {
                break g;
            }
            core::hint::spin_loop();
        };
        if i < j { (a, Some(b)) } else { (b, Some(a)) }
    }
    /// Waiters in all buckets (ktests, `stats`).
    pub(crate) fn waiters(&self) -> usize {
        self.buckets.iter().map(|b| b.lock().len()).sum()
    }
}

/// Marks an entry's thread woken and wakes it (bucket lock held).
pub(crate) fn wake_entry(e: FEntry) {
    e.who.futex.woken.store(true, Ordering::Release);
    WAKES.inc();
    crate::run::wake_thread(&e.who, false);
}

/// Monotonic deadline of `d` now (CLOCK_REALTIME: the distance on the wall clock from now).
pub(crate) fn mono_deadline(d: Deadline) -> u64 {
    match d {
        Deadline::Monotonic(ns) => ns,
        Deadline::Realtime(ns) => {
            let now = time::now_ns();
            let real = realtime_ns();
            now.saturating_add(ns.saturating_sub(real))
        }
    }
}

fn realtime_ns() -> u64 {
    time::wall::realtime_ns().unwrap_or_else(time::now_ns)
}

pub(crate) fn expired(d: Option<Deadline>) -> bool {
    match d {
        None => false,
        Some(Deadline::Monotonic(ns)) => time::now_ns() >= ns,
        Some(Deadline::Realtime(ns)) => realtime_ns() >= ns,
    }
}

/// The calling thread, or EINVAL for code that is not a scheduled thread.
pub(crate) fn me() -> Result<Arc<Thread>, i64> {
    crate::current_thread().ok_or(EINVAL)
}

/// Starts a wait of `slots` slots: nothing of this thread is queued now.
pub(crate) fn begin(me: &Thread) {
    me.futex.woken.store(false, Ordering::Release);
    me.futex.pi_fixup.store(false, Ordering::Release);
    me.set_state(State::Blocking);
}

/// Back to running without sleeping (a mismatch or an error after `begin`).
pub(crate) fn cancel_block(me: &Thread) {
    me.cas_state(State::Blocking, State::Running);
}

/// Queues `slot` of `me` in bucket `bi` (its lock held as `q`).
pub(crate) fn enqueue(q: &mut Vec<FEntry>, bi: u32, me: &Arc<Thread>, key: Key, bitset: u32, slot: u32) {
    me.futex.set_loc(slot, bi);
    q.push(table::Entry { key, bitset, slot, who: me.clone() });
}

/// Sleeps once after the entries are queued: not at all when woken already, past the deadline or interrupted
/// (checked after a full fence, pairing with `kick_thread`'s).
pub(crate) fn sleep(me: &Thread, sp: &dyn Space, d: Option<Deadline>) {
    fence(Ordering::SeqCst);
    if me.futex.woken.load(Ordering::Acquire) || expired(d) || sp.interrupted().is_some() {
        cancel_block(me);
        return;
    }
    match d {
        Some(d) => {
            // 0 means "no deadline" to the scheduler: an absolute 0 has expired anyway (above).
            let m = mono_deadline(d).max(1);
            crate::HOOKS.block(Some(m));
        }
        None => crate::HOOKS.block(None),
    }
    cancel_block(me);
}

/// Takes `slot` of `me` off its bucket; false when a waker took it (it was woken).
pub(crate) fn unqueue(t: &Table, me: &Arc<Thread>, slot: u32) -> bool {
    loop {
        let bi = me.futex.loc(slot);
        if bi == u32::MAX {
            return false;
        }
        let mut q = t.lock(bi);
        if me.futex.loc(slot) != bi {
            // A requeue moved it meanwhile.
            continue;
        }
        let gone = table::remove(&mut q, slot, &|w: &Arc<Thread>| Arc::ptr_eq(w, me));
        drop(q);
        return gone.is_some();
    }
}

/// FUTEX_WAIT / FUTEX_WAIT_BITSET (linux-6.6/kernel/futex/waitwake.c:631 `futex_wait`, :577 `futex_wait_setup`):
/// Ok when woken (also spuriously by a waker of an unrelated futex sharing a pattern: callers re-check), EAGAIN
/// when the word is not `val`, ETIMEDOUT, EINVAL for an empty bitset, EFAULT, or `Space::interrupted`'s errno.
pub fn wait(sp: &dyn Space, addr: u64, shared: bool, val: u32, bitset: u32, deadline: Option<Deadline>) -> Result<(), i64> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    let me = me()?;
    let t = buckets()?;
    me.futex.reserve(1)?;
    WAITS.inc();
    let mut faults = 0;
    'retry: loop {
        let kr = sp.key(addr, shared, false)?;
        // setup
        loop {
            let bi = t.index(&kr.key);
            let mut q = t.lock(bi);
            if q.try_reserve(1).is_err() {
                return Err(ENOMEM);
            }
            match sp.word(addr, WordOp::Load) {
                Ok(v) if v != val => return Err(EAGAIN),
                Ok(_) => {
                    begin(&me);
                    enqueue(&mut q, bi, &me, kr.key, bitset, 0);
                    break;
                }
                Err(WordError::NotLoaded) => {
                    drop(q);
                    not_loaded();
                    return Err(EFAULT);
                }
                Err(_) => {
                    drop(q);
                    sp.read_u32(addr)?;
                    retry_fault(&mut faults)?;
                    if shared {
                        continue 'retry;
                    }
                }
            }
        }
        sleep(&me, sp, deadline);
        if !unqueue(t, &me, 0) {
            return Ok(());
        }
        if expired(deadline) {
            TIMEOUTS.inc();
            return Err(ETIMEDOUT);
        }
        if let Some(e) = sp.interrupted() {
            return Err(e);
        }
        // A spurious wake-up: set up again (the word is read again, as Linux's `goto retry`).
        drop(kr);
    }
}

/// FUTEX_WAKE / FUTEX_WAKE_BITSET (waitwake.c:143): how many were woken.
pub fn wake(sp: &dyn Space, addr: u64, shared: bool, nr_wake: i32, bitset: u32) -> Result<i64, i64> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    let kr = sp.key(addr, shared, false)?;
    let t = buckets()?;
    let mut q = t.lock(t.index(&kr.key));
    let n = table::wake(&mut q, &kr.key, nr_wake, bitset, &mut wake_entry);
    drop(q);
    drop(kr);
    n
}

/// Retries of one operation after a word access failed although the page could be faulted in (a race with
/// munmap/mprotect, or memory the direct-map rule refuses, such as a device mapping that is not write-back):
/// past this many the operation answers EFAULT instead of looping (a bound against hostile input, not a count).
pub(crate) const FAULT_RETRY_LIMIT: u32 = 16;

/// One more retry after a fault: EFAULT past the limit (said once), else gives the CPU away.
pub(crate) fn retry_fault(n: &mut u32) -> Result<(), i64> {
    FAULT_RETRIES.inc();
    *n += 1;
    if *n > FAULT_RETRY_LIMIT {
        if !RETRY_SEEN.swap(true, Ordering::Relaxed) {
            klog::event!(Warn, "futex", "word.unusable", thread = crate::current_id(); "a futex word keeps faulting although its page is mapped (not write-back RAM, or a racing unmap): EFAULT");
        }
        return Err(EFAULT);
    }
    crate::cond_resched();
    Ok(())
}

static RETRY_SEEN: AtomicBool = AtomicBool::new(false);
static NOT_LOADED_SEEN: AtomicBool = AtomicBool::new(false);

/// A futex operation from a thread whose CR3 is not the process's root (a kernel path calling for a process it
/// does not run as): refused with EFAULT, said once.
pub(crate) fn not_loaded() {
    if !NOT_LOADED_SEEN.swap(true, Ordering::Relaxed) {
        klog::event!(Warn, "futex", "word.not_loaded", thread = crate::current_id(); "a futex word access from a thread that does not run in that address space: EFAULT");
    }
}

/// Futex waiters queued now (all buckets; for tests and the `locks`-style views).
pub fn waiters() -> usize {
    TABLE.get().map_or(0, |t| t.waiters())
}
