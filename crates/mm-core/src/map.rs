//! From a firmware memory map to the allocator's plan: which frames are free, which are boot modules kept for
//! a Reload, the regions (contiguous runs of pool memory, split at the 4 GiB zone boundary) and where the
//! allocator's own metadata goes.
//!
//! The map is input, not trusted: entries may be unsorted, overlap or be unaligned. Anything not free wins
//! over free (a frame claimed by both a free and a reserved entry stays reserved), frames are cut inward to
//! whole pages, and the first MiB is never used (BIOS data, the AP trampoline's neighbourhood; as Linux).

use crate::bitmap;
use crate::meta;
use crate::{MAX_ORDER, PAGE};
use alloc::vec::Vec;

/// What a memory map range is to the frame allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeUse {
    /// Free after the hand-over (conventional, loader and boot-services memory).
    Free,
    /// A boot module (initramfs, firmware): reserved until `release_module` (Reload rule, V2-PLAN §2.5).
    Module,
    /// Never handed out (KEEP, KERNEL, PAGETABLE, ACPI, runtime services, MMIO, holes, the heap seed).
    Reserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapRange {
    pub start: u64,
    pub end: u64,
    pub usage: RangeUse,
}

impl MapRange {
    pub fn new(start: u64, end: u64, usage: RangeUse) -> MapRange {
        MapRange { start, end, usage }
    }
}

/// The BootInfo memory map as ranges: free after hand-over -> `Free`, `BITS_MODULE` -> `Module`, every other
/// type (KEEP, KERNEL, PAGETABLE, ACPI, runtime services, MMIO, unknown) -> `Reserved`.
pub fn from_bootinfo(map: &bootinfo::MemoryMap<'_>) -> Vec<MapRange> {
    use bootinfo::memtype;
    map.iter()
        .map(|d| {
            // A descriptor running past the address space is cut at its end (input, never trusted).
            let end = d.pages.checked_mul(PAGE).and_then(|l| d.phys.checked_add(l)).unwrap_or(!(PAGE - 1));
            let usage = if memtype::is_free_after_handover(d.ty) {
                RangeUse::Free
            } else if d.ty == memtype::BITS_MODULE {
                RangeUse::Module
            } else {
                RangeUse::Reserved
            };
            MapRange::new(d.phys, end, usage)
        })
        .collect()
}

/// Memory below this is never part of the pool.
pub const LOW_RESERVED: u64 = 0x10_0000;
/// The DMA32 / NORMAL boundary, in frames.
pub const DMA32_END_PFN: u64 = (1u64 << 32) / PAGE;

/// Sorts and merges byte ranges; drops empty ones.
pub fn normalize(mut v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    v.retain(|r| r.1 > r.0);
    v.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
    for r in v {
        match out.last_mut() {
            Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

/// `a` minus `b`; both normalized, result normalized.
pub fn subtract(a: &[(u64, u64)], b: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut j = 0;
    for &(s, e) in a {
        let mut cur = s;
        while j < b.len() && b[j].1 <= cur {
            j += 1;
        }
        let mut k = j;
        while cur < e && k < b.len() && b[k].0 < e {
            if b[k].0 > cur {
                out.push((cur, b[k].0));
            }
            cur = cur.max(b[k].1);
            k += 1;
        }
        if cur < e {
            out.push((cur, e));
        }
    }
    out
}

/// Byte ranges -> whole frames inside them (pfn ranges).
fn to_frames(v: &[(u64, u64)]) -> Vec<(u64, u64)> {
    v.iter().map(|&(s, e)| (s.div_ceil(PAGE), e / PAGE)).filter(|r| r.1 > r.0).collect()
}

/// Splits pfn ranges at the 4 GiB boundary.
fn split_zones(v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(v.len() + 1);
    for (s, e) in v {
        if s < DMA32_END_PFN && e > DMA32_END_PFN {
            out.push((s, DMA32_END_PFN));
            out.push((DMA32_END_PFN, e));
        } else {
            out.push((s, e));
        }
    }
    out
}

/// Blocks of order `k` a region [s, e) (pfn) may contain, counted from the aligned block holding `s`.
pub fn blocks(s: u64, e: u64, k: u32) -> u64 {
    ((e - 1) >> k) - (s >> k) + 1
}

/// Metadata words for one region: the frame states plus a bitmap per order.
pub fn region_words(s: u64, e: u64) -> usize {
    meta::words_for(e - s) + (0..=MAX_ORDER).map(|k| bitmap::words_for(blocks(s, e, k))).sum::<usize>()
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Free frames (pfn ranges, sorted, disjoint), without the metadata carve.
    pub free: Vec<(u64, u64)>,
    /// Module frames (pfn), reserved until released.
    pub modules: Vec<(u64, u64)>,
    /// Regions (pfn), each inside one zone; every free or module frame lies in exactly one.
    pub regions: Vec<(u64, u64)>,
    /// Metadata words for all regions.
    pub words: usize,
    /// Frames taken for the metadata (pfn range), when carved from the pool.
    pub carve: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// No contiguous free run holds the metadata.
    NoRoomForMetadata { bytes: u64 },
}

/// Builds the plan. `carve`: take the metadata from the pool (boot) instead of the heap.
pub fn plan(ranges: &[MapRange], carve: bool) -> Result<Plan, PlanError> {
    let pick = |u: RangeUse| normalize(ranges.iter().filter(|r| r.usage == u).map(|r| (r.start, r.end)).collect());
    let mut reserved = pick(RangeUse::Reserved);
    reserved.push((0, LOW_RESERVED));
    let reserved = normalize(reserved);
    let modules_b = subtract(&pick(RangeUse::Module), &reserved);
    let free_b = subtract(&subtract(&pick(RangeUse::Free), &reserved), &modules_b);
    let mut free = to_frames(&free_b);
    let modules = to_frames(&modules_b);

    let mut pool: Vec<(u64, u64)> = free.iter().chain(modules.iter()).map(|&(s, e)| (s * PAGE, e * PAGE)).collect();
    pool = normalize(pool);
    let regions = split_zones(to_frames(&pool));
    let words: usize = regions.iter().map(|&(s, e)| region_words(s, e)).sum();

    let mut carved = None;
    if carve && words > 0 {
        let pages = (words as u64 * 8).div_ceil(PAGE);
        let best = free.iter().copied().filter(|r| r.1 - r.0 >= pages).max_by_key(|r| (r.1 - r.0, r.0));
        let Some((_, e)) = best else { return Err(PlanError::NoRoomForMetadata { bytes: pages * PAGE }) };
        let c = (e - pages, e);
        free = subtract(&free, &[c]);
        carved = Some(c);
    }
    Ok(Plan { free, modules, regions, words, carve: carved })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    #[test]
    fn interval_math() {
        assert_eq!(normalize(vec![(5, 9), (0, 2), (2, 3), (8, 12), (20, 20)]), vec![(0, 3), (5, 12)]);
        let a = normalize(vec![(0, 100), (200, 300)]);
        assert_eq!(subtract(&a, &[(10, 20), (90, 210), (250, 260)]), vec![(0, 10), (20, 90), (210, 250), (260, 300)]);
        assert_eq!(subtract(&a, &[]), a);
        assert_eq!(subtract(&a, &[(0, 1000)]), vec![]);
        assert_eq!(subtract(&[(0, 10)], &[(10, 20)]), vec![(0, 10)]);
    }

    #[test]
    fn reserved_wins_and_pages_cut_inward() {
        let r = [
            MapRange::new(0, 0x9F000, RangeUse::Free),
            MapRange::new(0x10_0000, 0x200_0000, RangeUse::Free),
            MapRange::new(0x80_0000, 0x90_0000, RangeUse::Reserved), // KEEP inside a free entry
            MapRange::new(0x100_0000, 0x110_0000, RangeUse::Module),
            MapRange::new(0x300_0800, 0x400_0800, RangeUse::Free), // unaligned
        ];
        let p = plan(&r, false).unwrap();
        assert_eq!(p.free, vec![(0x100, 0x800), (0x900, 0x1000), (0x1100, 0x2000), (0x3001, 0x4000)]);
        assert_eq!(p.modules, vec![(0x1000, 0x1100)]);
        assert_eq!(p.regions, vec![(0x100, 0x800), (0x900, 0x2000), (0x3001, 0x4000)]);
        assert!(p.carve.is_none());
    }

    #[test]
    fn regions_split_at_4g_and_carve_from_largest() {
        let r = [MapRange::new(0x10_0000, 0x1_4000_0000, RangeUse::Free)];
        let p = plan(&r, true).unwrap();
        assert_eq!(p.regions, vec![(0x100, DMA32_END_PFN), (DMA32_END_PFN, 0x14_0000)]);
        let c = p.carve.unwrap();
        assert_eq!(c.1, 0x14_0000, "taken from the top of the largest run (one run here, ending at 5 GiB)");
        assert!((c.1 - c.0) * PAGE >= p.words as u64 * 8);
        assert_eq!(p.free.iter().map(|r| r.1 - r.0).sum::<u64>() + (c.1 - c.0), 0x14_0000 - 0x100);
    }

    #[test]
    fn no_room_for_metadata() {
        // 4096 single free pages, each separated by a reserved page: no run holds the metadata.
        let r: std::vec::Vec<MapRange> = (0..4096u64).map(|i| MapRange::new(LOW_RESERVED + i * 2 * PAGE, LOW_RESERVED + (i * 2 + 1) * PAGE, RangeUse::Free)).collect();
        assert!(matches!(plan(&r, true), Err(PlanError::NoRoomForMetadata { .. })));
        assert!(plan(&r, false).is_ok());
    }
}
