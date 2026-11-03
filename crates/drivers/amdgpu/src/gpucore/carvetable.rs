//! GX-7h (GR-1 change request 2): GPU page tables in the carve-out, written by the CPU through `Hw` (the live
//! owner's uncached 2 MiB pieces, a fake's memory in tests). Tables are addressed as the GPU's page-table walker
//! sees them: host-physical (`FB_OFFSET << 24 + offset`, a `TableAddr` with `Target::Local`); the CPU writes them
//! at the MC address of the same byte (`Carveout::phys_to_mc`). Every entry is one 8-byte store
//! (`Hw::mem_write64`), and the caller puts `Hw::store_barrier` between the last table write and the TLB
//! invalidate.
//!
//! - `CarveGart`: the flat GART table of VMID 0 (`amdgpu_fmt::gart::{GartSink, GartRead}`), with a bulk fill that
//!   writes whole 4 KiB blocks (131072 entries for Linux's 512 MiB window).
//! - `CarveTables`: `amdgpu_fmt::store::TableStore` over a carve-out range (one table per 4 KiB page), for the
//!   per-process VMs of GX-7m.
//!
//! A failed store (a piece the owner may not map, a protected range) sets `failed` and the write is lost: the
//! caller checks `failed()` before it points a hub at the table.

use super::psp::Hw;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::gart::{GartRead, GartSink};
use amdgpu_fmt::pte::Target;
use amdgpu_fmt::store::{ENTRIES, TABLE_BYTES, TableAddr, TableRead, TableStore};
use core::cell::{Cell, RefCell};

/// Bytes written per store of a bulk fill (one GPU page of entries).
pub const BLOCK: usize = 4096;

/// The GART table at MC `mc` with `entries` entries.
pub struct CarveGart<'a> {
    hw: RefCell<&'a mut dyn Hw>,
    pub mc: u64,
    pub entries: u64,
    failed: Cell<bool>,
}

impl<'a> CarveGart<'a> {
    pub fn new(hw: &'a mut dyn Hw, mc: u64, entries: u64) -> CarveGart<'a> {
        CarveGart { hw: RefCell::new(hw), mc, entries, failed: Cell::new(false) }
    }

    pub fn failed(&self) -> bool {
        self.failed.get()
    }

    /// Every entry to `value`, a block at a time (the table is not live yet: no torn entry can be seen).
    pub fn fill(&mut self, value: u64) {
        let mut block = [0u8; BLOCK];
        for c in block.chunks_exact_mut(8) {
            c.copy_from_slice(&value.to_le_bytes());
        }
        let bytes = self.entries.saturating_mul(8);
        let mut at = 0u64;
        while at < bytes {
            let n = (bytes - at).min(BLOCK as u64) as usize;
            if !self.hw.get_mut().mem_write(self.mc + at, &block[..n]) {
                self.failed.set(true);
                return;
            }
            at += n as u64;
        }
    }
}

impl GartSink for CarveGart<'_> {
    fn write(&mut self, index: u64, value: u64) {
        let ok = index < self.entries && self.hw.get_mut().mem_write64(self.mc + index * 8, value);
        if !ok {
            self.failed.set(true);
        }
    }
}

impl GartRead for CarveGart<'_> {
    fn read(&self, index: u64) -> Option<u64> {
        if index >= self.entries {
            return None;
        }
        let mut b = [0u8; 8];
        self.hw.borrow_mut().mem_read(self.mc + index * 8, &mut b).then(|| u64::from_le_bytes(b))
    }
}

/// Page tables in `pages` (carve-out offsets, whole 4 KiB pages) of `carve`.
pub struct CarveTables<'a> {
    hw: RefCell<&'a mut dyn Hw>,
    carve: Carveout,
    start: u64,
    end: u64,
    next: u64,
    free: alloc::vec::Vec<u64>,
    failed: Cell<bool>,
}

impl<'a> CarveTables<'a> {
    /// `None` when the range is not whole pages inside the carve-out.
    pub fn new(hw: &'a mut dyn Hw, carve: Carveout, pages: core::ops::Range<u64>) -> Option<CarveTables<'a>> {
        let ok = pages.start.is_multiple_of(TABLE_BYTES) && pages.end.is_multiple_of(TABLE_BYTES) && pages.start < pages.end && pages.end <= carve.size();
        ok.then(|| CarveTables { hw: RefCell::new(hw), carve, start: pages.start, end: pages.end, next: pages.start, free: alloc::vec::Vec::new(), failed: Cell::new(false) })
    }

    pub fn failed(&self) -> bool {
        self.failed.get()
    }

    /// The MC address of entry `index` of the table at host-physical `table`.
    fn mc(&self, table: u64, index: usize) -> Option<u64> {
        if index >= ENTRIES || !self.carve.contains_phys(table, TABLE_BYTES) {
            return None;
        }
        self.carve.phys_to_mc(table)?.checked_add(index as u64 * 8)
    }
}

impl TableRead for CarveTables<'_> {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        let mc = self.mc(table, index)?;
        let mut b = [0u8; 8];
        self.hw.borrow_mut().mem_read(mc, &mut b).then(|| u64::from_le_bytes(b))
    }
}

impl TableStore for CarveTables<'_> {
    fn alloc(&mut self) -> Option<TableAddr> {
        let off = match self.free.pop() {
            Some(o) => o,
            None if self.next < self.end => {
                let o = self.next;
                self.next += TABLE_BYTES;
                o
            }
            None => return None,
        };
        Some(TableAddr { phys: self.carve.phys_of(off)?, target: Target::Local })
    }
    fn available(&self) -> usize {
        self.free.len() + ((self.end - self.next) / TABLE_BYTES) as usize
    }
    fn free(&mut self, table: TableAddr) {
        if let Some(off) = table.phys.checked_sub(self.carve.phys_base()).filter(|o| (self.start..self.end).contains(o) && !self.free.contains(o)) {
            self.free.push(off);
        }
    }
    fn write(&mut self, table: u64, index: usize, value: u64) {
        let ok = self.mc(table, index).is_some_and(|mc| self.hw.get_mut().mem_write64(mc, value));
        if !ok {
            self.failed.set(true);
        }
    }
    fn fill(&mut self, table: u64, value: u64) {
        let Some(mc) = self.mc(table, 0) else {
            self.failed.set(true);
            return;
        };
        let mut block = [0u8; BLOCK];
        for c in block.chunks_exact_mut(8) {
            c.copy_from_slice(&value.to_le_bytes());
        }
        if !self.hw.get_mut().mem_write(mc, &block[..TABLE_BYTES as usize]) {
            self.failed.set(true);
        }
    }
}
