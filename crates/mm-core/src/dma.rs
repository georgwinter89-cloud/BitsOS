//! DMA frames (MM-4, contract `C: DmaPool`): the only place that builds a `hal::dma_buf::DmaRegion`. The
//! policy (constraints, events, the driver API) is `mm::dma`; this file does what needs `unsafe`: frames from
//! the buddy below an address limit, zeroed, the direct-map alias retyped for UC/WC, and the way back.

use crate::{boot_buddy, Kind, PAGE};
use hal::dma_buf::DmaRegion;
use hal::paging::{self, CacheType, MapError, RetypeStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaAllocError {
    /// Zero length, a length past the largest block, or an alignment that is not a power of two.
    BadRequest,
    /// No run of that size below the limit.
    NoMemory,
    /// The direct map could not take the memory type (UC/WC); the frames went back.
    Retype(MapError),
}

/// Contiguous frames for DMA with their CPU view. Dropping it gives the direct map its WB type back (UC/WC)
/// and the frames to the buddy, which hands them out again at once: the owner stops the device first.
#[derive(Debug)]
pub struct DmaBlock {
    region: DmaRegion,
    pages: u64,
    /// What the retype did (UC/WC only), for the caller's event.
    retype: Option<RetypeStats>,
}

impl DmaBlock {
    pub fn region(&self) -> &DmaRegion {
        &self.region
    }
    /// Frames the block holds (the run is exactly this long, no power-of-two rounding).
    pub fn pages(&self) -> u64 {
        self.pages
    }
    pub fn retype_stats(&self) -> Option<RetypeStats> {
        self.retype
    }
}

/// `len` bytes (whole frames, zeroed) aligned to `align` (a power of two, at least a page), ending at or below
/// `limit` (exclusive), with the CPU view typed `cache`. The run sits at the start of a buddy block of at least
/// its own size and alignment, so it never crosses a multiple of that block size (`mm::dma` derives the
/// boundary rule from this).
pub fn alloc(len: usize, align: u64, limit: u64, cache: CacheType) -> Result<DmaBlock, DmaAllocError> {
    let pages = (len as u64).div_ceil(PAGE);
    if len == 0 || !align.is_power_of_two() {
        return Err(DmaAllocError::BadRequest);
    }
    crate::buddy::exact_order(pages, (align / PAGE).max(1)).ok_or(DmaAllocError::BadRequest)?;
    let b = boot_buddy().ok_or(DmaAllocError::NoMemory)?;
    let phys = b.alloc_exact_below(pages, (align / PAGE).max(1), limit, Kind::Dma).ok_or(DmaAllocError::NoMemory)?;
    let bytes = pages * PAGE;
    zero(phys, bytes);
    let mut retype = None;
    if cache != CacheType::WriteBack {
        match paging::set_direct_map_cache(phys, bytes, cache) {
            // Lines the zeroing left in the caches would be written back over the device's data later. Flushed
            // after the type change, as Linux's `cpa_flush` does: until the shootdown inside the retype the WB
            // alias was live on every CPU, so lines could be filled up to that point; after it no CPU holds the
            // WB translation, and CLFLUSH reaches every cache of the coherence domain (MM-6).
            Ok(st) => {
                hal::dma::flush_cache_range(hal::phys_to_virt(phys), bytes);
                retype = Some(st);
            }
            // A CPU did not confirm the shootdown after the type changed: some CPU may still use the WB alias,
            // so the frames must never be handed out again as ordinary memory.
            Err(e @ paging::MapError::NeedsShootdown) => {
                KEPT_UNTYPED.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
                return Err(DmaAllocError::Retype(e));
            }
            Err(e) => {
                give_back(phys, pages);
                return Err(DmaAllocError::Retype(e));
            }
        }
    }
    // SAFETY: [phys, phys + bytes) is a run the buddy just handed to us (nobody else knows it), RAM in the
    // direct map at phys_to_virt(phys), writable, and that alias is the only mapping, now typed `cache`. `len`
    // <= bytes. The block keeps the frames until its drop, which outlives the region it owns.
    let region = unsafe { DmaRegion::new(hal::phys_to_virt(phys), phys, len, cache) };
    Ok(DmaBlock { region, pages, retype })
}

/// Zeroes frames this module just got from the buddy (through the WB direct map).
fn zero(phys: u64, bytes: u64) {
    #[cfg(target_os = "none")]
    // SAFETY: only `alloc` calls it, on a run the buddy just handed out and nobody else knows yet, RAM in the
    // direct map.
    unsafe {
        core::ptr::write_bytes(hal::phys_to_virt(phys) as *mut u8, 0, bytes as usize)
    };
    #[cfg(not(target_os = "none"))]
    let _ = (phys, bytes);
}

/// Frames kept out of use because their direct-map type could not be set back to WB (never expected).
pub static KEPT_UNTYPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

impl Drop for DmaBlock {
    fn drop(&mut self) {
        let (phys, bytes) = (self.region.phys(), self.pages * PAGE);
        if self.region.cache() != CacheType::WriteBack {
            // Drain WC buffers before the type changes, then back to WB (the leaves are split already, so
            // only bits change). If that fails the frames must never be reused as ordinary memory.
            self.region.write_barrier();
            if let Err(e) = paging::set_direct_map_cache(phys, bytes, CacheType::WriteBack) {
                KEPT_UNTYPED.fetch_add(self.pages, core::sync::atomic::Ordering::Relaxed);
                klog::event!(Error, "mm", "dma.retype_back_failed", phys = klog::Hex(phys), pages = self.pages, error = alloc::format!("{e:?}");
                    "DMA frames kept out of use: their direct-map type could not go back to write-back");
                return;
            }
        }
        give_back(phys, self.pages);
    }
}

/// Frames the buddy refused to take back (wrong state or no buddy: never expected); they stay out of use.
pub static FREE_REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Returns a run to the buddy; a refusal is counted and logged (B6) instead of passing silently.
fn give_back(phys: u64, pages: u64) {
    if boot_buddy().is_some_and(|b| b.free_exact(phys, pages, Kind::Dma)) {
        return;
    }
    FREE_REFUSED.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
    klog::event!(Error, "mm", "dma.free_failed", phys = klog::Hex(phys), pages = pages;
        "DMA frames kept out of use: the frame allocator refused to take them back");
}
