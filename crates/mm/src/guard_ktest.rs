//! L7-VFS ktests of guard regions (MADV_GUARD_INSTALL / MADV_GUARD_REMOVE, linux-7.3.1/mm/madvise.c:1124, :1252):
//! a touch is a fault with SEGV_MAPERR's reason, a kernel copy EFAULT, MADV_POPULATE EFAULT; the pages under an
//! install go; DONTNEED keeps the guards, munmap and REMOVE take them; fork copies them (not into WIPEONFORK),
//! mremap moves them; a guarded 2 MiB never takes a huge page and a guarded 64 KiB block no fault-around;
//! device and locked VMAs refuse an install with EINVAL, mlock over a guard is ENOMEM.

use crate::linux::{madv, Errno, MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MREMAP_FIXED, MREMAP_MAYMOVE};
use crate::{AddressSpace, FaultReason, Fill, LockLimit, MmError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::paging::{self, PageSize};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = 3;
const PF_W: u64 = 2;
const PF_U: u64 = 4;
const HUGE: u64 = 2 << 20;
/// 2 MiB aligned, apart from the other mm ktests' places.
const BASE: u64 = 0x33_0000_0000;

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn filled(a: &AddressSpace, lo: u64, hi: u64) -> Vec<u64> {
    (0..(hi - lo) / P).filter(|i| paging::translate(a.root(), lo + i * P).is_some()).collect()
}

fn touch(a: &AddressSpace, va: u64, write: bool) -> Result<(), FaultReason> {
    a.handle_user_fault(va, PF_U | if write { PF_W } else { 0 })
}

fn guarded(a: &AddressSpace, va: u64, what: &str) -> TestResult {
    kassert!(touch(a, va, false) == Err(FaultReason::NotMapped), "{what}: a read of {va:#x} is no SEGV_MAPERR fault");
    kassert!(touch(a, va, true) == Err(FaultReason::NotMapped), "{what}: a write of {va:#x} is no SEGV_MAPERR fault");
    kassert!(a.read_u64(va) == Err(MmError::Fault), "{what}: a kernel copy from {va:#x} is no EFAULT");
    kassert!(paging::translate(a.root(), va).is_none(), "{what}: {va:#x} got a page");
    Ok(())
}

fn basics() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 8 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for i in 0..8 {
        a.write_u64(m + i * P, 0x100 + i).map_err(e)?;
    }
    a.sys_madvise(m + 2 * P, 3 * P, madv::GUARD_INSTALL).map_err(e)?;
    kassert!(filled(&a, m, m + 8 * P) == [0, 1, 5, 6, 7], "the install took the pages: {:?}", filled(&a, m, m + 8 * P));
    for i in 2..5 {
        guarded(&a, m + i * P, "installed")?;
    }
    kassert!(a.read_u64(m + P).map_err(e)? == 0x101 && a.read_u64(m + 5 * P).map_err(e)? == 0x105, "the neighbours kept their data");
    kassert!(a.vmas().iter().filter(|v| v.start >= m && v.end <= m + 8 * P).count() == 1, "an install split the VMA");
    // A copy across a guard stops there; POPULATE answers EFAULT; a second install is no error.
    let mut b = [0u8; 16];
    kassert!(a.copy_from_user(&mut b, m + 2 * P - 8) == Err(MmError::Fault), "a copy into the guard");
    kassert!(a.sys_madvise(m, 8 * P, madv::POPULATE_WRITE) == Err(Errno::Fault), "POPULATE_WRITE over a guard");
    a.sys_madvise(m + 3 * P, P, madv::GUARD_INSTALL).map_err(e)?;
    // DONTNEED and FREE keep the guards.
    a.sys_madvise(m, 8 * P, madv::DONTNEED).map_err(e)?;
    a.sys_madvise(m, 8 * P, madv::FREE).map_err(e)?;
    guarded(&a, m + 3 * P, "after DONTNEED")?;
    // REMOVE of one page: it reads zero again, the others stay guarded.
    a.sys_madvise(m + 3 * P, P, madv::GUARD_REMOVE).map_err(e)?;
    touch(&a, m + 3 * P, true).map_err(|r| format!("removed page: {r:?}"))?;
    kassert!(a.read_u64(m + 3 * P).map_err(e)? == 0, "a removed guard reads zero");
    guarded(&a, m + 2 * P, "beside the removed one")?;
    guarded(&a, m + 4 * P, "beside the removed one")?;
    // A hole in the range: the VMAs around it are done, the call answers ENOMEM.
    a.sys_munmap(m + 6 * P, P).map_err(e)?;
    kassert!(a.sys_madvise(m + 5 * P, 3 * P, madv::GUARD_INSTALL) == Err(Errno::NoMem), "a hole is ENOMEM");
    guarded(&a, m + 7 * P, "after the hole")?;
    // munmap takes them: a new mapping there has none.
    a.sys_munmap(m, 8 * P).map_err(e)?;
    a.sys_mmap(m, 8 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for i in 0..8 {
        touch(&a, m + i * P, true).map_err(|r| format!("page {i} after munmap and mmap: {r:?}"))?;
    }
    // MAP_FIXED over guards takes them too.
    a.sys_madvise(m, P, madv::GUARD_INSTALL).map_err(e)?;
    a.sys_mmap(m, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    touch(&a, m, false).map_err(|r| format!("after MAP_FIXED: {r:?}"))?;
    Ok(())
}
ktest!("mm.as.guard_region", Safe, basics);

/// A write fault to a guard in a read-only VMA keeps the VMA's answer (Linux checks the access first).
fn order() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 4 * P, 1, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(m, 4 * P, madv::GUARD_INSTALL).map_err(e)?;
    kassert!(touch(&a, m, true) == Err(FaultReason::ReadOnly), "a write to a read-only guard: {:?}", touch(&a, m, true));
    kassert!(touch(&a, m, false) == Err(FaultReason::NotMapped));
    a.sys_mprotect(m, 4 * P, 0).map_err(e)?;
    kassert!(touch(&a, m, false) == Err(FaultReason::ProtNone), "PROT_NONE first");
    a.sys_mprotect(m, 4 * P, RW).map_err(e)?;
    guarded(&a, m + P, "mprotect keeps them")?;
    // Locked VMAs: no install (EINVAL), a remove is fine.
    let l = a.sys_mmap(BASE + 16 * P, 2 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(l, P, madv::GUARD_INSTALL).map_err(e)?;
    a.sys_mlock(l + P, P, 0, LockLimit { bytes: 0, capable: true }).map_err(e)?;
    // mlock's population meets the guard: GUP's EFAULT, answered as mlock's ENOMEM; the VMA is locked anyway.
    kassert!(a.sys_mlock(l, P, 0, LockLimit { bytes: 0, capable: true }) == Err(Errno::NoMem), "mlock over a guard");
    guarded(&a, l, "mlock does not fill a guard")?;
    kassert!(a.sys_madvise(l + P, P, madv::GUARD_INSTALL) == Err(Errno::Inval), "an install on a locked VMA");
    a.sys_madvise(l, P, madv::GUARD_REMOVE).map_err(e)?;
    touch(&a, l, false).map_err(|r| format!("after the remove: {r:?}"))?;
    Ok(())
}
ktest!("mm.as.guard_region_order", Safe, order);

/// fork copies the guards (not into a WIPEONFORK VMA, none into a DONTFORK one); mremap moves them.
fn carried() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 4 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    let w = a.sys_mmap(BASE + 8 * P, 2 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(w, 2 * P, madv::WIPEONFORK).map_err(e)?;
    a.sys_madvise(m + P, P, madv::GUARD_INSTALL).map_err(e)?;
    a.sys_madvise(w, P, madv::GUARD_INSTALL).map_err(e)?;
    let b = a.fork().map_err(e)?;
    guarded(&b, m + P, "in the child")?;
    touch(&b, w, true).map_err(|r| format!("the child's WIPEONFORK page: {r:?}"))?;
    guarded(&a, w, "the parent keeps its WIPEONFORK guard")?;
    drop(b);
    // mremap to another place: the guard comes along, the old place is free of it.
    let to = BASE + 64 * P;
    kassert!(a.sys_mremap(m, 4 * P, 4 * P, MREMAP_MAYMOVE | MREMAP_FIXED, to).map_err(e)? == to);
    guarded(&a, to + P, "moved by mremap")?;
    touch(&a, to, true).map_err(|r| format!("beside the moved guard: {r:?}"))?;
    a.sys_mmap(m, 4 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    touch(&a, m + P, true).map_err(|r| format!("the old place: {r:?}"))?;
    Ok(())
}
ktest!("mm.as.guard_region_fork", Safe, carried);

/// A guard keeps a 2 MiB from a huge page and a 64 KiB block from fault-around; the rest still gets them.
fn sizes() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 2 * HUGE, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(m + HUGE - P, P, madv::GUARD_INSTALL).map_err(e)?;
    touch(&a, m, true).map_err(|r| format!("first 2 MiB: {r:?}"))?;
    let leaf = paging::translate_leaf(a.root(), m).ok_or("no page at the start")?;
    kassert!(leaf.size == PageSize::Size4K, "a guarded 2 MiB took a huge page");
    touch(&a, m + HUGE - 16 * P, true).map_err(|r| format!("the guard's block: {r:?}"))?;
    kassert!(filled(&a, m + HUGE - 16 * P, m + HUGE) == [0], "fault-around filled the guard's block: {:?}", filled(&a, m + HUGE - 16 * P, m + HUGE));
    guarded(&a, m + HUGE - P, "after the faults around it")?;
    Ok(())
}
ktest!("mm.as.guard_region_sizes", Safe, sizes);

/// L7-RESTE: /proc/<pid>/pagemap's entries (`AsCore::pagemap`, linux-7.3.1/fs/proc/task_mmu.c:2018): 0 for no
/// VMA and for an empty page, PM_SWAP | PM_GUARD_REGION for a guard (frame 31 | 4 << 5 for a privileged reader),
/// PM_PRESENT | PM_MMAP_EXCLUSIVE for the space's own page (frame number only with `show_pfn`), no
/// PM_MMAP_EXCLUSIVE while a fork shares it; each 4 KiB of a 2 MiB page its own frame; `has_guard` per range.
fn pagemap() -> TestResult {
    const PRESENT: u64 = 1 << 63;
    const SWAP: u64 = 1 << 62;
    const GUARD: u64 = 1 << 58;
    const EXCL: u64 = 1 << 56;
    let map = |a: &AddressSpace, lo: u64, hi: u64, pfn: bool| -> Result<Vec<u64>, String> {
        let mut v = Vec::new();
        a.pagemap(lo, hi, pfn, &mut v).map_err(e)?;
        Ok(v)
    };
    let a = lazy()?;
    let m = a.sys_mmap(BASE + P, 4 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(m, 4 * P, madv::NOHUGEPAGE).map_err(e)?;
    a.write_u64(m, 1).map_err(e)?;
    a.sys_madvise(m + 2 * P, P, madv::GUARD_INSTALL).map_err(e)?;
    let v = map(&a, m - P, m + 4 * P, false)?;
    kassert!(v == [0, PRESENT | EXCL, 0, SWAP | GUARD, 0], "entries {v:x?}");
    let phys = paging::translate(a.root(), m).ok_or("no page")?.0;
    let v = map(&a, m, m + 3 * P, true)?;
    kassert!(v == [PRESENT | EXCL | phys >> 12, 0, SWAP | GUARD | 31 | 4 << 5], "with frames {v:x?}");
    kassert!(a.has_guard(m, m + 4 * P) && !a.has_guard(m, m + 2 * P) && !a.has_guard(m + 3 * P, m + 4 * P), "has_guard");
    let b = a.fork().map_err(e)?;
    kassert!(map(&a, m, m + P, false)? == [PRESENT], "shared with the child: {:x?}", map(&a, m, m + P, false)?);
    kassert!(map(&b, m + 2 * P, m + 3 * P, false)? == [SWAP | GUARD], "the child's guard");
    drop(b);
    kassert!(map(&a, m, m + P, false)? == [PRESENT | EXCL], "the child gone: {:x?}", map(&a, m, m + P, false)?);
    a.sys_madvise(m + 2 * P, P, madv::GUARD_REMOVE).map_err(e)?;
    kassert!(map(&a, m + 2 * P, m + 3 * P, false)? == [0] && !a.has_guard(m, m + 4 * P), "after the remove");
    // A 2 MiB page: every 4 KiB present and the space's own, frame numbers counting up from the block's.
    let _back = ModeBack(crate::thp::mode());
    crate::thp::set_mode(crate::thp::Mode::Always);
    crate::thp_ktest::room();
    let h = a.sys_mmap(BASE + 4 * HUGE, HUGE, RW, AP | MAP_FIXED, 0).map_err(e)?;
    touch(&a, h + 8, true).map_err(|r| format!("huge fault: {r:?}"))?;
    let leaf = paging::translate_leaf(a.root(), h).ok_or("no page in the 2 MiB")?;
    kassert!(leaf.size == PageSize::Size2M, "no 2 MiB page (no free block?)");
    let v = map(&a, h, h + HUGE, true)?;
    let head = leaf.phys & !(HUGE - 1);
    kassert!(v.len() == 512 && v.iter().enumerate().all(|(i, &x)| x == PRESENT | EXCL | (head >> 12) + i as u64), "2 MiB entries {:x?}", &v[..4]);
    device_pages(&a)
}
ktest!("mm.as.guard_region_pagemap", Safe, pagemap);

/// A device page is mapped but reads 0, also with frame numbers (VM_PFNMAP: `walk_page_test` gives it to
/// `pagemap_pte_hole`, linux-7.3.1/mm/pagewalk.c:396).
fn device_pages(a: &AddressSpace) -> TestResult {
    use crate::device::{Aperture, DeviceMap, DeviceOps, DevicePage};
    use crate::dma::{DmaBuffer, DmaConstraints};
    struct Dev;
    impl DeviceOps for Dev {
        fn fault(&self, pgoff: u64, _write: bool) -> Result<DevicePage, vfs::pagecache::PageFault> {
            Ok(DevicePage { aperture: 0, page: pgoff, writable: true })
        }
    }
    let Ok(f) = vfs::root_ns().open_flags(b"/dev/zero", vfs::abi::O_RDWR, 0) else { return ktest::fail("no /dev/zero to stand in for a device file") };
    let buf = DmaBuffer::alloc(2 * P as usize, DmaConstraints::dma64()).map_err(e)?;
    let dev = DeviceMap::new(alloc::sync::Arc::new(Dev), alloc::vec![Aperture::Ram(alloc::sync::Arc::new(buf))]).map_err(e)?;
    let d = a.sys_mmap_device(0, 2 * P, RW, crate::linux::MAP_SHARED, 0, &f, &dev).map_err(e)?;
    a.write_u64(d, 7).map_err(e)?;
    kassert!(paging::translate(a.root(), d).is_some(), "the device page is not mapped");
    for pfn in [false, true] {
        let mut v = Vec::new();
        a.pagemap(d, d + 2 * P, pfn, &mut v).map_err(e)?;
        kassert!(v == [0, 0], "device entries (frames {pfn}): {v:x?}");
    }
    a.sys_munmap(d, 2 * P).map_err(e)?;
    Ok(())
}

/// Restores the THP mode when the test ends, also on failure.
struct ModeBack(crate::thp::Mode);

impl Drop for ModeBack {
    fn drop(&mut self) {
        crate::thp::set_mode(self.0);
    }
}
