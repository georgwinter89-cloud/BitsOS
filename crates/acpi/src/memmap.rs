//! The firmware memory map as the ACPI code needs it: which physical ranges are RAM (direct-mapped, never a
//! register), which are firmware-reserved (tables may live there, read through `hal::mmio`), and which are
//! holes or MMIO (never read as a table).

use alloc::vec::Vec;
use bootinfo::memtype;

/// UEFI "reserved" (type 0): firmware data, sometimes ACPI tables, sometimes MMIO.
pub const RESERVED: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
    pub ty: u32,
}

/// Sorted, non-overlapping regions. Overlaps in a broken firmware map are cut: the region that starts first
/// keeps the overlapping part (lookups then stay a binary search and every walk makes progress).
#[derive(Clone, Debug, Default)]
pub struct MemMap {
    regions: Vec<Region>,
}

impl MemMap {
    pub fn new(mut regions: Vec<Region>) -> MemMap {
        regions.sort_by_key(|r| (r.start, r.end));
        let mut out: Vec<Region> = Vec::with_capacity(regions.len());
        for mut r in regions {
            if let Some(l) = out.last() {
                r.start = r.start.max(l.end);
            }
            if r.end <= r.start {
                continue;
            }
            // Merge touching regions of the same type so a table across two descriptors reads as one range.
            match out.last_mut() {
                Some(l) if l.ty == r.ty && r.start == l.end => l.end = r.end,
                _ => out.push(r),
            }
        }
        MemMap { regions: out }
    }

    pub fn from_bootinfo(bi: &bootinfo::BootInfo<'_>) -> MemMap {
        let regions = bi
            .memory_map()
            .map(|m| m.iter().map(|d| Region { start: d.phys, end: d.end(), ty: d.ty }).collect())
            .unwrap_or_default();
        MemMap::new(regions)
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// The region holding `addr`, if any.
    pub fn find(&self, addr: u64) -> Option<&Region> {
        let i = self.regions.partition_point(|r| r.end <= addr);
        self.regions.get(i).filter(|r| r.start <= addr)
    }

    /// True if any byte of `start..end` lies in a region whose type satisfies `pred`.
    pub fn touches(&self, start: u64, end: u64, pred: impl Fn(u32) -> bool) -> bool {
        self.regions.iter().any(|r| r.start < end && start < r.end && pred(r.ty))
    }

    /// True if every byte of `start..end` lies in regions whose type satisfies `pred` (no holes).
    pub fn covers(&self, start: u64, end: u64, pred: impl Fn(u32) -> bool) -> bool {
        let mut at = start;
        while at < end {
            match self.find(at) {
                Some(r) if pred(r.ty) && r.end > at => at = r.end,
                _ => return false,
            }
        }
        true
    }
}

/// RAM of any use: reachable through the direct map, never a device register.
pub fn is_ram(ty: u32) -> bool {
    memtype::is_ram(ty)
}

/// Memory a table may be read from outside RAM: firmware-reserved only (not MMIO, not unusable, not a hole).
pub fn is_reserved(ty: u32) -> bool {
    ty == RESERVED
}
