//! An open file's position (Linux `f_pos`) and its lock (`f_pos_lock`). As Linux 7 (`fdget_pos`,
//! `file_needs_f_pos_lock`), the lock serialises read, write and lseek only for regular files and directories
//! (FMODE_ATOMIC_POS, set in `do_dentry_open`), a directory always, a regular file only while the description may
//! be used by someone else: another fd, another process or another thread of the table. Devices (/dev/null, /dev/zero,
//! fbdev) never take it: before, eight `cat > /dev/null` sharing their stdout waited for each other's writes.
//! pread and pwrite never touch it.

use crate::fd::FdTable;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use ksync::{Mutex, MutexGuard};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// Not FMODE_ATOMIC_POS (devices, nameless objects that are not regular): the position is read and stored
    /// without a lock; concurrent users race on it as on Linux.
    Free,
    /// A regular file: locked unless the caller is its only user.
    Regular,
    /// A directory: always locked (getdents reads and moves it in one step; Linux: `iterate_shared`).
    Dir,
}

pub(crate) struct FilePos {
    /// How often the lock was taken: counted under it (no atomic of its own), so a measurement of one description
    /// is not mixed with the kernel-wide class `vfs.file.pos` (ktest `fs.fpos.contention`).
    lock: Mutex<u64>,
    at: AtomicU64,
    kind: Kind,
}

impl FilePos {
    pub(crate) fn new(kind: Kind) -> FilePos {
        FilePos { lock: Mutex::named("vfs.file.pos", 0), at: AtomicU64::new(0), kind }
    }
    /// `alone`: the caller showed that nobody else can use the description during the call (`OpenFile::alone`).
    pub(crate) fn guard(&self, alone: bool) -> PosGuard<'_> {
        let locked = match self.kind {
            Kind::Free => false,
            Kind::Regular => !alone,
            Kind::Dir => true,
        };
        let g = locked.then(|| {
            let mut g = self.lock.lock();
            *g += 1;
            g
        });
        PosGuard { _g: g, v: self.at.load(Ordering::Acquire), at: &self.at }
    }
    /// How often a call took the lock so far.
    pub(crate) fn locks(&self) -> u64 {
        *self.lock.lock()
    }
    /// The position as of the last store (fdinfo; Linux reads `f_pos` unlocked there).
    pub(crate) fn racy(&self) -> u64 {
        self.at.load(Ordering::Relaxed)
    }
}

/// The position for one call; stored back when it ends.
pub(crate) struct PosGuard<'a> {
    _g: Option<MutexGuard<'a, u64>>,
    v: u64,
    at: &'a AtomicU64,
}

impl core::ops::Deref for PosGuard<'_> {
    type Target = u64;
    fn deref(&self) -> &u64 {
        &self.v
    }
}

impl core::ops::DerefMut for PosGuard<'_> {
    fn deref_mut(&mut self) -> &mut u64 {
        &mut self.v
    }
}

impl Drop for PosGuard<'_> {
    fn drop(&mut self) {
        // Before the lock goes (`_g` drops after this body): the next holder reads this value.
        self.at.store(self.v, Ordering::Release);
    }
}

/// The threads that use an fd table (Linux `files_struct.count`), counted by the process layer: one `FdUse`
/// per live thread on the table its image has (`proc::task::start`, `exec`, `thread_ends`). A table nobody counts
/// (kernel callers, tests) has 0 and is never treated as one thread's own.
#[derive(Default)]
pub(crate) struct Users(Arc<AtomicUsize>);

impl Users {
    pub(crate) fn one(&self) -> bool {
        self.0.load(Ordering::Acquire) == 1
    }
}

/// A thread's place on an fd table's count; given back when dropped.
pub struct FdUse(Arc<AtomicUsize>);

impl FdUse {
    pub fn new(t: &FdTable) -> FdUse {
        let c = t.users().0.clone();
        c.fetch_add(1, Ordering::AcqRel);
        FdUse(c)
    }
}

impl Drop for FdUse {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
