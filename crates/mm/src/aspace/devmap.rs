//! Device mappings in an address space (MM-5b, GX-D7): `mmap` of a registered device file and the device's
//! `write_protect` (see `crate::device` for the contract).

use super::{AsCore, CACHE_BITS};
use crate::device::{self, DeviceMap};
use crate::linux::Errno;
use crate::vma::{FileRef, VmaKind};
use crate::PAGE;
use alloc::sync::Arc;
use hal::paging::{self, PageFlags};
use vfs::OpenFile;

impl AsCore {
    /// `mmap` of the device file `f` (registered with `device::register`): the address checks as for any file,
    /// then the file checks and the device's own `check`, then a file VMA whose pages come from the device's
    /// fault hook. Never charged to the commit. Errors are errnos.
    #[allow(clippy::too_many_arguments)]
    pub fn sys_mmap_device(&self, addr: u64, len: u64, prot: u64, flags: u64, off: u64, f: &OpenFile, map: &Arc<DeviceMap>) -> Result<u64, i64> {
        let mut g = self.inner.lock();
        let mut plan = g.space.plan_mmap_file(addr, len, prot, flags, off).map_err(|e| e.code())?;
        let m = device::check(f, map, prot, flags, off >> 12, plan.len)?;
        if !map.add_space(&self.me) {
            return Err(Errno::NoMem.code());
        }
        plan.account = false;
        let file = FileRef { backing: m.backing, pgoff: off >> 12, shared: true, may_write: m.may_write, may_exec: true, device: Some(map.clone()) };
        self.mmap_locked(&mut g, &plan, VmaKind::File, Some(file)).map_err(|e| e.code())
    }

    /// `DeviceMap::write_protect` for this space: device pages `first..=last` of `map` read-only (keeping their
    /// memory type), one shootdown per VMA. Returns how many entries changed.
    pub(crate) fn device_write_protect(&self, map: &DeviceMap, first: u64, last: u64) -> u64 {
        let g = self.inner.lock();
        let mut n = 0;
        for v in g.space.tree.iter() {
            let Some(f) = v.file.as_ref().filter(|f| f.device.as_ref().is_some_and(|d| core::ptr::eq(&**d, map))) else { continue };
            let pages = (v.end - v.start) / PAGE;
            let (lo, hi) = (first.max(f.pgoff), last.min(f.pgoff + pages - 1));
            if lo > hi {
                continue;
            }
            let (s, e) = (v.start + (lo - f.pgoff) * PAGE, v.start + (hi - f.pgoff + 1) * PAGE);
            let r = paging::protect_user(self.root, s, e - s, &mut |u| {
                u.flags.contains(PageFlags::WRITABLE).then_some(PageFlags(u.flags.0 & !PageFlags::WRITABLE | (u.flags.0 & CACHE_BITS)))
            });
            match r {
                Ok((k, o)) => {
                    n += k;
                    if !o.confirmed() {
                        super::UNCONFIRMED.inc();
                        klog::event!(Warn, "mm", "as.shootdown_unconfirmed", root = klog::Hex(self.root), start = klog::Hex(s), end = klog::Hex(e),
                            cpus = o.unanswered, resends = o.resends, not_ready = o.not_ready;
                            "a CPU did not answer the TLB shootdown of a device write-protect: a store there may go unseen until it does");
                    }
                }
                Err(e) => klog::event!(Error, "mm", "as.protect_failed", start = klog::Hex(s), error = alloc::format!("{e:?}")),
            }
        }
        n
    }
}
