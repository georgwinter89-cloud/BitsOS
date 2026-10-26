//! The memory map the new kernel gets: the running BootInfo's map (the firmware's types and attributes as bitsboot
//! recorded them) with the old kernel's own ranges given back and the new kernel's ranges marked. Pure.
//!
//! - `BITS_KERNEL` and `BITS_PAGETABLE` (the running kernel's image, boot stack and boot tables) become free:
//!   nothing runs from them once the stub switched CR3.
//! - The replaced initramfs (`BITS_MODULE`) becomes free; other modules (firmware blobs) stay.
//! - The frames Reload took from the running kernel's free memory become `BITS_KERNEL` (image, stack),
//!   `BITS_MODULE` (initramfs) and `BITS_PAGETABLE` (the new tables).
//! - `BITS_KEEP` is never touched. Adjacent equal descriptors are merged, so the map does not grow per Reload.

use alloc::vec::Vec;
use bootinfo::{MemDesc, memtype};

const PAGE: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapError {
    /// A range to mark is not wholly inside descriptors of an allowed type.
    NotCovered { start: u64, end: u64, want: u32 },
    NotAligned { start: u64, end: u64 },
}

/// One range to change: `[start, end)` becomes `ty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub start: u64,
    pub end: u64,
    pub ty: u32,
}

/// Splits the descriptors overlapping `[start, end)` and gives that part type `ty`. Every byte of the range
/// must lie in a descriptor for which `ok(old type)` holds.
fn retype(map: &mut Vec<MemDesc>, start: u64, end: u64, ty: u32, ok: fn(u32) -> bool) -> Result<(), MapError> {
    if !start.is_multiple_of(PAGE) || !end.is_multiple_of(PAGE) || end <= start {
        return Err(MapError::NotAligned { start, end });
    }
    let mut covered = 0u64;
    let mut out = Vec::with_capacity(map.len() + 2);
    for d in map.iter() {
        let (s, e) = (d.phys, d.end());
        if e <= start || s >= end {
            out.push(*d);
            continue;
        }
        let (a, b) = (s.max(start), e.min(end));
        if !ok(d.ty) {
            return Err(MapError::NotCovered { start, end, want: ty });
        }
        covered += b - a;
        let part = |from: u64, to: u64, t: u32| MemDesc {
            ty: t,
            phys: from,
            virt: if d.virt == 0 { 0 } else { d.virt + (from - s) },
            pages: (to - from) / PAGE,
            attr: d.attr,
        };
        if a > s {
            out.push(part(s, a, d.ty));
        }
        out.push(part(a, b, ty));
        if e > b {
            out.push(part(b, e, d.ty));
        }
    }
    if covered != end - start {
        return Err(MapError::NotCovered { start, end, want: ty });
    }
    *map = out;
    Ok(())
}

fn merge(map: Vec<MemDesc>) -> Vec<MemDesc> {
    let mut out: Vec<MemDesc> = Vec::with_capacity(map.len());
    for d in map.into_iter().filter(|d| d.pages > 0) {
        if let Some(l) = out.last_mut() {
            let virt_ok = (l.virt == 0 && d.virt == 0) || (l.virt != 0 && d.virt == l.virt + l.pages * PAGE);
            if l.ty == d.ty && l.attr == d.attr && l.end() == d.phys && virt_ok {
                l.pages += d.pages;
                continue;
            }
        }
        out.push(d);
    }
    out
}

/// The new kernel's map. `release`: module ranges to give back (the replaced initramfs); `marks`: the new
/// kernel's ranges (frames the running kernel's allocator handed out, so inside memory that is free after the
/// hand-over).
pub fn rebuild(old: impl Iterator<Item = MemDesc>, release: &[(u64, u64)], marks: &[Mark]) -> Result<Vec<MemDesc>, MapError> {
    let mut map: Vec<MemDesc> = old.collect();
    map.sort_by_key(|d| d.phys);
    for d in map.iter_mut() {
        if matches!(d.ty, memtype::BITS_KERNEL | memtype::BITS_PAGETABLE) {
            d.ty = memtype::CONVENTIONAL;
        }
    }
    for &(s, e) in release {
        retype(&mut map, s & !(PAGE - 1), e.next_multiple_of(PAGE), memtype::CONVENTIONAL, |t| t == memtype::BITS_MODULE)?;
    }
    for m in marks {
        retype(&mut map, m.start, m.end, m.ty, memtype::is_free_after_handover)?;
    }
    Ok(merge(map))
}

/// Whether every byte of `[start, end)` lies in descriptors of type `ty` (Reload checks that the stub page and
/// the BootInfo block are bitsboot's `BITS_KEEP` pages before it writes them at the jump). Empty ranges are not.
pub fn covered_by(map: impl Iterator<Item = MemDesc>, start: u64, end: u64, ty: u32) -> bool {
    if end <= start {
        return false;
    }
    let mut parts: Vec<(u64, u64)> =
        map.filter(|d| d.ty == ty && d.end() > start && d.phys < end).map(|d| (d.phys.max(start), d.end().min(end))).collect();
    parts.sort_unstable();
    let mut at = start;
    for (s, e) in parts {
        if s > at {
            return false;
        }
        at = at.max(e);
    }
    at >= end
}

/// What the new kernel will be able to use: (all free bytes above 1 MiB, the largest free descriptor). The
/// kernel seeds its heap from the largest single descriptor (`kernel/src/main.rs`).
pub fn free_after(map: &[MemDesc]) -> (u64, u64) {
    let mut total = 0;
    let mut largest = 0;
    for d in map.iter().filter(|d| memtype::is_free_after_handover(d.ty)) {
        let (s, e) = (d.phys.max(0x10_0000), d.end());
        if e > s {
            total += e - s;
            largest = largest.max(e - s);
        }
    }
    (total, largest)
}

/// RAM ranges (merged) for the direct map, like bitsboot's `mem::ram_ranges`.
pub fn ram_ranges(map: &[MemDesc]) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = Vec::new();
    for d in map.iter().filter(|d| memtype::is_ram(d.ty) && d.pages > 0) {
        match v.last_mut() {
            Some(l) if l.1 == d.phys => l.1 = d.end(),
            _ => v.push((d.phys, d.end())),
        }
    }
    v
}
