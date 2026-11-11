//! The heap's large blocks: exact page runs from the buddy allocator, single frames in the vmap area above the
//! costly order (`scatter`), and the orders missing runs ask kswapd to make free (`OrderWant`).
//!
//! Linux 7.x: kvmalloc's contiguous try keeps __GFP_KSWAPD_RECLAIM, and an atomic high-order failure does the
//! same: `wake_all_kswapds` / `wakeup_kswapd(zone, gfp, order, ...)` raise `kswapd_order`, and kswapd balances
//! for that order (`pgdat_balanced` with `zone_watermark_ok_safe(zone, order, ...)`). Here the orders are noted in
//! an atomic and each note wakes kswapd (W35 MM-1, `reclaim::wake_kswapd`; deferred under a spin lock):
//! callers under a spin lock cannot reclaim, so the free blocks they need must be made in the background.

use super::KernelHeap;
use crate::{Kind, PAGE};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// Pages and page alignment of a large block.
pub(super) fn large_shape(size: usize, align: usize) -> (u64, u64) {
    ((size as u64).div_ceil(PAGE), (align as u64 / PAGE).max(1))
}

/// Linux's `MAX_PAGE_ORDER` (4 MiB): the largest block kswapd is asked to make.
pub const MAX_WANTED: u32 = 10;

/// The block orders heap allocations found no free run for since kswapd last took them, lock-free: one bit per
/// order (not only the highest: a rare big block must not hide the small ones asked for all the time).
pub struct OrderWant {
    bits: AtomicU32,
    noted: AtomicU64,
}

impl OrderWant {
    pub const fn new() -> Self {
        OrderWant { bits: AtomicU32::new(0), noted: AtomicU64::new(0) }
    }

    /// Order 0 is left out: a missing frame is the free count's shortage, which kswapd balances anyway. Orders
    /// above `MAX_WANTED` too, as Linux's page allocator refuses them before it wakes anyone.
    /// W35 MM-1: every note wakes kswapd (`reclaim::wake_kswapd`, deferred where the context may not call the
    /// scheduler), as Linux's `wakeup_kswapd` does on each failure: an order kswapd put back (`keep`) stays pending,
    /// and a wake only for a new bit would leave its later failures to the poll. While kswapd runs a look or is
    /// kicked already, the wake is a few atomics (`WaitQueue::wake_one` with nobody queued); `MIN_GAP_MS` bounds
    /// the looks.
    pub fn note(&self, order: u32) {
        if order > 0 && order <= MAX_WANTED {
            self.bits.fetch_or(1 << order, Relaxed);
            self.noted.fetch_add(1, Relaxed);
            crate::reclaim::wake_kswapd();
        }
    }

    /// The pending orders as bits (bit k: order k).
    pub fn pending(&self) -> u32 {
        self.bits.load(Relaxed)
    }

    /// The pending orders, cleared; an order noted meanwhile stays for the next take.
    pub fn take(&self) -> u32 {
        self.bits.swap(0, Relaxed)
    }

    /// Puts back an order kswapd took but did not finish (not counted as noted).
    pub fn keep(&self, order: u32) {
        if order <= MAX_WANTED {
            self.bits.fetch_or(1 << order, Relaxed);
        }
    }

    /// Allocations that noted an order (`HEAP_ORDER_WANTED` for the kernel heap).
    pub fn noted(&self) -> u64 {
        self.noted.load(Relaxed)
    }
}

impl Default for OrderWant {
    fn default() -> Self {
        Self::new()
    }
}

impl KernelHeap {
    /// The orders this heap's large blocks found no run for (kswapd's side: `take`, and `keep` what it left).
    pub fn order_want(&self) -> &OrderWant {
        &self.want
    }

    pub(super) fn large_alloc(&self, size: usize, align: usize) -> usize {
        let (pages, align_pages) = large_shape(size, align);
        let exact = crate::buddy::exact_order(pages, align_pages);
        let order = exact.unwrap_or(crate::MAX_ORDER);
        // kvmalloc: every round tries the run first (what `regain` just freed may be one), then single frames
        // (which reclaim does produce).
        let scatter = self.may_scatter(pages, align_pages);
        // A run up to the costly order insists on contiguous frames first (the cheap, usual case) and takes single
        // frames only once reclaim and compaction did not make it one.
        let late = self.may_scatter_late(pages, align_pages);
        // A block single frames serve at once asks nobody to make a run: no order for kswapd and kcompactd, no
        // direct compaction (Linux's kvmalloc tries the run with __GFP_NORETRY | __GFP_NOWARN and goes to vmalloc).
        // Mini-PC 11.11.2026: Steam verifying files reads through 500 KiB buffers, 40 of these a second, each one a
        // compaction run over up to 2 GiB of frames and a wake of kcompactd (33 million frames scanned a second).
        let mut noted = exact.is_none() || scatter;
        let mut try_once = |pressed: bool| {
            if let Some(v) = self.large_try(pages, align_pages) {
                return Some(v);
            }
            if !noted {
                noted = true;
                self.want.note(order);
                if self.global && order > 0 {
                    crate::reclaim::HEAP_ORDER_WANTED.inc();
                }
            }
            if scatter || (late && pressed) { self.scatter_alloc(pages) } else { None }
        };
        let mut regain = super::Regain::default();
        // The reserve as in `small_alloc`.
        let mut keep = crate::reclaim::keep_reserve();
        for attempt in 0u32.. {
            if attempt == super::KEEP_TRIES && keep.as_ref().is_some_and(crate::reclaim::Keep::held) {
                keep = None;
            }
            if let Some(v) = try_once(attempt >= 2) {
                if attempt >= 2 {
                    crate::reclaim::HEAP_RESCUED.inc();
                }
                return v;
            }
            if !self.regain(&mut regain, attempt, order) {
                break;
            }
        }
        if keep.as_ref().is_some_and(crate::reclaim::Keep::held) {
            drop(keep);
            return try_once(true).unwrap_or(0);
        }
        0
    }

    fn large_try(&self, pages: u64, align_pages: u64) -> Option<usize> {
        let v = self.src.alloc_run(pages, align_pages, Kind::Heap)?;
        self.large_bytes.fetch_add(pages * PAGE, Relaxed);
        self.large_blocks.fetch_add(1, Relaxed);
        if self.src.in_seed(v) {
            self.seed_large.fetch_add(pages, Relaxed);
        }
        Some(v)
    }

    pub(super) fn large_free(&self, v: usize, size: usize, align: usize) -> bool {
        let (pages, align_pages) = large_shape(size, align);
        if self.scattered(v) {
            return self.scatter_free(v, pages);
        }
        let seed = self.src.in_seed(v);
        if v == 0 || !(v as u64).is_multiple_of(align_pages * PAGE) || !self.src.free_run(v, pages, Kind::Heap) {
            return false;
        }
        if seed {
            self.seed_large.fetch_sub(pages, Relaxed);
        }
        self.large_bytes.fetch_sub(pages * PAGE, Relaxed);
        self.large_blocks.fetch_sub(1, Relaxed);
        true
    }
}
