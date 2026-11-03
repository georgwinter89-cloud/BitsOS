//! Abstract storage for page tables: the GX driver backs it with carve-out or pinned system pages and a
//! CPU mapping, DR's fake and the tests with plain memory.

use crate::pte::Target;

/// Entries per table on every GFX 11 level (block size 9, amdgpu_vm_pt.c:74 num_entries).
pub const ENTRIES: usize = 512;
/// Bytes of one table (512 entries of 8 bytes, one GPU page; amdgpu_vm_pt.c:137 pt_size).
pub const TABLE_BYTES: u64 = (ENTRIES as u64) * 8;

/// Where a table lives: host-physical address (4 KiB aligned) and memory kind (carve-out or system).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableAddr {
    pub phys: u64,
    pub target: Target,
}

/// Reading entries; what the walker needs. `None` when `table` is not a table the storage knows (the
/// hardware would read whatever lies there; the walker reports it).
pub trait TableRead {
    fn read(&self, table: u64, index: usize) -> Option<u64>;
}

/// Table storage the builder allocates from and writes through (always the CPU, never SDMA).
pub trait TableStore: TableRead {
    /// One table of [`TABLE_BYTES`]; `None` when out of memory. Contents need not be zero: the builder fills
    /// every new table before it links it.
    fn alloc(&mut self) -> Option<TableAddr>;
    /// How many more tables `alloc` can hand out at most (may be optimistic: `alloc` may still fail). The
    /// builder refuses a change that needs more with ENOMEM before it walks the whole request, so the work of
    /// one call stays proportional to the table memory the VM may use (GX gives each VM its own budget).
    fn available(&self) -> usize;
    /// Gives a table back. The builder calls it only for tables the GPU can no longer reach (never linked, or
    /// unlinked and past the TLB flush the caller confirmed).
    fn free(&mut self, table: TableAddr);
    /// One 8-byte entry, written as a single store so the GPU never sees half of it.
    fn write(&mut self, table: u64, index: usize, value: u64);
    /// Every entry of a table to one value.
    fn fill(&mut self, table: u64, value: u64) {
        for i in 0..ENTRIES {
            self.write(table, i, value);
        }
    }
}
