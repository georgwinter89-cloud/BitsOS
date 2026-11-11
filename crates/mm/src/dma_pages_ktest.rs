//! ktests of MM-14's pinned scattered DMA pages: allocation in blocks with the bookkeeping exact and nothing
//! leaked, WC/UC through the direct map and back, a page-list aperture mapped into programs (scattered frames
//! page by page, the object's type, fork, munmap while a device pin holds the pages, the end of a program), and
//! frames running out halfway (Safe), the limits (QemuOnly: the system's pin limit, an owner's share, the strict
//! commit), and a CPU that does not confirm a shootdown (QemuOnly, MM-14 fixer, named `mm.tlb.*_device`: an
//! unmap keeps the object until the address space drops; a leaked address space keeps it for good).

use crate::commit;
use crate::device::{Aperture, DeviceMap, DeviceOps, DevicePage};
use crate::dma::DmaCache;
use crate::dma_pages::{self, DmaPages, DmaPagesError, DmaPagesOptions, PinOwner};
use crate::linux::{Errno, MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::paging::{self, CacheType, PageFlags};
use ktest::{kassert, ktest, TestResult};
use mm_core::{boot_buddy, Kind, PAGE};
use vfs::pagecache::PageFault;

const P: u64 = PAGE;
const RW: u64 = 3;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// Frames DMA page sets hold now: exact even while USB and network drivers take DMA frames at boot (a balance
/// of `used_by_kind[Dma]` raced with them).
fn held() -> u64 {
    mm_core::dma_pages::live_frames()
}

fn get(pages: u64, o: DmaPagesOptions) -> Result<Arc<DmaPages>, String> {
    DmaPages::alloc(pages, o).map_err(|x| format!("{pages} DMA pages {o:?}: {x:?}"))
}

/// Every block of `p` is an allocated DMA block, the blocks cover the pages in order, none overlaps another.
fn check_blocks(p: &DmaPages) -> TestResult {
    let b = boot_buddy().ok_or("no buddy")?;
    let mut page = 0;
    let mut seen: Vec<(u64, u64)> = Vec::new();
    for i in 0..p.segments() {
        let (dev, len) = p.segment(i).ok_or("segment missing")?;
        let n = len / P;
        kassert!(n.is_power_of_two() && dev % len == 0, "block {i}: {dev:#x} {len:#x} not a buddy block");
        kassert!(b.run_allocated(dev, n, Kind::Dma), "block {i} at {dev:#x} not allocated as DMA");
        kassert!(p.device_addr(page) == Some(dev) && p.device_addr(page + n - 1) == Some(dev + len - P), "page lookup in block {i}");
        seen.push((dev, dev + len));
        page += n;
    }
    kassert!(page == p.pages() && p.device_addr(page).is_none(), "blocks cover {page} of {} pages", p.pages());
    seen.sort();
    kassert!(seen.windows(2).all(|w| w[0].1 <= w[1].0), "blocks overlap");
    Ok(())
}

fn alloc() -> TestResult {
    let (used, st) = (held(), dma_pages::stats());
    kassert!(DmaPages::alloc(0, DmaPagesOptions::default()).err() == Some(DmaPagesError::Invalid));
    // Hostile sizes: a byte size past u64 or past any physical space is EINVAL, more than the pin limit ENOMEM,
    // all before a frame or a charge is taken.
    for n in [u64::MAX / P + 1, u64::MAX, (1 << 52) / P + 1] {
        let x = DmaPages::alloc(n, DmaPagesOptions::default()).err();
        kassert!(x == Some(DmaPagesError::Invalid) && x.map(|x| x.errno()) == Some(uapi::errno::EINVAL), "{n} pages: {x:?}");
    }
    let over = dma_pages::limit() + 1;
    let x = DmaPages::alloc(over, DmaPagesOptions::default()).err();
    kassert!(x == Some(DmaPagesError::PinLimit) && x.map(|x| x.errno()) == Some(uapi::errno::ENOMEM), "{over} pages: {x:?}");
    kassert!(dma_pages::stats().pinned_pages == st.pinned_pages && held() == used, "a refusal took pins or frames");
    {
        let p = get(300, DmaPagesOptions::default())?;
        kassert!(p.pages() == 300 && p.len() == 300 * P && p.cache() == DmaCache::WriteBack);
        check_blocks(&p)?;
        let now = dma_pages::stats();
        kassert!(now.pinned_pages == st.pinned_pages + 300 && now.objects == st.objects + 1, "stats {now:?} after {st:?}");
        // Zeroed, and the CPU view spans blocks: a copy across the first block's end, typed values inside.
        let mut z = alloc::vec![0xFFu8; (300 * P) as usize];
        kassert!(p.read(0, &mut z) && z.iter().all(|&x| x == 0), "not zeroed");
        let first = p.segment(0).ok_or("no block")?.1;
        kassert!(p.write(first - 3, &[1, 2, 3, 4, 5, 6]));
        let mut t = [0u8; 6];
        kassert!(p.read(first - 3, &mut t) && t == [1, 2, 3, 4, 5, 6], "copy across blocks: {t:?}");
        kassert!(p.write_u64(299 * P + 8, 0x1122_3344_5566_7788) && p.read_u64(299 * P + 8) == Some(0x1122_3344_5566_7788));
        kassert!(p.write_u32(first, 0xC0FF_EE00) && p.read_u32(first) == Some(0xC0FF_EE00));
        kassert!(p.read_u32(2).is_none() && p.read_u64(300 * P).is_none() && !p.write_u32(300 * P, 1), "misaligned or outside");
        kassert!(!p.read(300 * P - 2, &mut t) && !p.write(u64::MAX - 1, &t) && !p.fill(300 * P, 1, 0), "range past the end");
        kassert!(p.fill(10, 20, 0xAB) && p.read(10, &mut t) && t == [0xAB; 6]);
        // Single frames only: as many blocks as pages, every one its own.
        let s = get(64, DmaPagesOptions::default().with_max_order(0))?;
        kassert!(s.segments() == 64, "{} blocks", s.segments());
        check_blocks(&s)?;
    }
    let now = dma_pages::stats();
    kassert!(now.pinned_pages == st.pinned_pages && now.objects == st.objects, "stats {now:?}, before {st:?}");
    kassert!(held() == used, "DMA page frames leaked: {} -> {}", used, held());
    Ok(())
}
ktest!("mm.dmapages.alloc", Safe, alloc);

fn wc_uc() -> TestResult {
    let root = paging::kernel_root().ok_or("kernel still on the loader's tables")?;
    let used = held();
    for (cache, want) in [(DmaCache::WriteCombining, CacheType::WriteCombining), (DmaCache::Uncached, CacheType::Uncached)] {
        let p = get(520, DmaPagesOptions::default().with_cache(cache))?;
        let blocks: Vec<(u64, u64)> = (0..p.segments()).filter_map(|i| p.segment(i)).collect();
        for &(pa, len) in &blocks {
            for off in (0..len).step_by(P as usize) {
                let l = paging::translate_leaf(root, hal::phys_to_virt(pa + off)).ok_or("block page unmapped")?;
                kassert!(l.flags.cache() == Some(want) && l.phys == pa + off, "{cache:?} block {pa:#x}+{off:#x}: {:?}", l.flags.cache());
            }
        }
        kassert!(dma_pages::stats().retyped_pages >= 520);
        kassert!(p.write_u32(519 * P, 0xDEAD_BEEF) && p.read_u32(519 * P) == Some(0xDEAD_BEEF));
        p.write_barrier();
        drop(p);
        for &(pa, _) in &blocks {
            if let Some(l) = paging::translate_leaf(root, hal::phys_to_virt(pa)) {
                kassert!(l.flags.cache() == Some(CacheType::WriteBack), "{cache:?} block {pa:#x} not write-back after the drop");
            }
        }
    }
    kassert!(held() == used, "DMA page frames leaked: {used} -> {}, kept untyped {}, kept with interrupts off {}", held(),
        mm_core::dma::KEPT_UNTYPED.load(core::sync::atomic::Ordering::Relaxed), dma_pages::stats().kept_irq_off);
    Ok(())
}
ktest!("mm.dmapages.wc", Safe, wc_uc);

/// Page `pgoff` of the mapping is page `pgoff` of aperture `ap`, writable; answers past the aperture on purpose
/// (MM's bound check must turn them into SIGBUS).
struct ListDev {
    ap: usize,
}

impl DeviceOps for ListDev {
    fn fault(&self, pgoff: u64, _write: bool) -> Result<DevicePage, PageFault> {
        Ok(DevicePage { aperture: self.ap, page: pgoff, writable: true })
    }
}

fn leaf(a: &AddressSpace, va: u64) -> Option<(u64, PageFlags)> {
    paging::translate(a.root(), va)
}

fn map() -> TestResult {
    let ns = vfs::root_ns();
    let Ok(f) = ns.open_flags(b"/dev/zero", vfs::abi::O_RDWR, 0) else { return ktest::fail("no /dev/zero to stand in for a device file") };
    let (used, st) = (held(), dma_pages::stats());
    const N: u64 = 40;
    let p = get(N, DmaPagesOptions::default().with_max_order(0))?;
    let wc = get(2, DmaPagesOptions::default().with_cache(DmaCache::WriteCombining))?;
    let frames: Vec<u64> = (0..N).filter_map(|i| p.page_phys(i)).collect();
    kassert!(frames.len() == N as usize);
    let pin = {
        let map = DeviceMap::new(Arc::new(ListDev { ap: 0 }), alloc::vec![Aperture::Pages(p.clone())]).map_err(e)?;
        let wmap = DeviceMap::new(Arc::new(ListDev { ap: 0 }), alloc::vec![Aperture::Pages(wc.clone())]).map_err(e)?;
        let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
        let m = a.sys_mmap_device(0, (N + 2) * P, RW, MAP_SHARED, 0, &f, &map).map_err(e)?;
        kassert!(a.committed_pages() == 0, "the mapping itself is not charged (the pages are, once)");
        for i in 0..N {
            kassert!(p.write_u64(i * P + 16, 0x5000 + i));
        }
        for i in 0..N {
            kassert!(a.read_u64(m + i * P + 16).map_err(e)? == 0x5000 + i, "page {i} reads the object's page");
            let (pa, fl) = leaf(&a, m + i * P).ok_or("page not mapped")?;
            kassert!(pa == frames[i as usize] && fl.cache() == Some(CacheType::WriteBack), "page {i}: {pa:#x} {:?}", fl.cache());
        }
        a.write_u64(m + 7 * P, 0x77).map_err(e)?;
        kassert!(p.read_u64(7 * P) == Some(0x77), "the program's store reached the object");
        kassert!(a.handle_user_fault(m + N * P, 4) == Err(crate::FaultReason::Bus), "a page past the object is SIGBUS");
        kassert!(leaf(&a, m + N * P).is_none());
        // A mapping whose offset starts past the page list: mmap succeeds (Linux checks the range at fault time
        // for a hook-backed mapping), its first touch is SIGBUS.
        let past = a.sys_mmap_device(0, P, RW, MAP_SHARED, (N + 5) * P, &f, &map).map_err(e)?;
        kassert!(a.handle_user_fault(past, 4) == Err(crate::FaultReason::Bus) && leaf(&a, past).is_none(), "offset past the object");
        let w = a.sys_mmap_device(0, 2 * P, RW, MAP_SHARED, 0, &f, &wmap).map_err(e)?;
        a.write_u64(w + P, 9).map_err(e)?;
        kassert!(leaf(&a, w + P).is_some_and(|(pa, fl)| Some(pa) == wc.page_phys(1) && fl.cache() == Some(CacheType::WriteCombining)), "WC object mapped WC");
        // fork: the child faults in the same frames, and keeps them when the parent goes.
        let b = a.fork().map_err(e)?;
        kassert!(b.read_u64(m + 7 * P).map_err(e)? == 0x77 && leaf(&b, m + 7 * P).map(|x| x.0) == Some(frames[7]));
        kassert!(map.mapped_by() == 2, "{} spaces map the object", map.mapped_by());
        // The device got the addresses; the parent unmaps, then dies; the child dies without unmapping.
        let pin = p.pin_for_device();
        kassert!(p.device_pins() == 1 && dma_pages::stats().device_pins == st.device_pins + 1);
        a.sys_munmap(m, (N + 2) * P).map_err(e)?;
        kassert!(leaf(&a, m).is_none() && leaf(&a, m + 7 * P).is_none(), "munmap left a translation");
        drop(a);
        kassert!(b.read_u64(m + 16).map_err(e)? == 0x5000, "the child still reads the pages");
        drop(b);
        kassert!(map.mapped_by() == 0);
        pin
    };
    drop(p);
    drop(wc);
    kassert!(Arc::strong_count(pin.pages()) == 1, "something but the pin still holds the object");
    let b = boot_buddy().ok_or("no buddy")?;
    kassert!(frames.iter().all(|&fr| b.run_allocated(fr, 1, Kind::Dma)), "a pinned frame was freed");
    kassert!(pin.pages().read_u64(7 * P) == Some(0x77), "the pinned pages kept their contents");
    kassert!(dma_pages::stats().objects == st.objects + 1);
    drop(pin);
    let now = dma_pages::stats();
    kassert!(now.pinned_pages == st.pinned_pages && now.objects == st.objects && now.device_pins == st.device_pins, "{now:?} vs {st:?}");
    kassert!(held() == used, "DMA page frames leaked: {used} -> {}", held());
    Ok(())
}
ktest!("mm.dmapages.map", Safe, map);

/// Frames running out halfway (WB and WC, Safe and rerunnable: only this allocation is affected, for one call):
/// everything taken goes back, retyped blocks back to WB, every charge too.
fn fail_halfway() -> TestResult {
    let (used, st) = (held(), dma_pages::stats());
    for cache in [DmaCache::WriteBack, DmaCache::WriteCombining] {
        let kept = mm_core::dma::KEPT_UNTYPED.load(core::sync::atomic::Ordering::Relaxed);
        dma_pages::test_fail_after(Some(10));
        let x = DmaPages::alloc(300, DmaPagesOptions::default().with_cache(cache)).err();
        dma_pages::test_fail_after(None);
        kassert!(x == Some(DmaPagesError::NoMemory) && x.map(|x| x.errno()) == Some(uapi::errno::ENOMEM), "{cache:?}: {x:?}");
        kassert!(mm_core::dma::KEPT_UNTYPED.load(core::sync::atomic::Ordering::Relaxed) == kept, "frames kept untyped");
        let now = dma_pages::stats();
        kassert!(now.pinned_pages == st.pinned_pages && now.objects == st.objects && now.retyped_pages == st.retyped_pages, "{cache:?}: {now:?} vs {st:?}");
        kassert!(held() == used, "{cache:?}: frames left after the failure: {used} -> {}", held());
    }
    Ok(())
}
ktest!("mm.dmapages.fail_halfway", Safe, fail_halfway);

/// QemuOnly: it lowers the system's pin and commit limits for a moment (a GPU driver or program could be
/// refused meanwhile).
fn limits() -> TestResult {
    let (used, st, c0) = (held(), dma_pages::stats(), commit::committed());
    let r = (|| -> TestResult {
        // The system's pin limit: all or nothing, ENOMEM, room again after the drop.
        dma_pages::set_test_limit(Some(st.pinned_pages + 100));
        let x = DmaPages::alloc(101, DmaPagesOptions::default()).err();
        kassert!(x == Some(DmaPagesError::PinLimit) && x.map(|x| x.errno()) == Some(uapi::errno::ENOMEM), "{x:?}");
        let p = get(100, DmaPagesOptions::default())?;
        kassert!(DmaPages::alloc(1, DmaPagesOptions::default()).err() == Some(DmaPagesError::PinLimit));
        drop(p);
        kassert!(get(1, DmaPagesOptions::default()).is_ok(), "room after the drop");
        dma_pages::set_test_limit(None);
        kassert!(dma_pages::limit() > st.pinned_pages + 100, "the share again: {}", dma_pages::limit());
        // An owner's share (1 % of RAM): refused before a frame is taken.
        let w = PinOwner::new(1);
        let lim = w.limit();
        kassert!(lim > 0 && lim < dma_pages::limit());
        kassert!(DmaPages::alloc_for(lim + 1, DmaPagesOptions::default(), Some(&w)).err() == Some(DmaPagesError::OwnerLimit));
        let q = DmaPages::alloc_for(8, DmaPagesOptions::default(), Some(&w)).map_err(e)?;
        kassert!(w.pinned() == 8);
        drop(q);
        kassert!(w.pinned() == 0);
        // The strict commit: pins take promised memory, so a program's private memory gets ENOMEM while they live.
        let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
        commit::set_test_limit(Some(commit::committed() + 64));
        let x = DmaPages::alloc(65, DmaPagesOptions::default()).err();
        kassert!(x == Some(DmaPagesError::Commit), "{x:?}");
        kassert!(dma_pages::stats().pinned_pages == st.pinned_pages, "a refused commit left a pin charge");
        let p = get(64, DmaPagesOptions::default())?;
        kassert!(a.sys_mmap(0, P, RW, MAP_ANONYMOUS | MAP_PRIVATE, 0) == Err(Errno::NoMem), "the pins hold the commit");
        drop(p);
        kassert!(a.sys_mmap(0, P, RW, MAP_ANONYMOUS | MAP_PRIVATE, 0).is_ok(), "free again after the drop");
        commit::set_test_limit(None);
        drop(a);
        Ok(())
    })();
    dma_pages::set_test_limit(None);
    commit::set_test_limit(None);
    r?;
    let now = dma_pages::stats();
    kassert!(now.pinned_pages == st.pinned_pages && now.objects == st.objects, "{now:?} vs {st:?}");
    kassert!(held() == used && commit::committed() == c0, "leak: dma {used} -> {}, commit {c0} -> {}", held(), commit::committed());
    Ok(())
}
ktest!("mm.dmapages.limits", QemuOnly, limits);

/// Waits (bounded) until no CPU is marked stuck any more, so the next change is confirmed again.
fn wait_unstuck() {
    let end = time::now_ns() + 5_000_000_000;
    while paging::tlb::stats().stuck_cpus != 0 && time::now_ns() < end {
        sched::sleep_ns(1_000_000);
    }
}

// The two tests below run under `mm.tlb.` (after the DMA tests that balance `dma_used`): their holds stall USB
// enumeration at boot, which then takes DMA frames while those tests run.

/// QemuOnly (holds an AP with interrupts off): CPU c keeps the space's root loaded while the program unmaps a
/// device mapping of DMA pages, so the shootdown is not confirmed; then every other reference goes. The object
/// must live on until the address space drops (a CPU may still hold a translation), and go back then.
fn give_up_device() -> TestResult {
    let Some(&c) = crate::tlb_ktest::aps().last() else { return ktest::fail("one CPU") };
    let Ok(f) = vfs::root_ns().open_flags(b"/dev/zero", vfs::abi::O_RDWR, 0) else { return ktest::fail("no /dev/zero to stand in for a device file") };
    let _cpu0 = crate::tlb_ktest::OnCpu0::take()?;
    let (used, st, unconfirmed0) = (held(), dma_pages::stats(), crate::aspace::UNCONFIRMED.get());
    let kept0 = mm_core::dma::KEPT_UNTYPED.load(core::sync::atomic::Ordering::Relaxed);
    {
        let p = get(8, DmaPagesOptions::default().with_cache(DmaCache::WriteCombining).with_max_order(0))?;
        let weak = Arc::downgrade(&p);
        let map = DeviceMap::new(Arc::new(ListDev { ap: 0 }), alloc::vec![Aperture::Pages(p)]).map_err(e)?;
        let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
        let m = a.sys_mmap_device(0, 8 * P, RW, MAP_SHARED, 0, &f, &map).map_err(e)?;
        for i in 0..8 {
            a.write_u64(m + i * P, i).map_err(e)?;
        }
        let h = crate::tlb_ktest::hold(c, a.root())?;
        let r = a.sys_munmap(m, 8 * P);
        let held = crate::tlb_ktest::release(h)?;
        r.map_err(e)?;
        kassert!(crate::aspace::UNCONFIRMED.get() > unconfirmed0, "the unmap was confirmed (hold {} ms)", held / 1_000_000);
        kassert!(leaf(&a, m).is_none(), "munmap left a translation");
        drop(map);
        kassert!(weak.upgrade().is_some(), "the pages went while a CPU may still hold their translation");
        kassert!(dma_pages::stats().objects == st.objects + 1);
        wait_unstuck();
        drop(a);
        kassert!(weak.upgrade().is_none(), "the pages outlived their address space");
    }
    let now = dma_pages::stats();
    kassert!(now.pinned_pages == st.pinned_pages && now.objects == st.objects && now.retyped_pages == st.retyped_pages, "{now:?} vs {st:?}");
    kassert!(mm_core::dma::KEPT_UNTYPED.load(core::sync::atomic::Ordering::Relaxed) == kept0 && now.kept_irq_off == st.kept_irq_off, "frames kept instead of freed");
    kassert!(held() == used, "DMA page frames leaked: {used} -> {}", held());
    Ok(())
}
ktest!("mm.tlb.give_up_device", QemuOnly, give_up_device);

/// QemuOnly (leaks an address space and 4 write-back DMA pages for the rest of the boot on purpose): CPU c keeps the root
/// loaded while the address space drops, so it is leaked; its device mapping's pages must be kept for good
/// (frames still allocated, the object alive), not freed or retyped under tables a CPU may still walk.
fn leaked_space_keeps_device() -> TestResult {
    let Some(&c) = crate::tlb_ktest::aps().last() else { return ktest::fail("one CPU") };
    let Ok(f) = vfs::root_ns().open_flags(b"/dev/zero", vfs::abi::O_RDWR, 0) else { return ktest::fail("no /dev/zero to stand in for a device file") };
    let _cpu0 = crate::tlb_ktest::OnCpu0::take()?;
    let (leaked0, kept0) = (crate::aspace::AS_LEAKED.get(), crate::aspace::DEVICE_KEPT.get());
    // Write-back: kept WC pages would leave the direct map WC for the rest of the boot (mm.paging.direct_map).
    let p = get(4, DmaPagesOptions::default().with_max_order(0))?;
    let weak = Arc::downgrade(&p);
    let frames: Vec<u64> = (0..4).filter_map(|i| p.page_phys(i)).collect();
    let map = DeviceMap::new(Arc::new(ListDev { ap: 0 }), alloc::vec![Aperture::Pages(p)]).map_err(e)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = a.sys_mmap_device(0, 4 * P, RW, MAP_SHARED, 0, &f, &map).map_err(e)?;
    for i in 0..4 {
        a.write_u64(m + i * P, 0x100 + i).map_err(e)?;
    }
    drop(map);
    let h = crate::tlb_ktest::hold(c, a.root())?;
    drop(a);
    let held = crate::tlb_ktest::release(h)?;
    wait_unstuck();
    kassert!(crate::aspace::AS_LEAKED.get() == leaked0 + 1, "the address space was not leaked (hold {} ms)", held / 1_000_000);
    kassert!(crate::aspace::DEVICE_KEPT.get() > kept0, "no device backing kept");
    let kept = weak.upgrade().ok_or("the leaked space's device pages were freed")?;
    let b = boot_buddy().ok_or("no buddy")?;
    kassert!(frames.len() == 4 && frames.iter().all(|&fr| b.run_allocated(fr, 1, Kind::Dma)), "a kept frame was freed");
    kassert!(kept.read_u64(3 * P) == Some(0x103), "the kept pages changed");
    klog::event!(Info, "mm", "ktest.dmapages_leaked_space", cpu = c, held_ms = held / 1_000_000; "device pages of a leaked address space kept");
    Ok(())
}
ktest!("mm.tlb.leaked_device", QemuOnly, leaked_space_keeps_device);
