//! The plan of the kernel's own page tables (pure, host-tested; `kernel_tables` carries it out).
//!
//! Direct map: every RAM page at `DIRECT_MAP_BASE + phys`, with the largest page (1 GiB if the CPU has
//! them, 2 MiB, 4 KiB) that is aligned, all RAM, of one MTRR type and on one side of every read-only range
//! (the kernel's own text and rodata, so their direct-map alias is not writable either, as Linux's CPA does).
//! Kernel image: one permission per 4 KiB page, W^X: text RX, rodata R, data/bss RW, never W+X.

use crate::mtrr::Mtrrs;
use alloc::vec::Vec;
use hal::paging::{PageFlags, PageSize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub phys: u64,
    pub size: PageSize,
    pub writable: bool,
    /// MTRR type of the chunk (one type by construction).
    pub mtrr: u8,
}

/// Sorted, merged, page-aligned ranges (cut inward to whole pages; empty ones dropped).
pub fn page_ranges(it: impl IntoIterator<Item = (u64, u64)>) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = it.into_iter().map(|(s, e)| (s.saturating_add(0xFFF) & !0xFFF, e & !0xFFF)).filter(|r| r.1 > r.0).collect();
    v.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
    for r in v {
        match out.last_mut() {
            Some(l) if r.0 <= l.1 => l.1 = l.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

/// Whether [s, e) lies wholly inside one range of `v` (sorted, merged) or touches none.
fn one_side(v: &[(u64, u64)], s: u64, e: u64) -> Option<bool> {
    let i = v.partition_point(|r| r.1 <= s);
    match v.get(i) {
        Some(r) if r.0 <= s && e <= r.1 => Some(true),
        Some(r) if r.0 < e => None,
        _ => Some(false),
    }
}

/// Calls `f` for each page of the direct map of `ram` (from `page_ranges`), lowest address first.
pub fn plan_direct_map(ram: &[(u64, u64)], read_only: &[(u64, u64)], mtrrs: &Mtrrs, gb_pages: bool, f: &mut dyn FnMut(Chunk)) {
    let sizes: &[PageSize] = if gb_pages { &[PageSize::Size1G, PageSize::Size2M, PageSize::Size4K] } else { &[PageSize::Size2M, PageSize::Size4K] };
    for &(s, e) in ram {
        let mut a = s;
        while a < e {
            // A 4 KiB page always has one type and one side (MTRRs and the ranges are page-granular); the
            // fallback step only keeps a broken input from looping forever.
            let mut step = 0x1000;
            for &size in sizes {
                let n = size.bytes();
                if a % n != 0 || e - a < n {
                    continue;
                }
                let (Some(mtrr), Some(ro)) = (mtrrs.block_type(a, n), one_side(read_only, a, a + n)) else { continue };
                f(Chunk { phys: a, size, writable: !ro, mtrr });
                step = n;
                break;
            }
            a += step;
        }
    }
}

/// Leaf flags of a direct-map chunk.
pub fn direct_flags(c: &Chunk) -> PageFlags {
    let w = if c.writable { PageFlags::WRITABLE } else { 0 };
    PageFlags(PageFlags::PRESENT | PageFlags::GLOBAL | PageFlags::NX | w)
}

/// What one page of the kernel image may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImagePerm {
    /// .text: read + execute.
    Text,
    /// .rodata and the registries: read only.
    ReadOnly,
    /// .data, .bss: read + write.
    Data,
    /// A page the loader mapped writable and executable (two segments sharing a page): kept working, but a
    /// W^X violation the ktest reports.
    WriteExec,
}

impl ImagePerm {
    pub fn from_loader(writable: bool, executable: bool) -> ImagePerm {
        match (writable, executable) {
            (false, true) => ImagePerm::Text,
            (false, false) => ImagePerm::ReadOnly,
            (true, false) => ImagePerm::Data,
            (true, true) => ImagePerm::WriteExec,
        }
    }
    pub fn flags(self) -> PageFlags {
        let base = PageFlags::PRESENT | PageFlags::GLOBAL;
        PageFlags(match self {
            ImagePerm::Text => base,
            ImagePerm::ReadOnly => base | PageFlags::NX,
            ImagePerm::Data => base | PageFlags::WRITABLE | PageFlags::NX,
            ImagePerm::WriteExec => base | PageFlags::WRITABLE,
        })
    }
    pub fn read_only(self) -> bool {
        matches!(self, ImagePerm::Text | ImagePerm::ReadOnly)
    }
}

/// Section bounds from linker symbols (kernel/linker.ld), when the desk passes them; each [start, end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelSections {
    pub text: (u64, u64),
    pub rodata: (u64, u64),
    pub data: (u64, u64),
}

impl KernelSections {
    /// The permission of the page at `va` by the linker's sections; `None` outside all three.
    pub fn perm_of(&self, va: u64) -> Option<ImagePerm> {
        let page = (va & !0xFFF, (va & !0xFFF) + 0x1000);
        let touches = |r: (u64, u64)| r.0 < page.1 && page.0 < r.1;
        // A page shared by two sections gets the rights of both (cannot happen with 4 KiB-aligned sections).
        let (w, x) = (touches(self.data), touches(self.text));
        if !w && !x && !touches(self.rodata) {
            return None;
        }
        Some(ImagePerm::from_loader(w, x))
    }
}

/// The permission of one image page. The loader's mapping (bitsboot builds it from the ELF program headers)
/// is what the kernel runs on now, so it stays wherever it is W^X already: the linker's sections only split a
/// page the loader mapped W+X, and otherwise serve as a cross-check. Letting the sections win elsewhere would
/// let one wrong symbol make running code NX or a live stack read-only, which faults right after the switch.
/// Returns the permission and whether sections and loader disagreed (reported, never fatal).
pub fn image_perm(sections: Option<&KernelSections>, va: u64, loader: ImagePerm) -> (ImagePerm, bool) {
    match sections.and_then(|s| s.perm_of(va)) {
        Some(p) if loader == ImagePerm::WriteExec => (p, p != loader),
        Some(p) => (loader, p != loader),
        None => (loader, false),
    }
}

/// `ranges` (sorted, merged) cut to [0, limit): the direct map has room for that much physical address
/// space only. Returns the kept ranges and the bytes dropped (a firmware map beyond it is not trusted).
pub fn clamp_ranges(ranges: &[(u64, u64)], limit: u64) -> (Vec<(u64, u64)>, u64) {
    let mut dropped = 0;
    let mut kept = Vec::with_capacity(ranges.len());
    for &(s, e) in ranges {
        let ce = e.min(limit);
        dropped += e - s.max(ce).min(e);
        if ce > s {
            kept.push((s, ce));
        }
    }
    (kept, dropped)
}

#[cfg(test)]
#[path = "kmap_tests.rs"]
mod tests;
