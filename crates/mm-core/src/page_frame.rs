//! Page-cache frames (FS-5 change request 3): one 4 KiB frame of `Kind::PageCache` from the per-CPU single-frame
//! path, zeroed, reached by safe code only through bounds-checked copies, given back when the handle drops.
//! Before this the page cache took `dma::alloc` per page (exact-run path, accounted as `Kind::Dma`).
//!
//! The frame may also be mapped into programs (file mappings, MM): their stores race with the kernel's copies
//! exactly as a device's DMA races with a driver's, which the volatile `DmaRegion` accessors allow for. The
//! frame is plain data, never a Rust object.

use crate::{frames, Kind, PAGE};
use hal::dma_buf::DmaRegion;
use hal::paging::CacheType;

/// One owned, zeroed page-cache frame.
#[derive(Debug)]
pub struct PageFrame {
    region: DmaRegion,
}

impl PageFrame {
    /// A zeroed frame; `None` when memory is out.
    pub fn new_zeroed() -> Option<PageFrame> {
        let phys = crate::alloc_zeroed(Kind::PageCache)?;
        // SAFETY: `phys` is a frame the allocator just handed out (zeroed by `alloc_zeroed`, nobody else knows
        // it), RAM reachable writable and write-back through the direct map, which is its only kernel mapping;
        // the handle owns it until `drop` frees it. A later user mapping of it is write-back too (MM maps
        // page-cache frames with the default memory type).
        let region = unsafe { DmaRegion::new(hal::phys_to_virt(phys), phys, PAGE as usize, CacheType::WriteBack) };
        Some(PageFrame { region })
    }

    pub fn phys(&self) -> u64 {
        self.region.phys()
    }

    /// Copies `out.len()` bytes from `off`; false (nothing copied) when the range leaves the page.
    pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
        self.region.copy_out(off, out)
    }

    /// Copies `data` to `off`; false (nothing copied) when the range leaves the page.
    pub fn write(&self, off: usize, data: &[u8]) -> bool {
        self.region.copy_in(off, data)
    }

    /// Sets `n` bytes from `off` to `byte`; false when the range leaves the page.
    pub fn fill(&self, off: usize, n: usize, byte: u8) -> bool {
        self.region.fill(off, n, byte)
    }
}

impl Drop for PageFrame {
    fn drop(&mut self) {
        if let Some(fa) = frames() {
            fa.free(self.region.phys(), 0, Kind::PageCache);
        }
    }
}
