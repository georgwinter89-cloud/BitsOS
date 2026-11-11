//! shmat and shmdt (linux-7.3.1/ipc/shm.c:1520 `do_shmat`, :1751 `ksys_shmdt`). A segment's VMA has its own backing
//! around the file's `FileMap`; MM opens and closes the VMAs (`vma_open`, `vma_close`, Linux's `shm_vm_ops`), so
//! fork, munmap, mprotect's splits and exec count as on Linux, and exec detaches by dropping the address space.

use super::Caller;
use super::shm::{self, Segment};
use crate::ns::misc::IpcNs;
use alloc::sync::Arc;
use mm::AddressSpace;
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::Errno;
use vfs::mmap::{FileMap, MapBacking};
use vfs::pagecache::PageFault;

const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;

klog::counter!(ATTACHES, "ipc.shm.attaches");

/// One attachment (Linux's `shm_file_data` with its clone of the file).
struct ShmMap {
    file: Arc<FileMap>,
    seg: Arc<Segment>,
    ns: Arc<IpcNs>,
    size: u64,
}

fn current_tgid() -> Option<u32> {
    crate::task::current_thread().map(|t| t.group().tgid)
}

impl MapBacking for ShmMap {
    fn get_page(&self, pgoff: u64, write: bool) -> Result<u64, PageFault> {
        self.file.get_page(pgoff, write)
    }
    fn put_page(&self, phys: u64) -> bool {
        self.file.put_page(phys)
    }
    fn shared(&self) -> bool {
        self.file.shared()
    }
    fn track_dirty(&self) -> bool {
        self.file.track_dirty()
    }
    fn sync(&self, first: u64, last: u64) -> Result<(), Errno> {
        self.file.sync(first, last)
    }
    fn file(&self) -> Option<&FileMap> {
        Some(&self.file)
    }
    /// `__shm_open`.
    fn vma_open(&self) {
        let mut i = self.seg.inner.lock();
        i.atim = super::now_secs();
        if let Some(t) = current_tgid() {
            i.lpid = t;
        }
        i.nattch += 1;
    }
    /// `__shm_close`. Linux takes the table's lock first; only the last close needs it here.
    fn vma_close(&self) {
        let due = {
            let mut i = self.seg.inner.lock();
            i.dtim = super::now_secs();
            if let Some(t) = current_tgid() {
                i.lpid = t;
            }
            i.nattch = i.nattch.saturating_sub(1);
            shm::may_destroy(&i, &self.ns)
        };
        if due {
            shm::destroy_if_due(&self.seg, &self.ns);
        }
    }
    /// Linux's `is_mergeable_vma` refuses a VMA with a close op: two VMAs stay two attachments.
    fn mergeable(&self) -> bool {
        false
    }
    fn sysv_segment_size(&self) -> Option<u64> {
        Some(self.size)
    }
}

/// shmat: the address it landed at. `SHMLBA` is the page size on x86.
pub fn shmat(c: &Caller<'_>, mm: &AddressSpace, id: i32, shmaddr: u64, flg: i32) -> Result<u64, Errno> {
    if id < 0 {
        return Err(EINVAL);
    }
    let mut addr = shmaddr;
    if addr != 0 {
        if addr & (PAGE - 1) != 0 {
            if flg & SHM_RND == 0 {
                return Err(EINVAL);
            }
            addr &= !(PAGE - 1);
            if addr == 0 && flg & SHM_REMAP != 0 {
                return Err(EINVAL);
            }
        }
    } else if flg & SHM_REMAP != 0 {
        return Err(EINVAL);
    }
    let rdonly = flg & SHM_RDONLY != 0;
    let (mut prot, mut acc) = if rdonly { (PROT_READ, S_IRUGO) } else { (PROT_READ | PROT_WRITE, S_IRUGO | S_IWUGO) };
    if flg & SHM_EXEC != 0 {
        prot |= PROT_EXEC;
        acc |= S_IXUGO;
    }
    let seg = c.ns.tables.shm.ids.read().obtain_check(id)?;
    let (path, size) = {
        let mut i = seg.inner.lock();
        if !i.perm.permits(&c.ns, &c.cred, acc) {
            return Err(EACCES);
        }
        if i.perm.deleted {
            return Err(EIDRM);
        }
        let path = i.file.as_ref().and_then(|f| f.path.clone()).ok_or(EIDRM)?;
        // Keeps the segment while the mapping is being made (shm.c:1617).
        i.nattch += 1;
        (path, i.segsz)
    };
    let backing = Arc::new(ShmMap { file: FileMap::new(path, !rdonly, true), seg: seg.clone(), ns: c.ns.clone(), size });
    let r = mm.shm_attach(addr, size, prot, flg & SHM_REMAP != 0, !rdonly, backing);
    if r.is_ok() {
        ATTACHES.inc();
    }
    let due = {
        let mut i = seg.inner.lock();
        if r.is_ok() {
            i.lpid = c.tgid();
        }
        i.nattch -= 1;
        shm::may_destroy(&i, &c.ns)
    };
    if due {
        shm::destroy_if_due(&seg, &c.ns);
    }
    r
}

/// shmdt.
pub fn shmdt(mm: &AddressSpace, addr: u64) -> Result<(), Errno> {
    if addr & (PAGE - 1) != 0 {
        return Err(EINVAL);
    }
    mm.shm_detach(addr)
}
