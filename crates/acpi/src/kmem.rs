//! The kernel's `PhysMem`: RAM pages (ACPI reclaim/NVS and every other RAM type) through the direct map,
//! firmware-reserved pages through `hal::mmio::map_device` (it refuses RAM, so RAM is tried first). Holes,
//! MMIO, unusable memory, ECAM windows and disk controllers' BARs are never read as a table.
//! Reserved pages are mapped uncached: tables there are rare, and a reserved page may also hold device
//! registers that a driver maps UC later (two mappings with different cache types must not alias). Mapping
//! a page reads nothing; only the dwords a table read asks for are read. Pages holding registers an earlier
//! table named (HPET, I/O APICs, LAPIC, ECAM, IOMMU, FADT blocks in memory space) are refused; a pointer into
//! some other register page before those tables are known cannot be told from a table (spec/core.md §4 note).

use crate::guard::PciView;
use crate::mcfg::Ecam;
use crate::mem::{MemError, PhysMem};
use crate::memmap::{is_ram, is_reserved, MemMap};
use crate::ranges::RangeSet;
use alloc::collections::btree_map::{BTreeMap, Entry};
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;

const PAGE: u64 = 4096;

pub struct KernelMem<'a> {
    map: &'a MemMap,
    phys_bits: u32,
    ecam: Vec<Ecam>,
    /// A cheap view (a `PciSnapshot`): the guard asks it once per reserved page.
    pci: &'a dyn PciView,
    /// Register blocks earlier tables named (`PhysMem::avoid`): never read as a table.
    avoid: RangeSet,
    /// Reserved pages mapped so far (G0's vmap never unmaps; one window per page, reused; at most the parse
    /// budget's worth of pages).
    windows: BTreeMap<u64, IoMem>,
}

impl<'a> KernelMem<'a> {
    pub fn new(map: &'a MemMap, phys_bits: u32, pci: &'a dyn PciView) -> KernelMem<'a> {
        KernelMem { map, phys_bits, ecam: Vec::new(), pci, avoid: RangeSet::new(), windows: BTreeMap::new() }
    }

    /// ECAM windows known from an earlier parse (reads inside them are refused).
    pub fn with_ecam(mut self, ecam: &[Ecam]) -> Self {
        self.ecam = ecam.to_vec();
        self
    }

    /// Reserved pages mapped through `hal::mmio` so far.
    pub fn windows(&self) -> usize {
        self.windows.len()
    }

    fn read_reserved(&mut self, page: u64, off: usize, out: &mut [u8]) -> Result<(), MemError> {
        if self.avoid.overlaps(page, page + PAGE) {
            return Err(MemError::Guarded);
        }
        let g = crate::guard::Guard { map: self.map, phys_bits: self.phys_bits, ecam: &self.ecam, pci: self.pci };
        g.check_mem_range(page, page + PAGE).map_err(|_| MemError::Guarded)?;
        let w = match self.windows.entry(page) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => v.insert(hal::mmio::map_device(page, PAGE as usize, CacheType::Uncached).map_err(|_| MemError::MapFailed)?),
        };
        // Aligned dword reads: firmware memory behind a chipset decoder may not answer byte reads (QEMU's
        // HPET page does not), and uncached dwords are four times fewer bus reads.
        let mut word = (off & !3, [0u8; 4]);
        let mut have = false;
        for (i, b) in out.iter_mut().enumerate() {
            let at = off + i;
            if !have || at & !3 != word.0 {
                word = (at & !3, w.read32(at & !3).map_err(|_| MemError::MapFailed)?.to_le_bytes());
                have = true;
            }
            *b = word.1[at & 3];
        }
        Ok(())
    }
}

impl PhysMem for KernelMem<'_> {
    fn avoid(&mut self, start: u64, end: u64) {
        self.avoid.insert(start, end);
    }

    fn read(&mut self, phys: u64, out: &mut [u8]) -> Result<(), MemError> {
        let end = phys.checked_add(out.len() as u64).ok_or(MemError::OutOfRange)?;
        let mut at = phys;
        let mut done = 0usize;
        while at < end {
            let page = at & !(PAGE - 1);
            let stop = page.checked_add(PAGE).ok_or(MemError::OutOfRange)?.min(end);
            let n = (stop - at) as usize;
            let off = (at - page) as usize;
            let buf = &mut out[done..done + n];
            if self.map.covers(page, page + PAGE, is_ram) {
                // RAM is in the direct map (bitsboot maps every RAM type, ACPI reclaim and NVS included).
                if !hal::dma_buf::read(page, PAGE as usize, off, buf) {
                    return Err(MemError::OutOfRange);
                }
            } else if self.map.covers(page, page + PAGE, is_reserved) {
                self.read_reserved(page, off, buf)?;
            } else {
                return Err(MemError::NotTableMemory);
            }
            at = stop;
            done += n;
        }
        Ok(())
    }
}
