//! MEMCRASH row 1: Linux's watermark boost (linux-7.3.1/mm/page_alloc.c:2175 `boost_watermark`, :2321 its call in
//! `try_to_claim_block`, :3433 the wake in `rmqueue`; mm/vmscan.c:7186-7356 `balance_pgdat`).
//!
//! When an allocation has to take a block out of another migrate type's pageblock, memory is about to mix
//! movable and unmovable frames for good. Linux then raises the zone's watermarks by a pageblock (up to
//! `watermark_boost_factor` 15000 = 150 % of the high watermark) and wakes kswapd, which reclaims to the boosted
//! high watermark although the node may look balanced, drops the boost, and wakes kcompactd for the pageblock
//! order: free memory grows a little and is put back in large blocks before the next small kernel block finds
//! none. BitsOS keeps one set of watermarks for the machine (`reclaim::watermarks`), so the boost is one number;
//! its allocator has no low-watermark check on the fast path, so the boost works where Linux's does its work, in
//! kswapd's target. The fixed reserve (the min watermark's frames) never changes.
//!
//! Raised by the fallback (`Buddy::try_claim`) and by a kernel heap allocation of order > 0 that found no block
//! (`KernelHeap::regain`): the Mini-PC's crash was such a miss above the high watermark, where kswapd slept.

use crate::buddy::PAGEBLOCK_FRAMES;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// Linux `watermark_boost_factor` (per ten thousand of the high watermark).
pub const FACTOR: u64 = 15_000;

/// The boost in frames (Linux `zone->watermark_boost`).
static BOOST: AtomicU64 = AtomicU64::new(0);
/// A boost was raised since kswapd was last woken for one (Linux `ZONE_BOOSTED_WATERMARK`).
static BOOSTED: AtomicBool = AtomicBool::new(false);
/// Managed frames of the machine (set with the reclaimer; 0 before: no boost).
static TOTAL: AtomicU64 = AtomicU64::new(0);

klog::counter!(pub RAISED, "mm.frames.boost_raised");
klog::counter!(pub CLEARED, "mm.frames.boost_cleared");

/// The boot allocator's managed frames (`Buddy::set_reclaimer`).
pub(crate) fn set_total(frames: u64) {
    TOTAL.store(frames, Relaxed);
}

/// The largest boost for `total` frames: 150 % of the high watermark, at least a pageblock; 0 where Linux does not
/// boost (a machine of fewer than four pageblocks, no high watermark yet).
pub fn max_boost(total: u64) -> u64 {
    if PAGEBLOCK_FRAMES * 4 > total {
        return 0;
    }
    let high = crate::reclaim::watermarks(total).high;
    if high == 0 {
        return 0;
    }
    (high.saturating_mul(FACTOR) / 10_000).max(PAGEBLOCK_FRAMES)
}

/// Raises the boost by a pageblock up to `max_boost` and marks kswapd to be woken (`wake_if_boosted`, after the
/// zone lock). Lock-free; false when nothing was raised.
pub fn raise() -> bool {
    let max = max_boost(TOTAL.load(Relaxed));
    if max == 0 {
        return false;
    }
    let _ = BOOST.try_update(Relaxed, Relaxed, |b| Some(b.saturating_add(PAGEBLOCK_FRAMES).min(max)));
    RAISED.inc();
    BOOSTED.store(true, Relaxed);
    true
}

/// Wakes kswapd once for the boosts raised since (Linux's test-and-clear of `ZONE_BOOSTED_WATERMARK` at the end of
/// `rmqueue`); deferred by `reclaim::wake_kswapd` where the context may not call the scheduler. One load otherwise.
#[inline]
pub fn wake_if_boosted() {
    if BOOSTED.load(Relaxed) && BOOSTED.swap(false, Relaxed) {
        crate::reclaim::wake_kswapd();
    }
}

/// The boost now in frames (kswapd's snapshot at the start of a look).
pub fn get() -> u64 {
    BOOST.load(Relaxed)
}

/// kswapd's end of a boosted look (Linux: `zone->watermark_boost -= min(boost, zone_boosts[i])`): what it saw at
/// the start comes off (a boost raised meanwhile stays for the next look); then kcompactd is woken for the pageblock
/// order. Returns whether anything was boosted.
pub fn clear(seen: u64) -> bool {
    if seen == 0 {
        return false;
    }
    let _ = BOOST.try_update(Relaxed, Relaxed, |b| Some(b.saturating_sub(seen)));
    CLEARED.inc();
    crate::compact::wake_kcompactd(crate::buddy::PAGEBLOCK_ORDER);
    true
}

/// Tests: the boost back to 0.
pub fn reset_for_test() {
    BOOST.store(0, Relaxed);
    BOOSTED.store(false, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boost_is_capped_at_one_and_a_half_high_watermarks() {
        let total = (24u64 << 30) / crate::PAGE;
        let high = crate::reclaim::watermarks(total).high;
        assert_eq!(max_boost(total), high * FACTOR / 10_000);
        // Fewer than four pageblocks: Linux does not boost.
        assert_eq!(max_boost(PAGEBLOCK_FRAMES * 4 - 1), 0);
        // A small machine still gets one pageblock.
        let small = PAGEBLOCK_FRAMES * 4;
        assert_eq!(max_boost(small), (crate::reclaim::watermarks(small).high * FACTOR / 10_000).max(PAGEBLOCK_FRAMES));
    }

    #[test]
    fn raised_by_pageblocks_and_cleared_by_what_kswapd_saw() {
        reset_for_test();
        let total = (2u64 << 30) / crate::PAGE;
        set_total(total);
        let max = max_boost(total);
        assert!(raise());
        assert_eq!(get(), PAGEBLOCK_FRAMES);
        for _ in 0..1000 {
            raise();
        }
        assert_eq!(get(), max, "never above the cap");
        let seen = get();
        raise();
        assert!(clear(seen));
        assert_eq!(get(), 0, "a raise at the cap adds nothing");
        assert!(!clear(0));
        raise();
        let seen = get();
        raise();
        clear(seen);
        assert_eq!(get(), PAGEBLOCK_FRAMES.min(max), "a raise after kswapd's look stays for the next one");
        reset_for_test();
        set_total(0);
    }
}
