//! Scattered DMA frames (MM-14; the policy, limits and driver API are `mm::dma_pages`): a set of blocks from the
//! frame allocator that need not be contiguous with each other, each zeroed, its direct-map alias retyped for
//! UC/WC, with a CPU view per block. As Linux's TTM pool (linux-6.6/drivers/gpu/drm/ttm/ttm_pool.c:447-500) the
//! largest fitting block is tried first, and a larger order than one frame is taken only from what is free now
//! (`alloc_light`, as TTM's `__GFP_NORETRY` for orders above 0, ttm_pool.c:91-93): a big GPU buffer never
//! pushes the system into reclaim for contiguity it does not need; only single frames take the full path.
//!
//! The frames are `Kind::Dma`: never reclaimed (reclaim only shrinks the page cache), never moved, never merged
//! into a huge page (only `Kind::User` frames are), never copied by fork (device VMAs are re-faulted). They stay
//! allocated until the set drops; the owner stops the device first (the `mm::dma` rule).

use crate::dma::{DmaAllocError, FREE_REFUSED, KEPT_UNTYPED};
use crate::{frames, FrameAlloc, Kind, Zone, PAGE};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::dma_buf::DmaRegion;
use hal::paging::{self, CacheType, MapError, RetypeStats};

/// Pages of WC/UC sets whose last drop came with interrupts off (an interrupt handler, an IRQ lock held): kept out
/// of use instead of retyped back, which needs thread context (MM-14 fixer). Should stay 0.
pub static KEPT_IRQ_OFF: AtomicU64 = AtomicU64::new(0);

/// Frames held by sets now (taken, not yet back in the allocator; kept frames stay counted): an exact balance
/// for tests and reports that other DMA users (USB, network) do not disturb, unlike `used_by_kind[Dma]`.
static LIVE_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Frames all sets hold now (see `LIVE_FRAMES`).
pub fn live_frames() -> u64 {
    LIVE_FRAMES.load(Ordering::Relaxed)
}

/// One block of the set: 2^order contiguous frames with its CPU view.
#[derive(Debug)]
struct Run {
    region: DmaRegion,
    /// Index of the run's first page inside the set.
    first: u64,
    order: u32,
}

/// Frames of one DMA object in blocks; page `i` of the object is page `i - first` of the run holding it.
#[derive(Debug)]
pub struct PageRuns {
    runs: Vec<Run>,
    pages: u64,
    cache: CacheType,
    retype: RetypeStats,
}

/// The order of the next block for `remaining` pages: the largest power of two not above it, at most `cap`.
pub fn next_order(remaining: u64, cap: u32) -> u32 {
    if remaining == 0 {
        return 0;
    }
    (63 - remaining.leading_zeros()).min(cap)
}

/// The index of the run holding `page`, given `len` runs whose first pages `first(i)` rise from 0.
pub fn run_index(len: usize, first: impl Fn(usize) -> u64, page: u64) -> Option<usize> {
    // partition_point over 0..len: the runs starting at or before `page`.
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if first(mid) <= page {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.checked_sub(1)
}

/// ktest hook: an allocation fails as `NoMemory` once it would hold more than this many pages (0: off).
static TEST_FAIL_AFTER: AtomicU64 = AtomicU64::new(0);

/// Tests: make `alloc_runs` fail after `pages` pages (the out-of-frames path halfway), None = off.
pub fn test_fail_after(pages: Option<u64>) {
    TEST_FAIL_AFTER.store(pages.map_or(0, |p| p.max(1)), Ordering::Relaxed);
}

/// The block loop of `alloc_runs` (TTM's order walk): asks `take(order)` for blocks, largest fitting order
/// first, one order smaller whenever `take` answers false (none free at that order), until `pages` pages are
/// held. A refusal at order 0, or a block past `fail_after` pages (0: no such limit), is `NoMemory`.
pub fn fill(pages: u64, max_order: u32, fail_after: u64, mut take: impl FnMut(u32) -> Result<bool, DmaAllocError>) -> Result<(), DmaAllocError> {
    let cap = max_order.min(crate::MAX_ORDER);
    let mut held = 0;
    let mut order = next_order(pages, cap);
    while held < pages {
        order = order.min(next_order(pages - held, cap));
        if fail_after != 0 && held + (1 << order) > fail_after {
            return Err(DmaAllocError::NoMemory);
        }
        if take(order)? {
            held += 1 << order;
        } else if order == 0 {
            return Err(DmaAllocError::NoMemory);
        } else {
            order -= 1;
        }
    }
    Ok(())
}

/// `pages` zeroed frames from `zone` in blocks of at most 2^`max_order` frames, the CPU view typed `cache`.
/// Thread context only (single frames may reclaim; UC/WC retype with interrupts off and take frames). On any
/// failure every frame taken so far goes back (or stays out of use if its type could not be undone).
pub fn alloc_runs(pages: u64, max_order: u32, zone: Zone, cache: CacheType) -> Result<PageRuns, DmaAllocError> {
    if pages == 0 || pages > (1 << 52) / PAGE {
        return Err(DmaAllocError::BadRequest);
    }
    let fa = frames().ok_or(DmaAllocError::NoMemory)?;
    let mut set = PageRuns { runs: Vec::new(), pages: 0, cache, retype: RetypeStats::default() };
    // On an error `set` drops with the blocks taken so far: back to WB and to the allocator.
    fill(pages, max_order, TEST_FAIL_AFTER.load(Ordering::Relaxed), |order| set.take(fa, order, zone))?;
    Ok(set)
}

impl PageRuns {
    /// One block of `order` for the set (false: none free at that order).
    fn take(&mut self, fa: &dyn FrameAlloc, order: u32, zone: Zone) -> Result<bool, DmaAllocError> {
        // Room for the entry before the frames exist, so a full heap never strands a block.
        self.runs.try_reserve(1).map_err(|_| DmaAllocError::NoMemory)?;
        let got = if order > 0 { fa.alloc_light(order, zone, Kind::Dma) } else { fa.alloc(0, zone, Kind::Dma) };
        let Some(phys) = got else { return Ok(false) };
        let bytes = PAGE << order;
        for i in 0..1u64 << order {
            crate::zero_frame(phys + i * PAGE);
        }
        let cache = self.cache;
        if cache != CacheType::WriteBack {
            match paging::set_direct_map_cache(phys, bytes, cache) {
                // Flushed after the type change, as `dma::alloc` does (the WB alias was live until the shootdown).
                Ok(st) => {
                    hal::dma::flush_cache_range(hal::phys_to_virt(phys), bytes);
                    self.retype.split_1g += st.split_1g;
                    self.retype.split_2m += st.split_2m;
                    self.retype.pages += st.pages;
                }
                Err(e @ MapError::NeedsShootdown) => {
                    // Some CPU may still hold the WB alias: never hand these frames out again.
                    KEPT_UNTYPED.fetch_add(1 << order, Ordering::Relaxed);
                    return Err(DmaAllocError::Retype(e));
                }
                Err(e) => {
                    fa.free(phys, order, Kind::Dma);
                    return Err(DmaAllocError::Retype(e));
                }
            }
        }
        // SAFETY: [phys, phys + bytes) is a block the allocator just handed to us (nobody else knows it), RAM in
        // the direct map at phys_to_virt(phys), writable, and that alias is its only mapping, now typed `cache`.
        // The run owns the block until `PageRuns::drop` frees it, which outlives the region.
        let region = unsafe { DmaRegion::new(hal::phys_to_virt(phys), phys, bytes as usize, cache) };
        self.runs.push(Run { region, first: self.pages, order });
        self.pages += 1 << order;
        LIVE_FRAMES.fetch_add(1 << order, Ordering::Relaxed);
        Ok(true)
    }

    pub fn pages(&self) -> u64 {
        self.pages
    }
    pub fn cache(&self) -> CacheType {
        self.cache
    }
    /// How many contiguous blocks the set has.
    pub fn runs(&self) -> usize {
        self.runs.len()
    }
    /// Block `i`: (physical address, pages).
    pub fn run(&self, i: usize) -> Option<(u64, u64)> {
        self.runs.get(i).map(|r| (r.region.phys(), 1 << r.order))
    }
    /// What the UC/WC retype did over all blocks.
    pub fn retype_stats(&self) -> RetypeStats {
        self.retype
    }
    fn find(&self, page: u64) -> Option<&Run> {
        if page >= self.pages {
            return None;
        }
        run_index(self.runs.len(), |i| self.runs[i].first, page).and_then(|i| self.runs.get(i))
    }
    /// The physical address of page `page` of the set.
    pub fn page_phys(&self, page: u64) -> Option<u64> {
        self.find(page).map(|r| r.region.phys() + (page - r.first) * PAGE)
    }
    /// The CPU view holding byte `off` of the set and the offset inside it.
    pub fn locate(&self, off: u64) -> Option<(&DmaRegion, usize)> {
        let r = self.find(off / PAGE)?;
        Some((&r.region, (off - r.first * PAGE) as usize))
    }
    /// Drains WC buffers of every block (before a device reads what the CPU wrote).
    pub fn write_barrier(&self) {
        if let Some(r) = self.runs.first() {
            r.region.write_barrier();
        }
    }
}

impl Drop for PageRuns {
    fn drop(&mut self) {
        let fa: Option<&dyn FrameAlloc> = frames();
        if self.cache != CacheType::WriteBack {
            if !hal::cpu::interrupts_enabled() {
                // Retyping back shoots down every CPU's TLB and takes table frames: thread context only. Here
                // nothing may wait, allocate, free or log, so the frames (and the list) are kept for good.
                KEPT_IRQ_OFF.fetch_add(self.pages, Ordering::Relaxed);
                core::mem::forget(core::mem::take(&mut self.runs));
                return;
            }
            self.write_barrier();
        }
        for r in self.runs.drain(..) {
            let (phys, pages) = (r.region.phys(), 1u64 << r.order);
            if r.region.cache() != CacheType::WriteBack {
                if let Err(e) = paging::set_direct_map_cache(phys, pages * PAGE, CacheType::WriteBack) {
                    KEPT_UNTYPED.fetch_add(pages, Ordering::Relaxed);
                    klog::event!(Error, "mm", "dmapages.retype_back_failed", phys = klog::Hex(phys), pages = pages, error = alloc::format!("{e:?}");
                        "DMA pages kept out of use: their direct-map type could not go back to write-back");
                    continue;
                }
            }
            match fa {
                Some(fa) => {
                    fa.free(phys, r.order, Kind::Dma);
                    LIVE_FRAMES.fetch_sub(pages, Ordering::Relaxed);
                }
                None => {
                    FREE_REFUSED.fetch_add(pages, Ordering::Relaxed);
                }
            }
        }
    }
}
