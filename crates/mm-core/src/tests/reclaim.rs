//! MM-9: direct reclaim in the slow path (only for program and cache memory, bounded rounds, counted) and the
//! watermark / MemAvailable arithmetic.
use super::*;
use crate::reclaim::{self, DirectStats, Reclaim, Reclaimed, Watermarks};
use crate::Buddy;
use std::boxed::Box;
use std::sync::Mutex;

/// Stands in for the page cache: holds frames it allocated as `PageCache` and gives some back when asked.
struct Holder {
    b: &'static Buddy,
    frames: Mutex<Vec<u64>>,
    calls: Mutex<Vec<(u64, bool)>>,
    busy: Mutex<bool>,
}

impl Reclaim for Holder {
    fn reclaim(&self, pages: u64, thorough: bool) -> Reclaimed {
        self.calls.lock().unwrap().push((pages, thorough));
        let mut f = self.frames.lock().unwrap();
        let mut n = 0;
        while n < pages {
            let Some(p) = f.pop() else { break };
            self.b.free(p, 0, Kind::PageCache);
            n += 1;
        }
        Reclaimed { freed: n, busy: *self.busy.lock().unwrap() }
    }
}

/// A 4 MiB allocator whose every frame the holder owns as cache, with the holder installed.
fn full() -> (&'static Buddy, &'static Holder) {
    let f: &'static BuddyFrames = Box::leak(Box::new(staged(&[free(MIB, 5 * MIB)])));
    let b: &'static Buddy = f;
    let h: &'static Holder = Box::leak(Box::new(Holder { b, frames: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()), busy: Mutex::new(false) }));
    while let Some(p) = b.alloc(0, Zone::Dma32, Kind::PageCache) {
        h.frames.lock().unwrap().push(p);
    }
    assert!(b.set_reclaimer(h));
    assert!(!b.set_reclaimer(h), "installed once");
    (b, h)
}

#[test]
fn program_memory_takes_cache_pages_back() {
    let (b, h) = full();
    let held = h.frames.lock().unwrap().len() as u64;
    assert!(held > 900, "the cache holds the allocator: {held}");
    let p = b.alloc(0, Zone::Normal, Kind::User).expect("a user frame from reclaimed cache");
    assert_eq!(*h.calls.lock().unwrap(), [(reclaim::CLUSTER, false)], "one light round of a cluster");
    assert_eq!(b.direct_reclaim(), DirectStats { runs: 1, pages: 32, rescued: 1, failed: 0, dips: 0 });
    assert_eq!(b.stats().used_by_kind[Kind::PageCache as usize], (held - 32) * PAGE);
    // The other 31 frames come without asking again (they sit in this CPU's list).
    let more: Vec<u64> = (0..31).map(|_| b.alloc(0, Zone::Normal, Kind::User).expect("freed by the round")).collect();
    assert_eq!(h.calls.lock().unwrap().len(), 1);
    b.free(p, 0, Kind::User);
    for q in more {
        b.free(q, 0, Kind::User);
    }
    b.check().unwrap();
}

#[test]
fn page_cache_and_blocks_reclaim_too() {
    let (b, h) = full();
    assert!(b.alloc(0, Zone::Normal, Kind::PageCache).is_some(), "a cache page makes room for itself");
    let p = b.alloc(3, Zone::Dma32, Kind::User).expect("an order-3 block after reclaim");
    assert_eq!(p % (8 * PAGE), 0);
    let calls = h.calls.lock().unwrap().clone();
    assert!(calls.contains(&(reclaim::CLUSTER, false)), "{calls:?}");
    assert!(b.direct_reclaim().rescued >= 1 && b.direct_reclaim().failed == 0);
}

#[test]
fn kernel_kinds_never_reclaim() {
    let (b, h) = full();
    for k in [Kind::Heap, Kind::PageTable, Kind::Dma, Kind::Stack, Kind::Driver, Kind::Other] {
        assert!(b.alloc(0, Zone::Normal, k).is_none(), "{k:?} found a frame");
    }
    assert!(h.calls.lock().unwrap().is_empty(), "the reclaimer was asked for a kernel kind");
    assert_eq!(b.direct_reclaim(), DirectStats::default());
    assert!(b.reports().alloc_fails.load(std::sync::atomic::Ordering::Relaxed) >= 6);
}

#[test]
fn nothing_left_fails_after_the_thorough_round() {
    let (b, h) = full();
    let all: Vec<u64> = core::mem::take(&mut *h.frames.lock().unwrap());
    // The cache "holds" nothing reclaimable now (the frames stay allocated, as if mapped or dirty).
    assert!(b.alloc(0, Zone::Normal, Kind::User).is_none());
    assert_eq!(*h.calls.lock().unwrap(), [(32, false), (32, true)], "light, then thorough, then the reserve (empty)");
    assert_eq!(b.direct_reclaim(), DirectStats { runs: 1, pages: 0, rescued: 0, failed: 1, dips: 1 });
    for p in all {
        b.free(p, 0, Kind::PageCache);
    }
    b.check().unwrap();
}

#[test]
fn without_a_reclaimer_nothing_changes() {
    let b = staged(&[free(MIB, 2 * MIB)]);
    while b.alloc(0, Zone::Normal, Kind::User).is_some() {}
    assert_eq!(b.direct_reclaim(), DirectStats::default());
}

#[test]
fn watermarks_follow_linux() {
    let f = |gib_frames: u64| reclaim::watermarks(gib_frames);
    // 32 GiB: min_free_kbytes = sqrt(32 Mi KiB x 16) = 23170 KiB; gap = 0.1 % of RAM.
    assert_eq!(f(8 << 20), Watermarks { min: 5792, low: 5792 + 8388, high: 5792 + 2 * 8388 });
    // 512 MiB: 2896 KiB; gap = min/4 (larger than 0.1 %).
    assert_eq!(f(131_072), Watermarks { min: 724, low: 905, high: 1086 });
    // Tiny: the 128 KiB floor.
    assert_eq!(f(256), Watermarks { min: 32, low: 40, high: 48 });
    // Huge: the 256 MiB cap on min.
    assert_eq!(f(1 << 30).min, 65_536);
    assert_eq!(f(0), Watermarks { min: 0, low: 0, high: 0 });
}

#[test]
fn mem_available_counts_reclaimable_cache() {
    let total = 32 * GIB;
    let w = reclaim::watermarks(total / PAGE);
    let (free, file) = (GIB, 10 * GIB);
    let want = (free + file) / PAGE - w.high - w.low;
    assert_eq!(reclaim::mem_available(total, free, file), want * PAGE);
    // Small cache: half of it stays.
    let small = 8 * w.low * PAGE;
    assert_eq!(reclaim::mem_available(total, free, small), (free + small) - (w.high + w.low) * PAGE, "keeps the low watermark");
    let tiny = 4 * PAGE;
    assert_eq!(reclaim::mem_available(total, free, tiny), free + tiny - (w.high + 2) * PAGE, "keeps half of a tiny cache");
    assert_eq!(reclaim::mem_available(total, w.high * PAGE / 2, 0), 0, "below the reserve: 0, never negative");
}

#[test]
fn the_reserve_stays_for_kernel_kinds() {
    let (b, h) = full();
    assert_eq!(b.reserve_frames()[0], reclaim::watermarks(1024).min, "the min watermark of the 1024 frames");
    // 40 frames come back outside reclaim (a file was deleted): fewer than the reserve of 64.
    let back: Vec<u64> = (0..40).filter_map(|_| h.frames.lock().unwrap().pop()).collect();
    for p in back {
        b.free(p, 0, Kind::PageCache);
    }
    b.drain_all();
    let p = b.alloc(0, Zone::Normal, Kind::User).expect("a user frame");
    assert_eq!(*h.calls.lock().unwrap(), [(32, false)], "program memory below the reserve reclaims first");
    let k: Vec<u64> = (0..40).map(|_| b.alloc(0, Zone::Normal, Kind::Heap).expect("the reserve serves kernel kinds")).collect();
    assert_eq!(h.calls.lock().unwrap().len(), 1, "kernel kinds never reclaim");
    b.free(p, 0, Kind::User);
    for q in k {
        b.free(q, 0, Kind::Heap);
    }
    b.check().unwrap();
}

#[test]
fn busy_caches_get_more_rounds_then_the_reserve() {
    let (b, h) = full();
    let all: Vec<u64> = core::mem::take(&mut *h.frames.lock().unwrap());
    *h.busy.lock().unwrap() = true;
    let back: Vec<u64> = all[..8].to_vec();
    for p in &back {
        b.free(*p, 0, Kind::PageCache);
    }
    b.drain_all();
    let p = b.alloc(0, Zone::Normal, Kind::User).expect("the reserve after the last round");
    assert_eq!(*h.calls.lock().unwrap(), [(32, false), (32, true), (32, true), (32, true)], "light, then thorough while busy");
    assert_eq!(b.direct_reclaim(), DirectStats { runs: 1, pages: 0, rescued: 1, failed: 0, dips: 1 });
    // Not busy and nothing found: the next hits skip the scan.
    *h.busy.lock().unwrap() = false;
    let q = b.alloc(0, Zone::Normal, Kind::User).expect("from the per-CPU list");
    b.free(p, 0, Kind::User);
    b.free(q, 0, Kind::User);
    for f in &all[8..] {
        b.free(*f, 0, Kind::PageCache);
    }
    b.check().unwrap();
}
