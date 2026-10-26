//! The switch from bitsboot's page tables to the kernel's own (MM-2, spec/core.md §8.3, V2-PLAN §2.3).
//!
//! The new root has: the direct map of all RAM (`kmap::plan_direct_map`: 1 GiB / 2 MiB pages where aligned
//! and of one MTRR type, 4 KiB at the edges, the kernel's text and rodata read-only in their alias too), the
//! kernel image W^X per 4 KiB page, every kernel-half PDPT (shared with user roots), global pages for all of
//! it, and the loader's vmap mappings (the GOP framebuffer, WC) copied over. After the checked CR3 switch the
//! vmap allocator takes over [VMAP_BASE, CPU_AREA_BASE).
//!
//! The desk calls `switch_to_own` from kernel/ right after the frame allocator is installed. A Core initcall
//! does the same from the loader's own direct map if that call has not happened (so the tables switch in
//! every kernel), and is a no-op otherwise.

use crate::kmap::{self, ImagePerm, KernelSections};
use crate::mtrr::{self, Mtrrs};
use alloc::vec::Vec;
use bootinfo::layout::{CPU_AREA_BASE, DIRECT_MAP_BASE, VMAP_BASE, VMAP_END};
use bootinfo::{memtype, BootInfo};
use hal::paging::{self, MapError, PageSize, SwitchError, Unmapped};
use ksync::SpinLock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub root: u64,
    pub ram_bytes: u64,
    pub pages_1g: u64,
    pub pages_2m: u64,
    pub pages_4k: u64,
    /// RAM whose MTRR type is not WB (mapped all the same; the effective type is the MTRR's).
    pub non_wb_ram_bytes: u64,
    pub image_pages: u64,
    /// Image pages that had to stay writable and executable (a linker layout that shares a page).
    pub wx_pages: u64,
    /// Image pages where the linker sections and the loader's mapping disagreed (the loader's W^X mapping
    /// stays; the sections decide only pages the loader mapped W+X).
    pub section_mismatches: u64,
    /// Loader leaves copied into the new root (vmap and anything else in the kernel half).
    pub adopted_leaves: u64,
    /// The GOP framebuffer, mapped anew at the loader's address with WC: 2 MiB and 4 KiB pages.
    pub fb_pages_2m: u64,
    pub fb_pages_4k: u64,
    pub mtrr_ranges: usize,
    /// Page-table frames the new root took.
    pub table_frames: u64,
    /// Where the RAM list came from: "bootinfo" or "loader_tables".
    pub source: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TablesError {
    NoMemoryMap,
    NoFrameAllocator,
    /// A kernel-image page is not mapped in the loader's tables.
    ImageUnmapped(u64),
    Map(MapError),
    Switch(SwitchError),
    /// A RAM chunk beyond the direct map's window (cannot happen after the clamp in `run`).
    RamOutOfRange(u64),
}

static REPORT: SpinLock<Option<Report>> = SpinLock::named("mm.tables_report", None);
static RAM: SpinLock<Vec<(u64, u64)>> = SpinLock::named("mm.tables_ram", Vec::new());
/// The GOP framebuffer as the switch found it: (virt, phys, size).
static FB: SpinLock<Option<(u64, u64, u64)>> = SpinLock::named("mm.tables_fb", None);
/// The BSP's raw MTRRs the direct map was planned with.
static MTRR_REGS: SpinLock<Option<hal::pat::MtrrRegs>> = SpinLock::named("mm.tables_mtrrs", None);

/// The BSP's MTRR snapshot the direct map and the vmap large-page decisions rest on (`None` before the switch
/// or without MTRRs). CP-4 compares each AP's `hal::pat::read_mtrrs()` with it (core.md §11 trap 12).
pub fn mtrr_snapshot() -> Option<hal::pat::MtrrRegs> {
    MTRR_REGS.lock().clone()
}

/// What the switch did (`None` before it).
pub fn report() -> Option<Report> {
    *REPORT.lock()
}

/// The RAM ranges the direct map covers (sorted, merged, page-aligned; empty before the switch).
pub fn ram() -> Vec<(u64, u64)> {
    RAM.lock().clone()
}

/// The GOP framebuffer mapping carried over from the loader: (virt, phys, size), if there is one.
pub fn framebuffer() -> Option<(u64, u64, u64)> {
    *FB.lock()
}

/// Builds the kernel's own tables from the BootInfo memory map and switches to them. `sections` are the
/// linker's section bounds once kernel/linker.ld exports them; without them the image permissions come from
/// the loader's mapping of the ELF segments. Idempotent; on error the loader's tables stay in use.
pub fn switch_to_own(bi: &BootInfo<'_>, sections: Option<KernelSections>) -> Result<Report, TablesError> {
    // The shootdown vector before anything else: this runs on the BSP before CP-4 starts the APs (MM-6).
    crate::tlb::start();
    let map = bi.memory_map().ok_or(TablesError::NoMemoryMap)?;
    let ram = kmap::page_ranges(map.iter().filter(|d| memtype::is_ram(d.ty)).map(|d| (d.phys, d.end())));
    if let Some(f) = bi.framebuffer().filter(|f| f.virt != 0 && f.size != 0) {
        *FB.lock() = Some((f.virt, f.phys, f.size));
    }
    run(ram, sections, "bootinfo")
}

fn from_loader_tables() -> Result<(), &'static str> {
    crate::tlb::start();
    if paging::kernel_root().is_some() {
        return Ok(());
    }
    let mut ram = Vec::new();
    paging::for_each_leaf(paging::current_root(), DIRECT_MAP_BASE, VMAP_BASE, &mut |u| {
        let p = u.virt - DIRECT_MAP_BASE;
        ram.push((p, p + u.size.bytes()));
    });
    let ram = kmap::page_ranges(ram);
    // bitsboot maps the framebuffer at the start of vmap; its size is the run of leaves there.
    let mut fb: Option<(u64, u64, u64)> = None;
    paging::for_each_leaf(paging::current_root(), VMAP_BASE, CPU_AREA_BASE, &mut |u| match &mut fb {
        None if u.virt == VMAP_BASE => fb = Some((u.virt, u.phys, u.size.bytes())),
        Some(f) if f.0 + f.2 == u.virt && f.1 + f.2 == u.phys => f.2 += u.size.bytes(),
        _ => {}
    });
    *FB.lock() = fb;
    klog::event!(Warn, "mm", "paging.switch_from_initcall"; "kernel/ did not call mm::kernel_tables::switch_to_own; RAM taken from the loader's direct map");
    run(ram, None, "loader_tables").map(|_| ()).map_err(|_| "switch to the kernel's own page tables failed (see mm paging.* event)")
}
kreg::initcall!(kreg::Level::Core, "mm.kernel_tables", from_loader_tables);

fn table_frames_used() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::PageTable as usize] / mm_core::PAGE)
}

fn run(ram: Vec<(u64, u64)>, sections: Option<KernelSections>, source: &'static str) -> Result<Report, TablesError> {
    if paging::kernel_root().is_some() {
        return report().ok_or(TablesError::Switch(SwitchError::SameRoot));
    }
    if mm_core::frames().is_none() {
        return Err(TablesError::NoFrameAllocator);
    }
    // One-shot in hal; a second run (after a failed first one) finds the same pair installed.
    let _ = paging::set_table_frames(mm_core::alloc_table_frame, mm_core::free_table_frame);
    // The direct map has room for [0, VMAP_BASE - DIRECT_MAP_BASE) only; RAM beyond it stays unmapped, but
    // still counts as RAM for the vmap allocator's refusal.
    let (direct, dropped) = kmap::clamp_ranges(&ram, VMAP_BASE - DIRECT_MAP_BASE);
    if dropped > 0 {
        klog::event!(Warn, "mm", "paging.ram_beyond_direct_map", dropped_mb = dropped >> 20, limit = klog::Hex(VMAP_BASE - DIRECT_MAP_BASE);
            "RAM above the direct map's window left unmapped");
    }
    let r = build(&direct, &ram, sections.as_ref(), source);
    match &r {
        Ok(rep) => {
            *REPORT.lock() = Some(*rep);
            *RAM.lock() = direct;
            klog::event!(Info, "mm", "paging.switched", root = klog::Hex(rep.root), source = rep.source, ram_mb = rep.ram_bytes >> 20,
                pages_1g = rep.pages_1g, pages_2m = rep.pages_2m, pages_4k = rep.pages_4k, non_wb_ram_mb = rep.non_wb_ram_bytes >> 20,
                image_pages = rep.image_pages, wx_pages = rep.wx_pages, section_mismatches = rep.section_mismatches,
                adopted = rep.adopted_leaves, fb_pages_2m = rep.fb_pages_2m, fb_pages_4k = rep.fb_pages_4k, mtrr_ranges = rep.mtrr_ranges, table_frames = rep.table_frames);
            if rep.wx_pages > 0 {
                klog::event!(Error, "mm", "paging.wx_violation", pages = rep.wx_pages; "kernel image pages both writable and executable");
            }
            if rep.section_mismatches > 0 {
                klog::event!(Warn, "mm", "paging.section_mismatch", pages = rep.section_mismatches; "linker sections and loader mapping disagree");
            }
        }
        Err(e) => klog::event!(Error, "mm", "paging.switch_failed", source = source, error = alloc::format!("{e:?}"); "staying on the loader's page tables"),
    }
    r
}

/// `ram` is what the direct map covers, `all_ram` every RAM range (the vmap allocator refuses all of it).
fn build(ram: &[(u64, u64)], all_ram: &[(u64, u64)], sections: Option<&KernelSections>, source: &'static str) -> Result<Report, TablesError> {
    let regs = hal::pat::read_mtrrs();
    let mtrrs = regs.as_ref().map_or_else(Mtrrs::none, Mtrrs::from_regs);
    let old = paging::current_root();
    let mut rep = Report { source, mtrr_ranges: mtrrs.variable_ranges(), ..Default::default() };

    // The image, page by page, as the loader mapped it (bitsboot follows the ELF program headers).
    let (img_s, img_e) = paging::kernel_image();
    let mut image: Vec<(u64, u64, ImagePerm)> = Vec::new();
    let mut va = img_s & !0xFFF;
    while va < img_e {
        let (phys, fl) = paging::translate(old, va).ok_or(TablesError::ImageUnmapped(va))?;
        let loader = ImagePerm::from_loader(fl.contains(paging::PageFlags::WRITABLE), !fl.contains(paging::PageFlags::NX));
        let (perm, mismatch) = kmap::image_perm(sections, va, loader);
        rep.section_mismatches += mismatch as u64;
        rep.wx_pages += (perm == ImagePerm::WriteExec) as u64;
        image.push((va, phys & !0xFFF, perm));
        va += 0x1000;
    }
    rep.image_pages = image.len() as u64;
    let read_only = kmap::page_ranges(image.iter().filter(|p| p.2.read_only()).map(|p| (p.1, p.1 + 0x1000)));

    let before = table_frames_used();
    let root = paging::new_kernel_root().map_err(TablesError::Map)?;
    let taken = match fill(root, old, ram, &read_only, &mtrrs, &image, &mut rep) {
        Ok(t) => t,
        Err(e) => {
            let _ = paging::free_unused_root(root);
            return Err(e);
        }
    };
    if let Err(e) = paging::switch_kernel_root(root) {
        let _ = paging::free_unused_root(root);
        return Err(TablesError::Switch(e));
    }
    rep.root = root;
    rep.table_frames = table_frames_used().saturating_sub(before);
    crate::vmap::init(VMAP_BASE, CPU_AREA_BASE, &taken, all_ram.to_vec(), mtrrs.clone());
    if let Some(r) = regs {
        klog::event!(Info, "mm", "mtrr", enabled = mtrrs.enabled(), default = mtrr::type_name(mtrrs.default_type()), variable = r.var.len(),
            valid = mtrrs.variable_ranges(), phys_bits = r.phys_bits, tom2 = klog::Hex(r.tom2.unwrap_or(0)));
        *MTRR_REGS.lock() = Some(r);
    }
    Ok(rep)
}

/// Maps everything into the new `root`; returns the vmap ranges already in use (adopted loader mappings).
fn fill(root: u64, old: u64, ram: &[(u64, u64)], read_only: &[(u64, u64)], mtrrs: &Mtrrs, image: &[(u64, u64, ImagePerm)], rep: &mut Report)
    -> Result<Vec<(u64, u64)>, TablesError> {
    let gb = hal::cpu::detect().pdpe1gb;
    let mut err = None;
    kmap::plan_direct_map(ram, read_only, mtrrs, gb, &mut |c| {
        if err.is_some() {
            return;
        }
        let Some(va) = DIRECT_MAP_BASE.checked_add(c.phys).filter(|v| v.checked_add(c.size.bytes()).is_some_and(|e| e <= VMAP_BASE)) else {
            err = Some(TablesError::RamOutOfRange(c.phys));
            return;
        };
        if let Err(e) = paging::map_kernel(root, va, c.phys, c.size, kmap::direct_flags(&c)) {
            err = Some(TablesError::Map(e));
            return;
        }
        let n = c.size.bytes();
        rep.ram_bytes += n;
        if c.mtrr != hal::pat::MT_WB {
            rep.non_wb_ram_bytes += n;
        }
        match c.size {
            PageSize::Size1G => rep.pages_1g += 1,
            PageSize::Size2M => rep.pages_2m += 1,
            PageSize::Size4K => rep.pages_4k += 1,
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    for &(va, phys, perm) in image {
        paging::map_kernel(root, va, phys, PageSize::Size4K, perm.flags()).map_err(TablesError::Map)?;
    }
    // The framebuffer at the loader's address, now with MM's WC pages (2 MiB where phys, virt and the MTRRs
    // allow); everything else the loader mapped in vmap (early MMIO, the CPU area's IST stacks) is copied, and
    // mappings outside the known regions, should there be any, are kept and reported.
    let mut taken = Vec::new();
    let fb = framebuffer().map(|(v, p, n)| (v & !0xFFF, p & !0xFFF, ((p + n + 0xFFF) & !0xFFF) - (p & !0xFFF)));
    let adopt = |lo: u64, hi: u64| paging::adopt_leaves(old, root, lo, hi).map_err(TablesError::Map);
    let vmap = if let Some((v, p, n)) = fb {
        map_framebuffer(root, v, p, n, mtrrs, rep)?;
        taken.push((v, v + n));
        let mut l = adopt(VMAP_BASE, v)?;
        l.extend(adopt(v + n, VMAP_END)?);
        l
    } else {
        adopt(VMAP_BASE, VMAP_END)?
    };
    let img_end = (paging::kernel_image().1 + 0xFFF) & !0xFFF;
    let mut other = adopt(paging::KERNEL_HALF, DIRECT_MAP_BASE)?;
    other.extend(adopt(VMAP_END, bootinfo::layout::KERNEL_BASE)?);
    other.extend(adopt(img_end, 0xFFFF_FFFF_FFFF_F000)?);
    if !other.is_empty() {
        klog::event!(Warn, "mm", "paging.adopted_unknown", leaves = other.len(), first = klog::Hex(other[0].virt);
            "loader mappings outside the direct map, image and vmap kept");
    }
    rep.adopted_leaves = (vmap.len() + other.len()) as u64;
    taken.extend(merge(&vmap));
    Ok(kmap::page_ranges(taken))
}

/// Maps [p, p + n) at v (both page-aligned) WC + global into `root`.
fn map_framebuffer(root: u64, v: u64, p: u64, n: u64, mtrrs: &Mtrrs, rep: &mut Report) -> Result<(), TablesError> {
    const M2: u64 = 2 << 20;
    let flags = paging::PageFlags(paging::PageFlags::kernel_data().with_cache(paging::CacheType::WriteCombining).0 | paging::PageFlags::GLOBAL);
    let mut off = 0;
    while off < n {
        let (va, pa) = (v + off, p + off);
        let big = va % M2 == 0 && pa % M2 == 0 && n - off >= M2 && mtrrs.block_type(pa, M2).is_some();
        let size = if big { PageSize::Size2M } else { PageSize::Size4K };
        paging::map_kernel(root, va, pa, size, flags).map_err(TablesError::Map)?;
        if big {
            rep.fb_pages_2m += 1;
        } else {
            rep.fb_pages_4k += 1;
        }
        off += size.bytes();
    }
    Ok(())
}

/// Adopted leaves as merged virtual ranges.
fn merge(leaves: &[Unmapped]) -> Vec<(u64, u64)> {
    kmap::page_ranges(leaves.iter().map(|u| (u.virt, u.virt + u.size.bytes())))
}
