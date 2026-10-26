//! MM-9: the frame allocator's side of page-cache reclaim (Linux mm/vmscan.c and mm/page_alloc.c, small).
//!
//! The page cache lives above mm-core (crate vfs), so the allocator knows it only as a registered `Reclaim`.
//! When an allocation of a reclaim-allowed kind found no frame (both attempts and the per-CPU drain failed:
//! the slow path, the fast path is unchanged), the buddy asks it for clean, unmapped cache pages and tries
//! again (`__alloc_pages_direct_reclaim`, linux-6.6/mm/page_alloc.c:3639): a light round first, then a thorough
//! one, no more (bounded, B4: the reclaimer never waits for a lock and never allocates). The kswapd-like
//! thread (crate mm, `mm.kswapd`) keeps free memory between the watermarks computed here in the background.

use crate::{Kind, PAGE};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The page cache's side (crate vfs implements it, crate mm installs it).
pub trait Reclaim: Sync {
    /// Takes back up to `pages` clean, unmapped page-cache pages. Must not wait for a lock (bounded try-locks
    /// only) or allocate frames: the allocator may be called with any lock of its caller held. `thorough`: look
    /// at every cached page (the rounds before the allocation would fail).
    fn reclaim(&self, pages: u64, thorough: bool) -> Reclaimed;
}

/// What one reclaim round did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    pub freed: u64,
    /// A cache was passed over because someone held its lock, or the round's scan budget ran out: nothing freed
    /// does not mean nothing reclaimable.
    pub busy: bool,
}

/// Direct reclaim rounds at most: one light, then thorough ones while caches were busy or a budget ran out.
pub const ROUNDS: u32 = 4;

/// `SWAP_CLUSTER_MAX`: the pages one direct reclaim round asks for at least.
pub const CLUSTER: u64 = 32;

/// Whether an allocation of `kind` may reclaim directly. Only program memory and the page cache itself: they are
/// allocated in thread context (faults, file reads and writes), never under the heap's lock (heap refills,
/// `Kind::Heap`), a page-table lock (`PageTable`), a driver's DMA setup (`Dma`, `Driver`) or the scheduler's
/// spawn path (`Stack`); those rely on the background thread keeping the watermark gap free (as Linux's
/// atomic allocations rely on the min reserve).
pub fn direct_allowed(kind: Kind) -> bool {
    matches!(kind, Kind::User | Kind::PageCache)
}

/// Pages a direct reclaim round asks for an allocation of 2^`order` frames (a block needs its neighbours free
/// too, so twice the block, at least a cluster).
pub fn direct_target(order: u32) -> u64 {
    CLUSTER.max(2u64 << order.min(20))
}

/// Free-frame watermarks of the whole machine in frames (`__setup_per_zone_wmarks`, one zone's worth).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Watermarks {
    pub min: u64,
    pub low: u64,
    pub high: u64,
}

/// Linux's watermarks for `total` managed frames: min = `min_free_kbytes` = sqrt(16 x RAM in KiB), clamped to
/// 128 KiB..256 MiB (linux-6.6/mm/page_alloc.c:5748); the kswapd gaps low - min and high - low are each the
/// larger of min/4 and 0.1 % of RAM (`watermark_scale_factor` 10, :5682). 32 GB: min 22.6 MiB, low 55 MiB,
/// high 87 MiB; 512 MiB: min 2.8 MiB, low 3.5 MiB, high 4.2 MiB.
pub fn watermarks(total: u64) -> Watermarks {
    let kib = total.saturating_mul(PAGE / 1024);
    let min_kib = isqrt(kib.saturating_mul(16)).clamp(128, 262_144);
    let min = (min_kib / (PAGE / 1024)).min(total);
    let gap = (min / 4).max(total.saturating_mul(10) / 10_000);
    Watermarks { min, low: min.saturating_add(gap), high: min.saturating_add(gap.saturating_mul(2)) }
}

/// `si_mem_available` (linux-6.6/mm/show_mem.c:32) in bytes: free memory minus the reserve (the high
/// watermark, `calculate_totalreserve_pages`), plus the file pages of the page cache minus what has to stay (half
/// of it, at most the low watermark). `file`: page-cache bytes that are not shared memory (tmpfs never goes:
/// no swap). V2 has no reclaimable slab to add.
pub fn mem_available(total_bytes: u64, free_bytes: u64, file_bytes: u64) -> u64 {
    let w = watermarks(total_bytes / PAGE);
    let (free, file) = (free_bytes / PAGE, file_bytes / PAGE);
    let keep = (file / 2).min(w.low);
    (free + file).saturating_sub(w.high.min(total_bytes / PAGE) + keep) * PAGE
}

fn isqrt(n: u64) -> u64 {
    n.isqrt()
}

/// Direct reclaim counts of one allocator (`Buddy::direct_reclaim`).
#[derive(Default)]
pub struct DirectCounters {
    /// Allocations that went into direct reclaim.
    pub runs: AtomicU64,
    /// Pages the reclaimer freed for them.
    pub pages: AtomicU64,
    /// Of `runs`, those that got their frames afterwards.
    pub rescued: AtomicU64,
    /// Of `runs`, those that failed all the same (ENOMEM while nothing reclaimable was left, or only busy pages).
    pub failed: AtomicU64,
    /// Program or cache allocations that took frames of the reserve (nothing was reclaimable).
    pub dips: AtomicU64,
}

/// A copy of `DirectCounters`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectStats {
    pub runs: u64,
    pub pages: u64,
    pub rescued: u64,
    pub failed: u64,
    pub dips: u64,
}

impl DirectCounters {
    pub const fn new() -> DirectCounters {
        DirectCounters { runs: AtomicU64::new(0), pages: AtomicU64::new(0), rescued: AtomicU64::new(0), failed: AtomicU64::new(0), dips: AtomicU64::new(0) }
    }
    pub fn get(&self) -> DirectStats {
        DirectStats {
            runs: self.runs.load(Relaxed),
            pages: self.pages.load(Relaxed),
            rescued: self.rescued.load(Relaxed),
            failed: self.failed.load(Relaxed),
            dips: self.dips.load(Relaxed),
        }
    }
}

klog::counter!(DIRECT_RUNS, "mm.reclaim.direct_runs");
klog::counter!(DIRECT_PAGES, "mm.reclaim.direct_pages");
klog::counter!(DIRECT_RESCUED, "mm.reclaim.direct_rescued");
klog::counter!(DIRECT_FAILED, "mm.reclaim.direct_failed");

/// Bumps the global klog counters (the boot allocator only).
pub(crate) fn count_global(pages: u64, outcome: Option<bool>) {
    match outcome {
        None => {
            DIRECT_RUNS.inc();
        }
        Some(true) => DIRECT_RESCUED.inc(),
        Some(false) => DIRECT_FAILED.inc(),
    }
    if pages > 0 {
        DIRECT_PAGES.add(pages);
    }
}

/// Installs the page cache's reclaimer into the boot allocator (crate mm's core initcall). False: no boot
/// allocator, or one was installed already.
pub fn set_reclaimer(r: &'static dyn Reclaim) -> bool {
    crate::boot_buddy().is_some_and(|b| b.set_reclaimer(r))
}

/// The boot allocator's direct reclaim counts (zeros without one).
pub fn direct_stats() -> DirectStats {
    crate::boot_buddy().map(|b| b.direct_reclaim()).unwrap_or_default()
}
