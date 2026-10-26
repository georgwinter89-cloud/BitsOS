//! ktests of the kernel's own page tables and the vmap allocator (MM-2): the switch, W^X and NX through
//! deliberate faults, the direct map against RAM and the MTRRs, WC/UC windows with their PAT bits, guard
//! pages, unmapping that gives page-table frames back, and the framebuffer's WC write speed (judged on the
//! Mini-PC only).

use crate::kernel_tables;
use crate::mtrr::Mtrrs;
use crate::vmap;
use alloc::string::String;
use alloc::vec::Vec;
use bootinfo::layout::{DIRECT_MAP_BASE, VMAP_BASE};
use core::sync::atomic::{AtomicU8, Ordering};
use hal::paging::probe::{self, Probe, PF_FETCH, PF_PRESENT, PF_WRITE};
use hal::paging::{self, CacheType, PageFlags, PageSize, VmapError};
use ktest::{kassert, ktest, TestResult};

/// The kernel root. A kernel thread may still hold a process's root lazily (MM-6): the MCP ktest thread on the
/// Mini-PC after init ran on its CPU. The kernel half is shared, so that counts as running on the kernel root.
fn root() -> Result<u64, String> {
    let r = paging::kernel_root().ok_or("the kernel still runs on the loader's tables")?;
    let on_kernel = paging::current_root() == r || (paging::tlb::lazy_here() && paging::tlb::thread_root() == r);
    kassert!(on_kernel, "loaded root {:#x} is not the kernel root {r:#x}", paging::current_root());
    Ok(r)
}

fn switched() -> TestResult {
    root()?;
    let rep = kernel_tables::report().ok_or("no switch report")?;
    kassert!(rep.wx_pages == 0, "{} image pages writable and executable", rep.wx_pages);
    kassert!(rep.ram_bytes > 0 && rep.image_pages > 0);
    kassert!(rep.ram_bytes == kernel_tables::ram().iter().map(|r| r.1 - r.0).sum::<u64>(), "direct map != RAM");
    kassert!(vmap::stats().is_some(), "vmap allocator not installed");
    kassert!(paging::device_mapper(), "hal device mapper not installed");
    if hal::cpu::detect().pdpe1gb && rep.ram_bytes >= 3 << 30 {
        kassert!(rep.pages_1g > 0, "no 1 GiB page in {} MiB of RAM", rep.ram_bytes >> 20);
    }
    Ok(())
}
ktest!("mm.paging.switched", Safe, switched);

#[inline(never)]
fn text_target() -> u64 {
    0x5EED
}
/// A `ret` in .rodata and one in .data: executing either must fault (NX); were NX broken, it would return.
static RET_RO: [u8; 1] = [0xC3];
static RET_RW: AtomicU8 = AtomicU8::new(0xC3);

fn faulted(p: Probe, want: u64, what: &str) -> TestResult {
    match p {
        Probe::Faulted { error, .. } if error & want == want => Ok(()),
        other => Err(alloc::format!("{what}: {other:?} (wanted a fault with error bits {want:#x})")),
    }
}

fn wx() -> TestResult {
    let r = root()?;
    let text = text_target as fn() -> u64 as usize as u64;
    kassert!(text_target() == 0x5EED);
    let ro = RET_RO.as_ptr() as u64;
    let rw = &RET_RW as *const AtomicU8 as u64;
    kassert!(RET_RW.load(Ordering::Relaxed) == 0xC3);
    let fl = |va: u64| paging::translate(r, va).map(|t| t.1).ok_or_else(|| alloc::format!("{va:#x} unmapped"));
    let alias = |va: u64| paging::translate(r, va).map(|t| DIRECT_MAP_BASE + t.0).ok_or_else(|| alloc::format!("{va:#x} unmapped"));
    let (t, o, w) = (fl(text)?, fl(ro)?, fl(rw)?);
    kassert!(!t.contains(PageFlags::WRITABLE) && !t.contains(PageFlags::NX) && t.contains(PageFlags::GLOBAL), "text flags {:#x}", t.0);
    kassert!(!o.contains(PageFlags::WRITABLE) && o.contains(PageFlags::NX), "rodata flags {:#x}", o.0);
    kassert!(w.contains(PageFlags::WRITABLE | PageFlags::NX), "data flags {:#x}", w.0);
    let p = |r: Result<Probe, probe::Refused>| r.map_err(|e| alloc::format!("probe refused: {e:?}"));
    faulted(p(probe::write_same(text))?, PF_PRESENT | PF_WRITE, "write to text")?;
    faulted(p(probe::write_same(ro))?, PF_PRESENT | PF_WRITE, "write to rodata")?;
    faulted(p(probe::exec(ro))?, PF_PRESENT | PF_FETCH, "execute rodata")?;
    faulted(p(probe::exec(rw))?, PF_PRESENT | PF_FETCH, "execute data")?;
    kassert!(p(probe::write_same(rw))? == Probe::WritableNotProbed);
    // The direct-map aliases: text and rodata read-only there too, nothing executable.
    let (ta, oa, wa) = (alias(text)?, alias(ro)?, alias(rw)?);
    kassert!(!fl(ta)?.contains(PageFlags::WRITABLE) && fl(ta)?.contains(PageFlags::NX), "text alias flags {:#x}", fl(ta)?.0);
    faulted(p(probe::write_same(ta))?, PF_PRESENT | PF_WRITE, "write to the text alias")?;
    faulted(p(probe::exec(oa))?, PF_PRESENT | PF_FETCH, "execute the rodata alias")?;
    faulted(p(probe::exec(wa))?, PF_PRESENT | PF_FETCH, "execute the data alias")?;
    // No image page both writable and executable.
    let (s, e) = paging::kernel_image();
    let mut va = s & !0xFFF;
    let (mut x, mut ronly) = (0, 0);
    while va < e {
        let f = fl(va)?;
        kassert!(!(f.contains(PageFlags::WRITABLE) && !f.contains(PageFlags::NX)), "image page {va:#x} is W+X");
        kassert!(f.contains(PageFlags::GLOBAL), "image page {va:#x} not global");
        x += !f.contains(PageFlags::NX) as u32;
        ronly += !f.contains(PageFlags::WRITABLE) as u32;
        va += 0x1000;
    }
    klog::event!(Info, "mm", "ktest.wx", exec_pages = x, read_only_pages = ronly, image_kb = (e - s) >> 10);
    Ok(())
}
ktest!("mm.paging.wx", Safe, wx);

fn mtrrs() -> Mtrrs {
    hal::pat::read_mtrrs().as_ref().map_or_else(Mtrrs::none, Mtrrs::from_regs)
}

fn direct_map() -> TestResult {
    let r = root()?;
    let ram = kernel_tables::ram();
    kassert!(!ram.is_empty());
    let m = mtrrs();
    let inside = |s: u64, e: u64| {
        let i = ram.partition_point(|x| x.1 <= s);
        i < ram.len() && ram[i].0 <= s && e <= ram[i].1
    };
    let mut leaves = Vec::new();
    paging::for_each_leaf(r, DIRECT_MAP_BASE, VMAP_BASE, &mut |u| leaves.push(u));
    let mut total = 0;
    let mut sizes = [0u64; 3];
    for u in &leaves {
        let (p, n) = (u.virt - DIRECT_MAP_BASE, u.size.bytes());
        kassert!(u.phys == p, "direct map {:#x} -> {:#x}", u.virt, u.phys);
        kassert!(inside(p, p + n), "leaf {p:#x}+{n:#x} not all RAM");
        kassert!(m.block_type(p, n).is_some(), "leaf {p:#x}+{n:#x} crosses an MTRR type boundary");
        kassert!(u.flags.contains(PageFlags::NX | PageFlags::GLOBAL) && !u.flags.contains(PageFlags::USER));
        kassert!(u.flags.cache() == Some(CacheType::WriteBack));
        total += n;
        sizes[u.size as usize] += 1;
    }
    kassert!(total == ram.iter().map(|x| x.1 - x.0).sum::<u64>(), "direct map covers {total:#x} bytes");
    // The highest RAM page and the edges of every range translate to themselves.
    for &(s, e) in &ram {
        for p in [s, e - 0x1000, s + (((e - s) / 2) & !0xFFF)] {
            let l = paging::translate_leaf(r, DIRECT_MAP_BASE + p).ok_or_else(|| alloc::format!("RAM {p:#x} not in the direct map"))?;
            kassert!(l.phys == p, "RAM {p:#x} -> {:#x}", l.phys);
        }
    }
    let top = ram.last().map_or(0, |x| x.1);
    let past = paging::translate(r, DIRECT_MAP_BASE + top);
    kassert!(past.is_none() || ram.iter().any(|x| x.0 <= top && top < x.1), "the direct map runs past RAM");
    klog::event!(Info, "mm", "ktest.direct_map", ram_mb = total >> 20, top = klog::Hex(top), above_4g_mb = ram.iter().map(|x| x.1.saturating_sub(x.0.max(1 << 32))).sum::<u64>() >> 20,
        pages_4k = sizes[PageSize::Size4K as usize], pages_2m = sizes[PageSize::Size2M as usize], pages_1g = sizes[PageSize::Size1G as usize]);
    Ok(())
}
ktest!("mm.paging.direct_map", Safe, direct_map);

/// HPET's register page: device memory on q35 and on the Mini-PC's FCH; mapped UC but never touched here.
const HPET: u64 = 0xFED0_0000;

fn wc_and_uc_windows() -> TestResult {
    let r = root()?;
    let pat = hal::pat::read_pat();
    kassert!(hal::pat::pat_entry(pat, 1) == hal::pat::MT_WC, "PAT entry 1 is {} (PAT {pat:#x})", hal::pat::pat_entry(pat, 1));
    for (c, bits) in [(CacheType::Uncached, PageFlags::PCD | PageFlags::PWT), (CacheType::UncachedMinus, PageFlags::PCD)] {
        let w = vmap::map_device(HPET, 0x400, c).map_err(|e| alloc::format!("{c:?} window: {e:?}"))?;
        let (pa, f) = paging::translate(r, w.virt()).ok_or("window unmapped")?;
        kassert!(pa == HPET && f.0 & (PageFlags::PCD | PageFlags::PWT | PageFlags::PAT_4K) == bits, "{c:?}: flags {:#x}", f.0);
        kassert!(f.contains(PageFlags::GLOBAL | PageFlags::NX | PageFlags::WRITABLE));
    }
    let Some((fb_virt, fb_phys, fb_size)) = kernel_tables::framebuffer() else {
        return ktest::skip("no GOP framebuffer: the WC part needs one");
    };
    // The framebuffer at the loader's address, mapped anew by MM: WC, global, 2 MiB pages where aligned.
    let fl = paging::translate_leaf(r, fb_virt).ok_or("framebuffer mapping lost in the switch")?;
    kassert!(fl.phys == fb_phys && fl.flags.cache() == Some(CacheType::WriteCombining), "framebuffer flags {:#x}", fl.flags.0);
    kassert!(fl.flags.contains(PageFlags::GLOBAL | PageFlags::NX | PageFlags::WRITABLE));
    let aligned = |p: u64| p.is_multiple_of(2 << 20) && mtrrs().block_type(p, 2 << 20).is_some();
    kassert!(!(aligned(fb_phys) && fb_virt % (2 << 20) == 0 && fb_size >= 2 << 20) || fl.size == PageSize::Size2M, "framebuffer not in 2 MiB pages");
    kassert!(!vmap::is_free(fb_virt), "the framebuffer's range is free in the allocator");
    // A new WC window on it (same type, so the alias is allowed), through hal's device-window path.
    let len = fb_size.min(8 << 20);
    let w = paging::map_device_window(fb_phys, len, CacheType::WriteCombining).map_err(|e| alloc::format!("WC window: {e:?}"))?;
    let l = paging::translate_leaf(r, w.virt()).ok_or("WC window unmapped")?;
    kassert!(l.flags.0 & (PageFlags::PCD | PageFlags::PWT | PageFlags::PAT_4K) == PageFlags::PWT, "WC flags {:#x}", l.flags.0);
    kassert!(!(aligned(fb_phys) && len >= 2 << 20) || l.size == PageSize::Size2M, "an aligned WC framebuffer window should use 2 MiB pages");
    klog::event!(Info, "mm", "ktest.wc_window", phys = klog::Hex(fb_phys), virt = klog::Hex(w.virt()), len = len, page = alloc::format!("{:?}", l.size));
    Ok(())
}
ktest!("mm.vmap.wc_uc_pat", Safe, wc_and_uc_windows);

fn guard_pages() -> TestResult {
    root()?;
    let a = vmap::map_device(HPET, 0x1000, CacheType::Uncached).map_err(|e| alloc::format!("{e:?}"))?;
    let b = vmap::map_device(HPET, 0x1000, CacheType::Uncached).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(a.virt() != b.virt());
    for (what, addr) in [("guard below a", a.guard_below()), ("page after a", a.page_after()), ("guard below b", b.guard_below())] {
        match probe::read_unmapped(addr) {
            Ok(Probe::Faulted { addr: cr2, error }) => {
                kassert!(cr2 == addr && error & PF_PRESENT == 0, "{what}: fault at {cr2:#x}, error {error:#x}");
            }
            other => return Err(alloc::format!("{what} {addr:#x}: {other:?}")),
        }
    }
    let gone = a.virt();
    let quarantined = vmap::stats().ok_or("no vmap")?.quarantined_windows;
    drop(a);
    // MM-6: unmapped on every CPU (shootdown), not quarantined, also with the APs online.
    let now = vmap::stats().ok_or("no vmap")?.quarantined_windows;
    kassert!(now == quarantined, "dropped window quarantined: {quarantined} -> {now}");
    kassert!(paging::translate(paging::current_root(), gone).is_none(), "dropped window still mapped");
    kassert!(matches!(probe::read_unmapped(gone), Ok(Probe::Faulted { .. })), "access to a dropped window must fault");
    Ok(())
}
ktest!("mm.vmap.guard_fault", Safe, guard_pages);

fn table_frames() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::PageTable as usize])
}

fn unmap_frees_tables() -> TestResult {
    root()?;
    let before = (table_frames(), vmap::stats().ok_or("no vmap")?);
    let mut v = Vec::new();
    for _ in 0..600 {
        v.push(vmap::map_device(HPET, 0x1000, CacheType::Uncached).map_err(|e| alloc::format!("{e:?}"))?);
    }
    let mid = table_frames();
    kassert!(mid > before.0, "600 windows took no page-table frame");
    let mut virts: Vec<u64> = v.iter().map(|w| w.virt()).collect();
    virts.sort_unstable();
    kassert!(virts.windows(2).all(|w| w[1] - w[0] >= 0x2000), "two windows without a guard page between");
    drop(v);
    let after = (table_frames(), vmap::stats().ok_or("no vmap")?);
    kassert!(after.0 == before.0, "page-table frames {} before, {} after (peak {mid})", before.0 >> 12, after.0 >> 12);
    kassert!(after.1.free_bytes == before.1.free_bytes && after.1.windows == before.1.windows, "vmap space not given back");
    kassert!(after.1.pages_4k == before.1.pages_4k && after.1.pages_2m == before.1.pages_2m,
        "page counters {}/{} before, {}/{} after", before.1.pages_4k, before.1.pages_2m, after.1.pages_4k, after.1.pages_2m);
    kassert!(after.1.quarantined_windows == 0, "{} windows quarantined", after.1.quarantined_windows);
    klog::event!(Info, "mm", "ktest.vmap_tables", peak_tables = (mid - before.0) >> 12, free_ranges = after.1.free_ranges);
    Ok(())
}
ktest!("mm.vmap.unmap_frees_tables", Safe, unmap_frees_tables);

fn refusals() -> TestResult {
    root()?;
    let ram = kernel_tables::ram();
    let p = ram.last().map(|x| x.1 - 0x1000).ok_or("no RAM")?;
    kassert!(matches!(vmap::map_device(p, 0x1000, CacheType::WriteCombining), Err(VmapError::OverlapsRam)), "a WC window over RAM");
    kassert!(matches!(vmap::map_device(HPET, 0, CacheType::Uncached), Err(VmapError::BadRange)));
    kassert!(matches!(vmap::map_device(u64::MAX - 0x10, 0x100, CacheType::Uncached), Err(VmapError::BadRange)));
    Ok(())
}
ktest!("mm.vmap.refusals", Safe, refusals);

/// Rewrites the bottom MiB of the screen with its own pixels through the WC mapping the console uses and
/// reports the time per MiB. QEMU only proves it works; the Mini-PC baseline is 0.10-0.13 ms/MB (25x UC).
fn fb_wc_write_speed() -> TestResult {
    let Some((virt, phys, size)) = kernel_tables::framebuffer() else {
        return ktest::skip("no GOP framebuffer");
    };
    let fb = bootinfo::Framebuffer {
        phys, virt, size, width: 0, height: 0, stride: 0, format: bootinfo::PixelFormat::Bgrx, masks: [0; 4], mode_index: 0,
    };
    let io = hal::mmio::framebuffer(&fb).ok_or("framebuffer window")?;
    let n = (1usize << 20).min(size as usize) & !3;
    let off = (size as usize - n) & !3;
    // Read once (slow on hardware, ~0.1 s), so the timed writes put back what is on the screen.
    let mut buf = Vec::with_capacity(n);
    for i in (0..n).step_by(4) {
        buf.extend_from_slice(&io.read32(off + i).map_err(|e| alloc::format!("{e:?}"))?.to_le_bytes());
    }
    let mut best = u64::MAX;
    for _ in 0..5 {
        let t0 = time::now_ns();
        io.write_bytes(off, &buf).map_err(|e| alloc::format!("{e:?}"))?;
        best = best.min(time::now_ns() - t0);
    }
    let per_mb = best * (1 << 20) / n as u64;
    klog::event!(Info, "mm", "ktest.fb_wc_write", bytes = n, best_ns = best, us_per_mb = per_mb / 1000;
        "Mini-PC baseline 100-130 us/MB with WC (UC ~3200)");
    Ok(())
}
ktest!("mm.vmap.fb_wc_write_speed", Safe, fb_wc_write_speed);

/// hal's kernel-root primitives refuse what would break the live tables.
fn primitive_refusals() -> TestResult {
    let r = root()?;
    use hal::paging::{MapError, SwitchError};
    let text = text_target as fn() -> u64 as usize as u64;
    kassert!(paging::map_kernel(r, DIRECT_MAP_BASE + (1 << 46), 0, PageSize::Size4K, PageFlags::kernel_data()) == Err(MapError::WrongRange),
        "live root mapped outside vmap");
    kassert!(paging::unmap_kernel(text & !0xFFF, 0x1000, &mut |_| {}) == Err(MapError::WrongRange), "kernel text unmapped");
    kassert!(paging::switch_kernel_root(r ^ 0x1000) == Err(SwitchError::NotBuilt));
    kassert!(paging::free_unused_root(r) == Err(MapError::WrongRange), "the live root freed");
    kassert!(paging::translate(r, text).is_some(), "text still mapped");
    // One-shot hooks: a second install changes nothing.
    kassert!(!paging::set_table_frames(mm_core::alloc_table_frame, mm_core::free_table_frame), "table frame source replaced");
    fn dummy(_: u64, _: u64, _: CacheType) -> Result<(u64, alloc::boxed::Box<dyn core::any::Any + Send + Sync>), VmapError> {
        Err(VmapError::NotReady)
    }
    kassert!(!paging::set_device_mapper(dummy), "device mapper replaced");
    kassert!(probe::read_unmapped(0x0000_8000_0000_0000) == Err(probe::Refused::NonCanonical), "non-canonical probe");
    // A root where the first image page (text) lost its execute right: the switch must refuse it.
    let (img, _) = paging::kernel_image();
    let page = img & !0xFFF;
    let (phys, fl) = paging::translate(r, page).ok_or("image start unmapped")?;
    kassert!(!fl.contains(PageFlags::NX), "first image page is not executable");
    let cand = paging::new_kernel_root().map_err(|e| alloc::format!("{e:?}"))?;
    let mapped = paging::map_kernel(cand, page, phys & !0xFFF, PageSize::Size4K, PageFlags(PageFlags::kernel_data().0 | PageFlags::GLOBAL));
    let sw = paging::switch_kernel_root(cand);
    let freed = paging::free_unused_root(cand);
    kassert!(mapped.is_ok() && freed.is_ok(), "candidate root: map {mapped:?}, free {freed:?}");
    kassert!(sw == Err(SwitchError::RightsMismatch(page)), "switch to a root with NX text: {sw:?}");
    kassert!(paging::kernel_root() == Some(r) && root()? == r, "kernel root changed");
    Ok(())
}
ktest!("mm.paging.refusals", Safe, primitive_refusals);
