//! The GART: VMID 0's flat page table (context depth 0, one 8-byte entry per 4 KiB GPU page) that gives the
//! kernel's own buffers (rings, IH, writeback, IB pools) a GPU address in the GART window.
//!
//! Linux: amdgpu_gart.c:341 amdgpu_gart_map, :296 unbind (dummy page address, flags 0: "system bit must be 0
//! to mean invalid"), gmc_v11_0.c:741-756 (table size = pages * 8, default flags MTYPE UC | EXECUTABLE).
//! The window's place and size are arguments (the 780M under Linux: 512 MiB at 0x7FFF_0000_0000, table in the
//! carve-out); nothing is built in.

use crate::pte::{self, MapFlags, Mtype, PTE_ADDR_MASK, SNOOPED, SYSTEM, Target, VALID};
use crate::addr::Carveout;
use crate::{GPU_PAGE, GPU_PAGE_SHIFT, addr, errno};

/// Where GART entries go (the table's CPU mapping in the driver, a vector in fakes and tests).
pub trait GartSink {
    fn write(&mut self, index: u64, value: u64);
}

/// Reading GART entries back.
pub trait GartRead {
    fn read(&self, index: u64) -> Option<u64>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GartError {
    /// Window or dummy page not page aligned, empty, or outside 48 bits.
    BadWindow,
    /// Offset not page aligned or the pages run past the window end.
    OutOfWindow(u64),
    /// A page address that is unaligned, wider than 48 bits, or (local page) outside the carve-out.
    BadPhys(u64),
}

impl GartError {
    pub fn errno(&self) -> i32 {
        errno::EINVAL
    }
}

/// A GART window: `start` (MC address) and the number of 4 KiB pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gart {
    start: u64,
    pages: u64,
    dummy: u64,
    local: Carveout,
}

/// What a GART page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GartPage {
    pub target: Target,
    pub writeable: bool,
}

impl Gart {
    /// A window of `size` bytes at MC address `start`; unbound entries point at `dummy` (a page the GPU may
    /// read or write harmlessly when a stale access races an unbind). Local pages must lie inside `local`.
    pub fn new(start: u64, size: u64, dummy: u64, local: Carveout) -> Result<Gart, GartError> {
        let aligned = |v: u64| v.is_multiple_of(GPU_PAGE);
        if size == 0 || !aligned(start) || !aligned(size) || !aligned(dummy) || dummy >> addr::VA_BITS != 0 {
            return Err(GartError::BadWindow);
        }
        if start.checked_add(size).is_none_or(|end| end > addr::VA_SIZE) {
            return Err(GartError::BadWindow);
        }
        Ok(Gart { start, pages: size >> GPU_PAGE_SHIFT, dummy, local })
    }

    pub fn start(&self) -> u64 {
        self.start
    }
    pub fn entries(&self) -> u64 {
        self.pages
    }
    /// Bytes of the table (gmc_v11_0.c:752).
    pub fn table_bytes(&self) -> u64 {
        self.pages * 8
    }

    /// Value of one bound entry: VALID, READABLE, WRITEABLE unless read-only, SYSTEM/SNOOPED by target, and
    /// the GART defaults MTYPE UC | EXECUTABLE (amdgpu_ttm.c:1344 amdgpu_ttm_tt_pte_flags).
    pub fn entry(phys: u64, page: GartPage) -> Result<u64, GartError> {
        let flags =
            MapFlags { read: true, write: page.writeable, exec: true, mtype: Mtype::Uc, tmz: false, noalloc: false };
        pte::leaf(phys, GPU_PAGE, page.target, flags, 0, false).map_err(|_| GartError::BadPhys(phys))
    }

    /// Value of an unbound entry (amdgpu_gart.c:296-318).
    pub fn unbound(&self) -> u64 {
        self.dummy
    }

    fn index(&self, offset: u64, n: u64) -> Result<u64, GartError> {
        if !offset.is_multiple_of(GPU_PAGE) {
            return Err(GartError::OutOfWindow(offset));
        }
        let first = offset >> GPU_PAGE_SHIFT;
        match first.checked_add(n) {
            Some(end) if end <= self.pages => Ok(first),
            _ => Err(GartError::OutOfWindow(offset)),
        }
    }

    /// Binds `pages` (host-physical, 4 KiB each) at byte `offset` of the window. Checks everything before the
    /// first write. Returns the GPU (MC) address of the first page.
    pub fn map(&self, sink: &mut dyn GartSink, offset: u64, pages: &[u64], page: GartPage) -> Result<u64, GartError> {
        let first = self.index(offset, pages.len() as u64)?;
        for &p in pages {
            Gart::entry(p, page)?;
            if page.target == Target::Local && !self.local.contains_phys(p, GPU_PAGE) {
                return Err(GartError::BadPhys(p));
            }
        }
        for (k, &p) in pages.iter().enumerate() {
            // Checked in the loop above.
            sink.write(first + k as u64, Gart::entry(p, page).unwrap_or(self.dummy));
        }
        Ok(self.start + offset)
    }

    /// Unbinds `n` pages at byte `offset`. The caller flushes VMID 0's TLB before reusing the pages.
    pub fn unmap(&self, sink: &mut dyn GartSink, offset: u64, n: u64) -> Result<(), GartError> {
        let first = self.index(offset, n)?;
        for k in 0..n {
            sink.write(first + k, self.dummy);
        }
        Ok(())
    }

    /// Every entry unbound (before the window is enabled).
    pub fn clear(&self, sink: &mut dyn GartSink) {
        for i in 0..self.pages {
            sink.write(i, self.dummy);
        }
    }

    /// Translates an MC address in the window: `Ok(Some((phys, target)))` for a bound page, `Ok(None)` for an
    /// unbound one or one outside the window.
    pub fn translate(&self, table: &dyn GartRead, mc: u64) -> Result<Option<(u64, Target)>, GartError> {
        let Some(off) = mc.checked_sub(self.start).filter(|&o| o < self.pages << GPU_PAGE_SHIFT) else {
            return Ok(None);
        };
        let e = table.read(off >> GPU_PAGE_SHIFT).ok_or(GartError::OutOfWindow(off))?;
        if e & VALID == 0 {
            return Ok(None);
        }
        let target = match (e & SYSTEM != 0, e & SNOOPED != 0) {
            (false, _) => Target::Local,
            (true, s) => Target::System { snooped: s },
        };
        Ok(Some(((e & PTE_ADDR_MASK) | (off & (GPU_PAGE - 1)), target)))
    }
}

/// A GART table in plain memory (fakes, tests).
pub struct GartImage(pub alloc::vec::Vec<u64>);

impl GartImage {
    pub fn new(g: &Gart) -> GartImage {
        GartImage(alloc::vec![g.unbound(); g.entries() as usize])
    }
}

impl GartSink for GartImage {
    fn write(&mut self, index: u64, value: u64) {
        if let Some(e) = self.0.get_mut(index as usize) {
            *e = value;
        }
    }
}

impl GartRead for GartImage {
    fn read(&self, index: u64) -> Option<u64> {
        self.0.get(index as usize).copied()
    }
}
