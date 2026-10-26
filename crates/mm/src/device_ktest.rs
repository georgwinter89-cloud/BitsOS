//! ktests of MM-5b's device mappings (GX-D7; DP-2's fbdev deferred io is the first user): a test device with a
//! four-page WC DMA buffer as its aperture answers faults read-only until written (dirty tracking);
//! `write_protect` makes them read-only again everywhere; fork, mremap, madvise and munmap treat the pages as the
//! device's. Then the trust boundary: answers outside the apertures end in SIGBUS, MMIO apertures over RAM or
//! with another type than a kernel window are refused, a read-only fd gets no writes through MAP_PRIVATE.

use crate::device::{Aperture, ApertureError, DeviceMap, DeviceOps, DevicePage};
use crate::dma::{DmaBuffer, DmaCache, DmaConstraints};
use crate::linux::{madv, Errno, MAP_SHARED, MREMAP_MAYMOVE};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
use hal::paging::{self, CacheType, PageFlags};
use ktest::{kassert, ktest, TestResult};
use vfs::pagecache::PageFault;

const P: u64 = 4096;
const RW: u64 = 3;
const PF_P: u64 = 1;
const PF_W: u64 = 2;
const PF_U: u64 = 4;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// Four pages of aperture 0, a dirty bit per page, every fault counted.
struct TestDev {
    dirty: AtomicU64,
    faults: AtomicU64,
}

impl DeviceOps for TestDev {
    fn check(&self, pgoff: u64, pages: u64, _prot: u64, shared: bool) -> Result<(), i64> {
        if !shared || pgoff > 64 || pages > 64 { Err(uapi::errno::EINVAL) } else { Ok(()) }
    }
    fn fault(&self, pgoff: u64, write: bool) -> Result<DevicePage, PageFault> {
        self.faults.fetch_add(1, SeqCst);
        if pgoff >= 4 {
            return Err(PageFault::Bus);
        }
        if write {
            self.dirty.fetch_or(1 << pgoff, SeqCst);
        }
        Ok(DevicePage { aperture: 0, page: pgoff, writable: self.dirty.load(SeqCst) & (1 << pgoff) != 0 })
    }
}

fn wc_buffer(pages: u64) -> Result<Arc<DmaBuffer>, String> {
    let c = DmaConstraints::dma64().with_cache(DmaCache::WriteCombining);
    DmaBuffer::alloc((pages * P) as usize, c).map(Arc::new).map_err(|x| format!("WC DMA buffer: {x:?}"))
}

fn leaf(a: &AddressSpace, va: u64) -> Option<(u64, PageFlags)> {
    paging::translate(a.root(), va)
}

fn deferred_io() -> TestResult {
    let ns = vfs::root_ns();
    let Ok(f) = ns.open_flags(b"/dev/zero", vfs::abi::O_RDWR, 0) else { return ktest::skip("no /dev/zero to stand in for a device file") };
    let buf = match wc_buffer(4) {
        Ok(b) => b,
        Err(x) => return ktest::skip(&x),
    };
    let frames: Vec<u64> = (0..4).map(|i| buf.phys() + i * P).collect();
    let dev = Arc::new(TestDev { dirty: AtomicU64::new(0), faults: AtomicU64::new(0) });
    let map = DeviceMap::new(dev.clone(), alloc::vec![Aperture::Ram(buf.clone())]).map_err(e)?;
    let r = (|| -> TestResult {
        let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
        kassert!(a.sys_mmap_device(0, 4 * P, RW, 0x02, 0, &f, &map) == Err(uapi::errno::EINVAL), "the device refuses MAP_PRIVATE");
        let m = a.sys_mmap_device(0, 8 * P, RW, MAP_SHARED, 0, &f, &map).map_err(e)?;
        kassert!(a.committed_pages() == 0, "device memory is not charged");
        // A read maps the page read-only with the device's memory type.
        kassert!(a.read_u64(m).map_err(e)? == 0);
        let (p0, f0) = leaf(&a, m).ok_or("page 0 not mapped")?;
        kassert!(p0 == frames[0] && !f0.contains(PageFlags::WRITABLE) && f0.cache() == Some(CacheType::WriteCombining), "page 0: {p0:#x} {f0:?}");
        // The first store faults: dirty, writable, still WC.
        a.handle_user_fault(m, PF_U | PF_W | PF_P).map_err(e)?;
        let (_, f0) = leaf(&a, m).ok_or("page 0 gone")?;
        kassert!(dev.dirty.load(SeqCst) & 1 == 1 && f0.contains(PageFlags::WRITABLE) && f0.cache() == Some(CacheType::WriteCombining), "after the store: {f0:?}");
        a.write_u64(m + P, 0x55).map_err(e)?;
        kassert!(dev.dirty.load(SeqCst) == 3, "a copy into page 1 dirtied it ({:#b})", dev.dirty.load(SeqCst));
        // The device copied pages 0 and 1 out: clean and write-protected again everywhere.
        dev.dirty.store(0, SeqCst);
        kassert!(map.write_protect(0, 3) == 2, "two entries write-protected");
        for va in [m, m + P] {
            let (_, fl) = leaf(&a, va).ok_or("page gone after write_protect")?;
            kassert!(!fl.contains(PageFlags::WRITABLE) && fl.cache() == Some(CacheType::WriteCombining), "{va:#x}: {fl:?}");
        }
        a.write_u64(m, 0x77).map_err(e)?;
        kassert!(dev.dirty.load(SeqCst) == 1, "the next store is seen again");
        // Past the device memory: SIGBUS.
        kassert!(a.handle_user_fault(m + 5 * P, PF_U) == Err(crate::FaultReason::Bus), "page 5 is past the device");
        // mprotect keeps the type and the read-only pages read-only (their stores must still fault).
        a.sys_mprotect(m, 4 * P, 1).map_err(e)?;
        a.sys_mprotect(m, 4 * P, RW).map_err(e)?;
        let (_, fl) = leaf(&a, m).ok_or("page 0 gone after mprotect")?;
        kassert!(!fl.contains(PageFlags::WRITABLE) && fl.cache() == Some(CacheType::WriteCombining), "after mprotect: {fl:?}");
        // madvise and mremap treat it as Linux's VM_PFNMAP/VM_DONTEXPAND.
        kassert!(a.sys_madvise(m, P, madv::DONTNEED) == Err(Errno::Inval), "DONTNEED on a device mapping");
        kassert!(a.sys_madvise(m, P, madv::DOFORK) == Err(Errno::Inval), "DOFORK on a device mapping");
        kassert!(a.sys_mremap(m, 8 * P, 9 * P, MREMAP_MAYMOVE, 0) == Err(Errno::Fault), "growing a device mapping");
        let b = a.fork().map_err(e)?;
        kassert!(map.mapped_by() == 2, "the child maps the device too ({})", map.mapped_by());
        kassert!(b.read_u64(m).map_err(e)? == 0x77 && leaf(&b, m).map(|x| x.0) == Some(frames[0]), "the child faults in the same page");
        let to = a.sys_mremap(m, 8 * P, 8 * P, MREMAP_MAYMOVE | 2, 0x40_0000_0000).map_err(e)?;
        kassert!(leaf(&a, to).map(|x| x.0) == Some(frames[0]) && leaf(&a, m).is_none(), "the move keeps the device page");
        a.sys_munmap(to, 8 * P).map_err(e)?;
        drop(b);
        drop(a);
        kassert!(mm_core::boot_buddy().is_some_and(|bb| bb.run_allocated(buf.phys(), 4, mm_core::Kind::Dma)), "MM freed a device frame");
        kassert!(map.mapped_by() == 0, "spaces still registered: {}", map.mapped_by());
        Ok(())
    })();
    drop(map);
    kassert!(Arc::strong_count(&buf) == 1, "the device map kept the buffer after its drop");
    r
}
ktest!("mm.device.deferred_io", Safe, deferred_io);

/// A driver bug on purpose: answers outside its apertures (an index past them, a page past the aperture). MM maps
/// nothing and ends the access with SIGBUS (`FaultReason::Bus`), also for a store through the user copy.
struct BadDev;

impl DeviceOps for BadDev {
    fn fault(&self, pgoff: u64, write: bool) -> Result<DevicePage, PageFault> {
        Ok(match pgoff {
            0 => DevicePage { aperture: 0, page: 0, writable: write },
            1 => DevicePage { aperture: 1, page: 0, writable: true },
            _ => DevicePage { aperture: 0, page: 99, writable: true },
        })
    }
}

fn bad_pages() -> TestResult {
    let ns = vfs::root_ns();
    let Ok(rw) = ns.open_flags(b"/dev/zero", vfs::abi::O_RDWR, 0) else { return ktest::skip("no /dev/zero to stand in for a device file") };
    let ro = ns.open_flags(b"/dev/zero", vfs::abi::O_RDONLY, 0).map_err(e)?;
    let buf = match wc_buffer(1) {
        Ok(b) => b,
        Err(x) => return ktest::skip(&x),
    };
    let map = DeviceMap::new(Arc::new(BadDev), alloc::vec![Aperture::Ram(buf.clone())]).map_err(e)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = a.sys_mmap_device(0, 3 * P, RW, MAP_SHARED, 0, &rw, &map).map_err(e)?;
    kassert!(a.read_u64(m).is_ok() && leaf(&a, m).map(|x| x.0) == Some(buf.phys()), "page 0 is the aperture's");
    kassert!(a.handle_user_fault(m + P, PF_U | PF_W) == Err(crate::FaultReason::Bus), "aperture index past the apertures");
    kassert!(a.handle_user_fault(m + 2 * P, PF_U) == Err(crate::FaultReason::Bus), "page past the aperture");
    kassert!(a.write_u64(m + 2 * P, 1).is_err() && leaf(&a, m + P).is_none() && leaf(&a, m + 2 * P).is_none(), "a bad answer got mapped");
    // MAP_PRIVATE writes the device's frames (no copy-on-write): a read-only fd gets no writes that way either.
    kassert!(a.sys_mmap_device(0, P, RW, 0x02, 0, &ro, &map) == Err(uapi::errno::EACCES), "MAP_PRIVATE|PROT_WRITE on an O_RDONLY fd");
    let p = a.sys_mmap_device(0, P, 1, 0x02, 0, &ro, &map).map_err(e)?;
    kassert!(a.sys_mprotect(p, P, RW) == Err(Errno::Acces), "mprotect to writable of a private map of an O_RDONLY fd");
    kassert!(a.sys_mmap_device(0, P, 1, MAP_SHARED, 0, &ro, &map).is_ok(), "a read-only shared map of an O_RDONLY fd is fine");
    let w = a.sys_mmap_device(0, P, RW, 0x02, 0, &rw, &map).map_err(e)?;
    kassert!(a.write_u64(w, 7).is_ok(), "MAP_PRIVATE of an O_RDWR fd may write (as shared)");
    // Apertures: none, empty, unaligned, MMIO over RAM.
    let ops = || -> Arc<dyn DeviceOps> { Arc::new(BadDev) };
    let mmio = |phys, len| Aperture::Mmio { phys, len, cache: CacheType::WriteCombining };
    kassert!(DeviceMap::new(ops(), alloc::vec![]).err() == Some(ApertureError::Empty));
    kassert!(DeviceMap::new(ops(), alloc::vec![mmio(0x3F_F000_0000, 0)]).err() == Some(ApertureError::Empty));
    kassert!(DeviceMap::new(ops(), alloc::vec![mmio(0x3F_F000_0800, P)]).err() == Some(ApertureError::NotAligned));
    kassert!(DeviceMap::new(ops(), alloc::vec![mmio(!0xFFF, P)]).err() == Some(ApertureError::NotAligned));
    kassert!(DeviceMap::new(ops(), alloc::vec![mmio(buf.phys(), P)]).err() == Some(ApertureError::OverlapsRam), "MMIO over a DMA buffer (RAM)");
    let heap = mm_core::alloc_zeroed(mm_core::Kind::Heap).ok_or("no frame")?;
    let r = DeviceMap::new(ops(), alloc::vec![mmio(heap, P)]).err();
    if let Some(fa) = mm_core::frames() {
        fa.free(heap, 0, mm_core::Kind::Heap);
    }
    kassert!(r == Some(ApertureError::OverlapsRam), "MMIO over a kernel heap frame: {r:?}");
    kassert!(DeviceMap::new(ops(), alloc::vec![mmio(0x3F_F000_0000, 4 * P)]).is_ok(), "MMIO outside RAM, no window over it");
    Ok(())
}
ktest!("mm.device.bad_pages", Safe, bad_pages);

/// An MMIO aperture with another type than a kernel window over the same device memory is refused (Linux
/// `reserve_pfn_range`). QemuOnly: it maps a kernel window over a physical range no device decodes under QEMU
/// (nothing is ever accessed, but on real hardware the range could be someone's BAR).
fn aperture_types() -> TestResult {
    const AT: u64 = 0x3F_F000_0000;
    let w = crate::vmap::map_device(AT, 2 * P, CacheType::Uncached).map_err(e)?;
    let mk = |cache| DeviceMap::new(Arc::new(BadDev), alloc::vec![Aperture::Mmio { phys: AT + P, len: 4 * P, cache }]).err();
    kassert!(mk(CacheType::WriteCombining) == Some(ApertureError::TypeConflict), "WC over a UC window");
    kassert!(mk(CacheType::Uncached).is_none(), "UC over a UC window");
    drop(w);
    kassert!(mk(CacheType::WriteCombining).is_none(), "WC once the window is gone");
    Ok(())
}
ktest!("mm.device.aperture_types", QemuOnly, aperture_types);
