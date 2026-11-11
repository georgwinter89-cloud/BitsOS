//! SX-IPC: what SysV shared memory does with the address space (linux-7.3.1/ipc/shm.c:1650 `do_shmat`'s mapping part,
//! :1751 `ksys_shmdt`), under the space's lock as Linux's `mmap_lock`. The backing counts its own attachments.

use super::AsCore;
use crate::vma::{FileRef, VmaKind};
use crate::PAGE;
use alloc::sync::Arc;
use hal::paging::tlb;
use uapi::errno::*;
use vfs::mmap::MapBacking;

const MAP_SHARED: u64 = 0x01;
const MAP_FIXED: u64 = 0x10;

impl AsCore {
    /// Maps a segment of `len` bytes. `addr` 0: anywhere. Else that address (MAP_FIXED): without `remap` a mapping in
    /// the range is EINVAL, not replaced. `may_write`: not SHM_RDONLY (VM_SHARED and VM_MAYWRITE).
    pub fn shm_attach(&self, addr: u64, len: u64, prot: u64, remap: bool, may_write: bool, backing: Arc<dyn MapBacking>) -> Result<u64, i64> {
        let mut g = self.inner.lock();
        if addr != 0 && !remap {
            let end = addr.checked_add(len).ok_or(EINVAL)?;
            if !g.space.tree.is_free(addr, end) {
                return Err(EINVAL);
            }
        }
        let flags = if addr != 0 { MAP_SHARED | MAP_FIXED } else { MAP_SHARED };
        let mut plan = g.space.plan_mmap_file(addr, len, prot, flags, 0).map_err(|e| e.code())?;
        plan.account = false;
        let file = FileRef::file(backing, 0, may_write, may_write);
        self.mmap_locked(&mut g, &plan, VmaKind::File, Some(file)).map_err(|e| e.code())
    }

    /// shmdt: the first VMA from `addr` on whose file page fits its distance from `addr` starts the attachment (a VMA
    /// that starts below `addr` wraps and never fits, as Linux's unsigned sum); the rest of that attachment within the
    /// segment's size goes with it. EINVAL when there is none.
    pub fn shm_detach(&self, addr: u64) -> Result<(), i64> {
        let mut g = self.inner.lock();
        let attached = |v: &crate::vma::Vma| v.file.as_ref().filter(|f| f.backing.sysv_segment_size().is_some() && v.start.wrapping_sub(addr) / PAGE == f.pgoff).map(|f| f.backing.clone());
        let mut at = addr;
        let first = loop {
            let Some(v) = g.space.tree.next_from(at) else { return Err(EINVAL) };
            if let Some(b) = attached(v) {
                break (v.start, v.end, b);
            }
            at = v.end;
        };
        let (start, end, backing) = first;
        let size = backing.sysv_segment_size().unwrap_or(0).next_multiple_of(PAGE);
        self.remove_range(&mut g, start, end, tlb::Cause::Munmap).map_err(|e| e.code())?;
        let mut at = end;
        while let Some(v) = g.space.tree.next_from(at) {
            if v.end - addr > size {
                break;
            }
            let (s, e) = (v.start, v.end);
            at = e;
            if attached(v).is_some_and(|b| Arc::ptr_eq(&b, &backing)) {
                self.remove_range(&mut g, s, e, tlb::Cause::Munmap).map_err(|e| e.code())?;
            }
        }
        Ok(())
    }
}
