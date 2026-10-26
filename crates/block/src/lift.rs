//! FS-12/FS-8b: priority inheritance for a file system's volume lock (fs-fat, fs-ext4). The lock is held
//! across most disk requests of the file system. A Background write-back that holds it would make an
//! Interactive reader waiting for the lock wait behind Background requests (priority inversion). So a request
//! of the lock's holder goes at least as Normal, and as Interactive while an Interactive thread waits for the
//! lock: priority inheritance per request (each one is a block run, so the holder's next request already
//! carries the waiter's priority; one already queued ages up). Requests made without the lock go at the
//! caller's own priority.
//!
//! A file system on an image file of another file system (ext4's loop image on the FAT stick) does its I/O
//! through that file system's code: it sets an I/O floor for the calling thread (`floor`) for the length of
//! the call, and `prio::current` (which the lower file system's requests use) honours it.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use devmodel::block::IoPriority;
use ksync::SpinLock;

klog::counter!(LIFTED, "block.io.lifted");

/// The volume lock's holder and its Interactive waiters, as the disk requests need them.
pub struct Lift {
    /// `ksync::thread_id` of the lock's holder (0: none).
    holder: AtomicU64,
    /// Interactive threads waiting for the lock now.
    urgent: AtomicUsize,
}

impl Default for Lift {
    fn default() -> Self {
        Lift::new()
    }
}

impl Lift {
    pub const fn new() -> Lift {
        Lift { holder: AtomicU64::new(0), urgent: AtomicUsize::new(0) }
    }

    /// The priority of a request of thread `tid` whose own priority is `me`.
    pub fn prio_for(&self, me: IoPriority, tid: u64) -> IoPriority {
        if tid == 0 || self.holder.load(Ordering::Acquire) != tid {
            return me;
        }
        let floor = if self.urgent.load(Ordering::Acquire) > 0 { IoPriority::Interactive } else { IoPriority::Normal };
        let p = crate::prio::higher(me, floor);
        if p != me {
            LIFTED.add(1);
        }
        p
    }

    /// The priority of a request of the calling thread.
    pub fn prio(&self) -> IoPriority {
        self.prio_for(crate::prio::current(), ksync::thread_id())
    }

    /// The calling thread holds the volume lock until the guard drops (drop it before the lock).
    pub fn hold(&self) -> Held<'_> {
        let tid = ksync::thread_id();
        self.holder.store(tid, Ordering::Release);
        Held(self, tid)
    }

    /// The calling thread waits for the volume lock (counted only when it is Interactive).
    pub fn wait(&self) -> Option<Waiting<'_>> {
        self.wait_as(crate::prio::current())
    }

    pub fn wait_as(&self, me: IoPriority) -> Option<Waiting<'_>> {
        (me == IoPriority::Interactive).then(|| {
            self.urgent.fetch_add(1, Ordering::AcqRel);
            Waiting(self)
        })
    }
}

pub struct Held<'a>(&'a Lift, u64);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let _ = self.0.holder.compare_exchange(self.1, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub struct Waiting<'a>(&'a Lift);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.urgent.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Per-thread I/O floors: (ksync thread id, priority). Empty unless some thread is inside a lifted call,
/// and `current` skips the lock then.
static FLOORS: SpinLock<Vec<(u64, IoPriority)>> = SpinLock::named("block.prio.floors", Vec::new());
static NFLOORS: AtomicUsize = AtomicUsize::new(0);

/// The calling thread's requests go at least at `p` until the guard drops (nothing when `p` is not higher
/// than its own priority, or when the table cannot grow).
pub fn floor(p: IoPriority) -> Floor {
    let tid = ksync::thread_id();
    if tid == 0 || crate::prio::higher(p, crate::prio::current()) == crate::prio::current() {
        return Floor(0);
    }
    let mut f = FLOORS.lock();
    if f.try_reserve(1).is_err() {
        return Floor(0);
    }
    f.push((tid, p));
    NFLOORS.fetch_add(1, Ordering::AcqRel);
    Floor(tid)
}

/// The floor of thread `tid`, if it has one (the highest of its entries).
pub fn floor_of(tid: u64) -> Option<IoPriority> {
    if NFLOORS.load(Ordering::Acquire) == 0 {
        return None;
    }
    FLOORS.lock().iter().filter(|e| e.0 == tid).map(|e| e.1).reduce(crate::prio::higher)
}

pub struct Floor(u64);

impl Drop for Floor {
    fn drop(&mut self) {
        if self.0 == 0 {
            return;
        }
        let mut f = FLOORS.lock();
        if let Some(i) = f.iter().rposition(|e| e.0 == self.0) {
            f.remove(i);
            NFLOORS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
