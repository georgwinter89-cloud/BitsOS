//! The HighAtomic reserve (`buddy::highatomic`): its size and place, who may take from it, where its frames go
//! back, what counts as free, and compaction passing it.
use super::*;
use crate::buddy::reserve_pageblocks;
use crate::reclaim::{self, Reclaim, Reclaimed};
use crate::{Buddy, MigrateType, PAGEBLOCK_FRAMES, PAGEBLOCK_ORDER};
use std::boxed::Box;

const HA: MigrateType = MigrateType::HighAtomic;
const PB: u64 = PAGEBLOCK_FRAMES * PAGE;
/// The reserve of `reserved`: the top two pageblocks.
const TOP: u64 = 76 * MIB;
const END: u64 = 80 * MIB;
/// Frames of `reserved` outside the reserve.
const ORDINARY: u64 = 30 * PAGEBLOCK_FRAMES;

/// 64 MiB at 16 MiB (32 pageblocks, DMA32) with a reserve of two pageblocks.
fn reserved() -> BuddyFrames {
    let b = staged(&[free(16 * MIB, END)]);
    assert_eq!(b.reserve_highatomic_in(0, 2), 2);
    b
}

fn in_reserve(b: &Buddy, p: u64) -> bool {
    b.pageblock_type(p) == Some(HA)
}

/// Takes every frame outside the reserve as single frames of `kind`.
fn fill(b: &Buddy, kind: Kind) -> Vec<u64> {
    let got: Vec<u64> = core::iter::from_fn(|| b.alloc(0, Zone::Normal, kind)).collect();
    assert_eq!(got.len() as u64, ORDINARY, "everything but the reserve");
    assert!(got.iter().all(|&p| !in_reserve(b, p)));
    got
}

#[test]
fn the_reserve_follows_the_zone_size() {
    let frames = |bytes: u64| bytes / PAGE;
    assert_eq!(reserve_pageblocks(frames(64 * MIB)), 0, "a tiny zone has none");
    assert_eq!(reserve_pageblocks(frames(128 * MIB)), 1, "Linux's cap: 1 % rounded up to a pageblock");
    assert_eq!(reserve_pageblocks(frames(256 * MIB)), 2);
    assert_eq!(reserve_pageblocks(frames(GIB)), 2, "at least two");
    assert_eq!(reserve_pageblocks(frames(3 * GIB)), 6);
    assert_eq!(reserve_pageblocks(frames(21 * GIB)), 42);
    assert_eq!(reserve_pageblocks(frames(24 * GIB)), 48, "1/256: 96 MiB");
}

#[test]
fn the_reserve_is_made_at_the_top_of_each_zone_and_typed() {
    let small = staged(&[free(16 * MIB, END)]);
    assert_eq!(small.reserve_highatomic(), [0, 0], "nothing unless asked in a small allocator");
    assert_eq!(small.stats().highatomic_free_bytes, 0);

    let b = staged(&[free(16 * MIB, 272 * MIB), free(4 * GIB, 4 * GIB + 256 * MIB)]);
    let before = b.stats();
    assert_eq!(b.reserve_highatomic(), [2, 2]);
    assert_eq!(b.reserve_highatomic(), [2, 2], "made once");
    for top in [272 * MIB, 4 * GIB + 256 * MIB] {
        assert!(in_reserve(&b, top - PB) && in_reserve(&b, top - 2 * PB) && !in_reserve(&b, top - 3 * PB));
    }
    for z in 0..2 {
        let i = b.zone_info(z);
        assert_eq!(i.pageblocks[HA as usize], 2);
        assert_eq!(i.pageblocks.iter().sum::<u64>(), 128);
        // Two halves per pageblock: never one free pageblock.
        let mut halves = [0; crate::ORDERS];
        halves[PAGEBLOCK_ORDER as usize - 1] = 4;
        assert_eq!(i.free_by_type[HA as usize], halves);
        assert_eq!(i.free_highatomic, 2 * PAGEBLOCK_FRAMES);
    }
    let s = b.stats();
    assert_eq!(s.free_bytes, before.free_bytes, "still free memory");
    assert_eq!((s.highatomic_free_bytes, s.usable_free_bytes()), (4 * PB, before.free_bytes - 4 * PB));
    b.check().unwrap();
}

#[test]
fn the_mini_pc_reserves_about_96_mib() {
    let b = staged(&crate::fixtures::typed(&crate::fixtures::MINIPC));
    let got = b.reserve_highatomic();
    std::println!("Mini-PC map: {} + {} pageblocks (dma32, normal), zones of {} and {} MiB", got[0], got[1],
        b.zone_info(0).managed_frames * PAGE / MIB, b.zone_info(1).managed_frames * PAGE / MIB);
    assert_eq!(got, [reserve_pageblocks(b.zone_info(0).managed_frames), reserve_pageblocks(b.zone_info(1).managed_frames)]);
    assert!((46..=49).contains(&(got[0] + got[1])), "{got:?}");
    assert_eq!(b.stats().highatomic_free_bytes, (got[0] + got[1]) * PB);
    // The largest blocks stay: the reserve sits at the top of each zone.
    assert!(b.has_free_block(crate::MAX_ORDER));
    b.check().unwrap();
}

#[test]
fn who_may_reclaim_never_takes_the_reserve() {
    let b = reserved();
    let held = fill(&b, Kind::User);
    assert!(reclaim::context_allows());
    for order in [0, 3] {
        for k in [Kind::User, Kind::PageCache, Kind::PageTable, Kind::Stack, Kind::Dma, Kind::Driver, Kind::Other] {
            assert_eq!(b.alloc(order, Zone::Normal, k), None, "{k:?} order {order}");
            assert_eq!(b.alloc_light(order, Zone::Normal, k), None, "{k:?} order {order}, light");
        }
        // The heap while it may still reclaim (its tries before the last).
        let keep = reclaim::keep_reserve().unwrap();
        assert_eq!(b.alloc(order, Zone::Normal, Kind::Heap), None);
        drop(keep);
    }
    // Program and cache memory not even where nothing can be reclaimed.
    let _atomic = ksync::PreemptGuard::new();
    for k in [Kind::User, Kind::PageCache] {
        assert_eq!(b.alloc(0, Zone::Normal, k), None, "{k:?}");
    }
    assert_eq!(b.zone_info(0).free_highatomic, 2 * PAGEBLOCK_FRAMES, "untouched");
    drop(held);
}

#[test]
fn an_unmovable_block_under_a_spin_lock_comes_from_the_reserve() {
    let b = reserved();
    // Fragmented: every other frame free, no two in a row.
    let held = fill(&b, Kind::Other);
    for p in held.iter().step_by(2) {
        b.free(*p, 0, Kind::Other);
    }
    b.drain_all();
    assert!(b.has_free_block(0) && !b.has_free_block(1));
    assert_eq!(b.alloc(3, Zone::Normal, Kind::Other), None, "a caller that may wait compacts or fails");
    let (p, single) = {
        let _lock = ksync::PreemptGuard::new();
        assert!(!reclaim::context_allows());
        let p = b.alloc(3, Zone::Normal, Kind::Other).expect("an order-3 block out of the reserve");
        // A frame that the ordinary lists still have does not come from the reserve.
        let single = b.alloc(0, Zone::Normal, Kind::PageTable).unwrap();
        assert_eq!(b.alloc(PAGEBLOCK_ORDER, Zone::Normal, Kind::Other), None, "the reserve holds no pageblock");
        (p, single)
    };
    assert!((TOP..END).contains(&p) && p % (8 * PAGE) == 0 && in_reserve(&b, p), "{p:#x}");
    assert!(!in_reserve(&b, single));
    assert_eq!(b.zone_info(0).free_highatomic, 2 * PAGEBLOCK_FRAMES - 8);
    // The heap's last try (no `keep_reserve` slot: null would be next) gets one too, wherever it runs.
    let q = b.alloc(2, Zone::Normal, Kind::Heap).expect("the heap's last try");
    assert!(in_reserve(&b, q));
    assert_eq!(b.stats().used_by_kind[Kind::Heap as usize], 4 * PAGE);
    b.free(q, 2, Kind::Heap);
    b.free(p, 3, Kind::Other);
    assert_eq!(b.zone_info(0).free_highatomic, 2 * PAGEBLOCK_FRAMES);
    b.check().unwrap();
}

#[test]
fn frames_go_back_into_the_reserve_and_never_make_a_free_pageblock() {
    let b = reserved();
    let initial = b.zone_info(0);
    let held = fill(&b, Kind::User);
    // Single frames too once nothing else is left (Linux ALLOC_NON_BLOCK), until the reserve is empty.
    let taken: Vec<u64> = {
        let _lock = ksync::PreemptGuard::new();
        core::iter::from_fn(|| b.alloc(0, Zone::Normal, Kind::Other)).collect()
    };
    assert_eq!(taken.len() as u64, 2 * PAGEBLOCK_FRAMES);
    assert!(taken.iter().all(|&p| in_reserve(&b, p)));
    assert_eq!((b.zone_info(0).free_highatomic, b.stats().free_bytes), (0, 0));
    for &p in &taken {
        b.free(p, 0, Kind::Other);
    }
    let i = b.zone_info(0);
    assert_eq!(i.cached_frames, 0, "past the per-CPU lists");
    assert_eq!(i.free_highatomic, 2 * PAGEBLOCK_FRAMES);
    assert_eq!(i.free_by_type[HA as usize][PAGEBLOCK_ORDER as usize - 1], 4, "merged up to the halves");
    assert!(i.free_blocks[PAGEBLOCK_ORDER as usize..].iter().all(|&n| n == 0), "and no further");
    assert!(in_reserve(&b, TOP) && in_reserve(&b, TOP + PB));
    assert_eq!(b.alloc(0, Zone::Normal, Kind::User), None, "free again, and still not for everyone");
    b.check().unwrap();
    for p in held {
        b.free(p, 0, Kind::User);
    }
    b.drain_all();
    let end = b.zone_info(0);
    assert_eq!((end.free_blocks, end.free_by_type, end.pageblocks[HA as usize]), (initial.free_blocks, initial.free_by_type, 2));
    b.check().unwrap();
}

struct Nothing;
impl Reclaim for Nothing {
    fn reclaim(&self, _pages: u64, _thorough: bool) -> Reclaimed {
        Reclaimed::default()
    }
}

#[test]
fn the_reserve_is_not_usable_free_memory() {
    let f: &'static BuddyFrames = Box::leak(Box::new(reserved()));
    let b: &'static Buddy = f;
    assert_eq!(b.stats().usable_free_bytes(), ORDINARY * PAGE);
    // The min watermark's share is kept out of what is free without the reserve: a program stops there.
    assert!(b.set_reclaimer(Box::leak(Box::new(Nothing))));
    let min = b.reserve_frames()[0];
    assert!(min > 0 && min < PAGEBLOCK_FRAMES);
    let light: Vec<u64> = core::iter::from_fn(|| b.alloc_light(0, Zone::Normal, Kind::User)).collect();
    let i = b.zone_info(0);
    let left = i.free_frames + i.cached_frames - i.free_highatomic;
    assert!(left + 1 >= min && left <= min + crate::pcp::BATCH as u64, "{left} left, min {min}");
    assert_eq!(light.len() as u64 + left, ORDINARY);
    // Nothing reclaimable: the program takes the rest, and still not the reserve.
    let rest: Vec<u64> = core::iter::from_fn(|| b.alloc(0, Zone::Normal, Kind::User)).collect();
    assert_eq!(rest.len() as u64, left);
    b.drain_all();
    let s = b.stats();
    assert_eq!((s.free_bytes, s.highatomic_free_bytes, s.usable_free_bytes()), (2 * PB, 2 * PB, 0));
    assert!(!b.zone_watermark_ok(0, 0, 0) && !b.order_balanced(0), "kswapd goes by what is usable");
    assert!(!b.has_free_block(0));
    assert_eq!(b.fragmentation_score(), 0, "the reserve's halves are not fragmentation");
    assert_eq!(b.census(3).largest_free, None);
    assert_eq!(reclaim::mem_available(s.total_bytes, s.usable_free_bytes(), 0), 0);
    b.check().unwrap();
}

#[test]
fn compaction_passes_the_reserve() {
    // Three pageblocks of pinned cache below one reserved pageblock: the scanner reads the three and no chunk of
    // the reserve, and the reserve's free blocks are not the block it was asked for.
    let (b, c) = super::compact::cached_below_reserve(8, 1);
    for i in 0..3 * PAGEBLOCK_FRAMES {
        c.pin(2 * MIB + i * PAGE);
    }
    assert!(!b.compact_direct(3));
    assert_eq!(b.compact_stats().migrate_scanned, 3 * PAGEBLOCK_FRAMES);
    assert_eq!(b.zone_info(0).free_highatomic, PAGEBLOCK_FRAMES);
    b.check().unwrap();
}

#[test]
fn a_random_run_with_a_reserve_keeps_it_apart() {
    let b = reserved();
    let initial = b.zone_info(0);
    let mut rng = Rng(0x5EED_11);
    let mut live: Vec<(u64, u32, Kind, bool)> = Vec::new();
    let kinds = [Kind::User, Kind::PageCache, Kind::Heap, Kind::PageTable, Kind::Other];
    for step in 0..40_000 {
        if live.is_empty() || rng.below(100) < 62 {
            let order = if rng.below(4) == 0 { rng.below(7) as u32 } else { 0 };
            let kind = kinds[rng.below(kinds.len() as u64) as usize];
            let atomic = rng.below(3) == 0;
            // The heap as the heap asks: with its slot unless its caller holds a spin lock.
            let keep = if kind == Kind::Heap && !atomic { reclaim::keep_reserve() } else { None };
            let lock = atomic.then(ksync::PreemptGuard::new);
            let got = b.alloc(order, Zone::Normal, kind);
            drop((lock, keep));
            if let Some(p) = got {
                if in_reserve(&b, p) {
                    assert!(atomic && MigrateType::of(kind) == MigrateType::Unmovable, "step {step}: {kind:?} order {order} in the reserve");
                }
                live.push((p, order, kind, atomic));
            }
        } else {
            let (p, o, k, _) = live.swap_remove(rng.below(live.len() as u64) as usize);
            b.free(p, o, k);
        }
        if step % 4000 == 0 {
            b.check().unwrap();
        }
    }
    assert!(live.iter().any(|&(p, ..)| in_reserve(&b, p)), "the run never reached the reserve");
    for (p, o, k, _) in live {
        b.free(p, o, k);
    }
    b.drain_all();
    let end = b.zone_info(0);
    assert_eq!((end.free_blocks, end.free_highatomic), (initial.free_blocks, initial.free_highatomic), "everything merged back, the reserve in halves");
    assert!(in_reserve(&b, TOP) && in_reserve(&b, TOP + PB));
    b.check().unwrap();
}
