//! Shared anonymous memory (MM-5b, FS-5 change request 4): `mmap(MAP_SHARED|MAP_ANONYMOUS)` gets a nameless
//! tmpfs file, as Linux's `shmem_zero_setup` does, so every space that inherits the mapping through fork sees
//! the same pages, also pages first touched after the fork, and `madvise(DONTNEED)` drops only this space's
//! entries (the data stays in the object). The file's size is the mapping's length, charged to the commit at
//! creation (this object is size-charged; other tmpfs files are charged per page since FS-14), so a fault inside it never meets an empty pool.
//!
//! The object is an O_TMPFILE file in the kernel's own view of `/dev/shm`; it lives as long as a mapping of it.
//! It is made without the space's lock (the VFS takes its own locks); the address checks run before and again
//! after, so their errors come first, as on Linux.

use super::AsCore;
use crate::linux::Errno;
use crate::vma::{FileRef, VmaKind};
use alloc::sync::Arc;
use vfs::mmap::FileMap;

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_SHARED: u64 = 0x01;

klog::counter!(SHM_OBJECTS, "mm.as.shm_objects");

/// A nameless, `len`-byte tmpfs file mapped shared (ENOMEM when the commit or `/dev/shm` refuses it).
fn object(len: u64) -> Result<Arc<FileMap>, Errno> {
    use vfs::abi::{O_DIRECTORY, O_EXCL, O_RDWR, O_TMPFILE_BIT};
    let f = vfs::root_ns().open_flags(b"/dev/shm", O_TMPFILE_BIT | O_DIRECTORY | O_RDWR | O_EXCL, 0o600).map_err(|e| {
        klog::event!(Warn, "mm", "shm.no_object", error = e; "no tmpfs object for shared anonymous memory (/dev/shm missing?)");
        Errno::NoMem
    })?;
    f.inode().ok_or(Errno::NoMem)?.truncate(len).map_err(|_| Errno::NoMem)?;
    let m = vfs::mmap::check(&f, PROT_READ | PROT_WRITE, MAP_SHARED, 0, len).map_err(|_| Errno::NoMem)?;
    SHM_OBJECTS.inc();
    Ok(m.backing)
}

impl AsCore {
    pub(crate) fn mmap_shared_anon(&self, addr: u64, len: u64, prot: u64, flags: u64, off: u64) -> Result<u64, Errno> {
        let plan = self.inner.lock().space.plan_mmap(addr, len, prot, flags, off)?;
        let obj = object(plan.len)?;
        let mut g = self.inner.lock();
        let plan = g.space.plan_mmap(addr, len, prot, flags, off)?;
        let file = FileRef::file(obj, 0, true, true);
        self.mmap_locked(&mut g, &plan, VmaKind::Anonymous, Some(file))
    }
}
