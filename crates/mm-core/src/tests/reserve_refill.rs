//! The HighAtomic reserve after 12.11.2026: who may take it (a heap caller that may wait only on its last try),
//! its refill and the hand-over of pageblocks that long-lived blocks settled in (`buddy::highatomic::refill`),
//! and the tables of callers (`callers`).
use super::*;
use crate::callers::{CallerTable, DEPTH};
use crate::reclaim;
use crate::{Buddy, MigrateType, PAGEBLOCK_FRAMES, PAGEBLOCK_ORDER};
use core::sync::atomic::Ordering::Relaxed;

const HA: MigrateType = MigrateType::HighAtomic;
const PB: u64 = PAGEBLOCK_FRAMES * PAGE;
const BASE: u64 = 16 * MIB;
const END: u64 = 80 * MIB;
/// The reserve of `reserved`: the top four pageblocks.
const TOP: u64 = END - 4 * PB;

/// 64 MiB (32 pageblocks) with a reserve of four pageblocks; everything else taken as single frames of `kind`.
fn reserved(kind: Kind) -> (BuddyFrames, Vec<u64>) {
    let b = staged(&[free(BASE, END)]);
    assert_eq!(b.reserve_highatomic_in(0, 4), 4);
    let held: Vec<u64> = core::iter::from_fn(|| b.alloc(0, Zone::Normal, kind)).collect();
    assert_eq!(held.len() as u64, 28 * PAGEBLOCK_FRAMES);
    (b, held)
}

fn ha_type(b: &Buddy, p: u64) -> bool {
    b.pageblock_type(p) == Some(HA)
}

/// Frees the frames of `held` inside the pageblock at `pb` (it becomes one free pageblock).
fn free_pageblock(b: &Buddy, held: &mut Vec<u64>, pb: u64, kind: Kind) {
    held.retain(|&p| {
        let inside = (pb..pb + PB).contains(&p);
        if inside {
            b.free(p, 0, kind);
        }
        !inside
    });
    b.drain_all();
}

/// Takes `frames` single frames out of the reserve as a caller under a spin lock would, to stay.
fn settle(b: &Buddy, frames: u64) -> Vec<u64> {
    let _lock = ksync::PreemptGuard::new();
    let got: Vec<u64> = (0..frames).map(|_| b.alloc(0, Zone::Normal, Kind::Other).expect("a frame out of the reserve")).collect();
    assert!(got.iter().all(|&p| ha_type(b, p)));
    got
}

#[test]
fn a_heap_caller_that_may_wait_takes_the_reserve_only_on_its_last_try() {
    let (b, _held) = reserved(Kind::User);
    // Counted from here: filling memory ended with a refused frame.
    let before = b.reports().alloc_fails.load(Relaxed);
    let fails = || b.reports().alloc_fails.load(Relaxed) - before;
    let keep = reclaim::keep_reserve().unwrap();
    assert!(reclaim::keeps_reserve() && reclaim::heap_waits() && !reclaim::heap_last_try());
    for order in [0, 1, 3] {
        assert_eq!(b.alloc(order, Zone::Normal, Kind::Heap), None, "order {order}, first tries");
    }
    // From its fourth try on the heap takes the frames below the min watermark: the HighAtomic pageblocks stay
    // closed (12.11.2026: they opened here).
    keep.dip();
    assert!(!reclaim::keeps_reserve() && reclaim::heap_waits() && !reclaim::heap_last_try());
    for order in [0, 1, 3] {
        assert_eq!(b.alloc(order, Zone::Normal, Kind::Heap), None, "order {order}, dipped");
    }
    assert_eq!((b.zone_info(0).free_highatomic, b.reserve_stats().total_takes()), (4 * PAGEBLOCK_FRAMES, 0));
    assert_eq!(fails(), 0, "a try the heap repeats is not a failed allocation");
    // The try after which null is next.
    keep.last();
    assert!(!reclaim::keeps_reserve() && !reclaim::heap_waits() && reclaim::heap_last_try());
    let p = b.alloc(3, Zone::Normal, Kind::Heap).expect("the last try");
    assert!(ha_type(&b, p));
    let s = b.reserve_stats();
    assert_eq!((s.takes[3], s.pages[Kind::Heap as usize], s.last_tries), (1, 8, 1));
    assert_eq!(b.alloc(PAGEBLOCK_ORDER, Zone::Normal, Kind::Heap), None);
    assert_eq!(fails(), 1, "the last try's failure is one");
    drop(keep);
    assert!(!reclaim::heap_waits() && !reclaim::heap_last_try());
    b.free(p, 3, Kind::Heap);
    b.check().unwrap();
}

#[test]
fn an_allocation_nested_in_a_waiting_one_is_not_held_to_its_patience() {
    let (b, _held) = reserved(Kind::User);
    let outer = reclaim::keep_reserve().unwrap();
    outer.dip();
    {
        // The grace queue growing inside the outer one's reclaim round, an interrupt handler's block.
        let inner = reclaim::keep_reserve().unwrap();
        assert!(!inner.held() && !reclaim::heap_waits());
        let p = b.alloc(1, Zone::Normal, Kind::Heap).expect("the inner allocation");
        assert!(ha_type(&b, p));
        assert_eq!(b.reserve_stats().last_tries, 0, "counted as a caller that cannot wait");
        b.free(p, 1, Kind::Heap);
    }
    assert!(reclaim::heap_waits() && !reclaim::keeps_reserve(), "the outer one waits again, dipped as before");
    assert_eq!(b.alloc(1, Zone::Normal, Kind::Heap), None);
    drop(outer);
}

#[test]
fn the_heap_keeps_a_waiting_caller_off_the_reserve_until_null_is_next() {
    // A heap over 16 MiB with a reserve of two pageblocks and nothing else free: no reclaimer, so a caller that
    // may wait runs out of things to try at once and makes its last try; one under a spin lock takes at once.
    let b = super::heap::host_buddy(16);
    // Host memory lies wherever the host put it: the zone that has it.
    let zone = (0..2).find(|&z| b.zone_info(z).managed_frames > 0).unwrap();
    assert_eq!(b.reserve_highatomic_in(zone, 2), 2);
    let _rest: Vec<u64> = core::iter::from_fn(|| b.alloc(0, Zone::Normal, Kind::User)).collect();
    let heap = super::heap::heap(Some(b), None, 0);
    let p = heap.alloc(super::heap::lay(2048, 8));
    assert!(!p.is_null());
    let s = b.reserve_stats();
    assert_eq!((s.takes[3], s.last_tries), (1, 1), "one 8-frame slab, by the last try: {s:?}");
    let q = {
        let _lock = ksync::PreemptGuard::new();
        heap.alloc(super::heap::lay(17296, 8))
    };
    assert!(!q.is_null());
    let s = b.reserve_stats();
    assert_eq!((s.takes[3], s.last_tries, s.pages[Kind::Heap as usize]), (2, 1, 16));
    assert_eq!(b.zone_info(zone).free_highatomic, 2 * PAGEBLOCK_FRAMES - 8 - 5);
    // SAFETY: both blocks came from this heap with these layouts and are not used again.
    unsafe {
        heap.dealloc(q, super::heap::lay(17296, 8));
        heap.dealloc(p, super::heap::lay(2048, 8));
    }
}

#[test]
fn a_reserve_above_its_low_mark_is_left_alone() {
    let (b, mut held) = reserved(Kind::User);
    let taken = settle(&b, PAGEBLOCK_FRAMES);
    free_pageblock(&b, &mut held, BASE, Kind::User);
    assert_eq!(b.zone_info(0).free_highatomic, 3 * PAGEBLOCK_FRAMES, "at the low mark, not below");
    assert!(!b.reserve_low());
    assert_eq!(b.refill_reserve(&mut || {}), crate::buddy::Refill::default());
    assert!(b.has_free_block(PAGEBLOCK_ORDER), "the free pageblock stayed");
    for p in taken {
        b.free(p, 0, Kind::Other);
    }
    b.check().unwrap();
}

#[test]
fn a_low_reserve_takes_free_pageblocks_and_hands_over_what_settled() {
    let (b, mut held) = reserved(Kind::User);
    // Long-lived frames in the reserve: all of its first pageblock and a few of the second.
    let taken = settle(&b, PAGEBLOCK_FRAMES + 40);
    assert!((TOP..TOP + PB).contains(&taken[0]) && (TOP + PB..TOP + 2 * PB).contains(&taken[PAGEBLOCK_FRAMES as usize]));
    assert!(b.reserve_low());
    // Nothing is free and there is nothing to make a pageblock of: short, and nothing changed.
    let r = b.refill_reserve(&mut || {});
    assert_eq!((r.reserved, r.handed, r.short), (0, 0, 1));
    assert_eq!(b.zone_info(0).pageblocks[HA as usize], 4);
    // Two pageblocks come free elsewhere.
    free_pageblock(&b, &mut held, BASE + 2 * PB, Kind::User);
    free_pageblock(&b, &mut held, BASE + 5 * PB, Kind::User);
    let usable = b.stats().usable_free_bytes();
    let r = b.refill_reserve(&mut || {});
    // The pageblock with the least free leaves, a free one joins; then all but 1/16 is free and the pass stops:
    // the 40 frames in the second pageblock stay where they are.
    assert_eq!((r.reserved, r.handed, r.short), (1, 1, 0));
    let i = b.zone_info(0);
    assert_eq!(i.pageblocks[HA as usize], 4, "never more than its size");
    assert_eq!(i.free_highatomic, 4 * PAGEBLOCK_FRAMES - 40);
    assert!(!b.reserve_low());
    // The pageblock the frames settled in is Unmovable now.
    assert_eq!(b.pageblock_type(TOP), Some(MigrateType::Unmovable));
    assert!(ha_type(&b, TOP + PB) && ha_type(&b, TOP + 2 * PB) && ha_type(&b, TOP + 3 * PB));
    assert!(ha_type(&b, BASE + 2 * PB) && !ha_type(&b, BASE + 5 * PB), "one free pageblock joined, the other stayed free");
    assert_eq!(b.stats().usable_free_bytes(), usable - PB, "one pageblock in; the one that left had nothing free");
    assert!(b.has_free_block(PAGEBLOCK_ORDER));
    let s = b.reserve_stats();
    assert_eq!((s.refilled, s.handed, s.pageblocks[0], s.target[0]), (1, 1, 4, 4));
    b.check().unwrap();
    // What settled goes back to ordinary memory when it is freed, not into the reserve.
    let usable = b.stats().usable_free_bytes();
    for &p in &taken[..PAGEBLOCK_FRAMES as usize] {
        b.free(p, 0, Kind::Other);
    }
    b.drain_all();
    assert_eq!(b.zone_info(0).free_highatomic, 4 * PAGEBLOCK_FRAMES - 40);
    assert_eq!(b.stats().usable_free_bytes(), usable + PB, "the handed-over pageblock is ordinary free memory");
    // What is still in the reserve goes back into it.
    for &p in &taken[PAGEBLOCK_FRAMES as usize..] {
        b.free(p, 0, Kind::Other);
    }
    assert_eq!(b.zone_info(0).free_highatomic, 4 * PAGEBLOCK_FRAMES);
    // And the reserve serves again.
    let _lock = ksync::PreemptGuard::new();
    let p = b.alloc_exact(5, 1, Zone::Normal, Kind::Heap).unwrap();
    assert!(b.free_exact(p, 5, Kind::Heap));
    b.check().unwrap();
}

#[test]
fn a_pass_is_bounded_and_the_next_one_goes_on() {
    // Twelve pageblocks of reserve in 64 MiB, every one of them with frames that stay.
    let b = staged(&[free(BASE, END)]);
    assert_eq!(b.reserve_highatomic_in(0, 12), 12);
    let mut held: Vec<u64> = core::iter::from_fn(|| b.alloc(0, Zone::Normal, Kind::User)).collect();
    let taken: Vec<u64> = {
        let _lock = ksync::PreemptGuard::new();
        (0..12 * PAGEBLOCK_FRAMES).filter_map(|_| b.alloc(0, Zone::Normal, Kind::PageTable)).collect()
    };
    assert_eq!(taken.len() as u64, 12 * PAGEBLOCK_FRAMES);
    // Half of each pageblock's frames go back: every pageblock half used.
    for p in taken.iter().filter(|&&p| (p / PAGE) % 2 == 0) {
        b.free(*p, 0, Kind::PageTable);
    }
    assert_eq!(b.zone_info(0).free_highatomic, 6 * PAGEBLOCK_FRAMES);
    for k in 0..12 {
        free_pageblock(&b, &mut held, BASE + k * PB, Kind::User);
    }
    let mut passes = 0;
    while b.reserve_low() {
        let r = b.refill_reserve(&mut || {});
        assert_eq!((r.reserved, r.handed, r.short), (4, 4, 0), "a quarter of twelve, and one: pass {passes}");
        assert_eq!(b.zone_info(0).pageblocks[HA as usize], 12);
        passes += 1;
    }
    // 6 of 12 free; each pass adds 4 x 256 frames: at the low mark (9 of 12) after two passes.
    assert_eq!(passes, 2);
    assert_eq!(b.zone_info(0).free_highatomic, 10 * PAGEBLOCK_FRAMES);
    b.check().unwrap();
}

#[test]
fn a_reserve_that_misses_a_pageblock_gets_it_without_handing_one_over() {
    let b = staged(&[free(BASE, END)]);
    assert_eq!(b.reserve_highatomic_in(0, 2), 2);
    let mut held: Vec<u64> = core::iter::from_fn(|| b.alloc(0, Zone::Normal, Kind::User)).collect();
    // Asked for three with nothing free: two it stays, three is what the refill aims at.
    assert_eq!(b.reserve_highatomic_in(0, 3), 2);
    assert!(b.reserve_low());
    free_pageblock(&b, &mut held, BASE + 7 * PB, Kind::User);
    let r = b.refill_reserve(&mut || {});
    assert_eq!((r.reserved, r.handed, r.short), (1, 0, 0));
    let i = b.zone_info(0);
    assert_eq!((i.pageblocks[HA as usize], i.free_highatomic), (3, 3 * PAGEBLOCK_FRAMES));
    assert!(ha_type(&b, BASE + 7 * PB) && !b.reserve_low());
    b.check().unwrap();
}

#[test]
fn where_no_pageblock_is_free_the_refill_makes_one_of_clean_cache() {
    // 16 MiB of clean cache below a reserve of two pageblocks (the drop-clean migrator of the page cache).
    let (b, _cache) = super::compact::cached_below_reserve(16, 2);
    let top = 18 * MIB - 2 * PB;
    let taken = settle(b, PAGEBLOCK_FRAMES + 8);
    assert!(b.reserve_low() && !b.has_free_block(PAGEBLOCK_ORDER));
    let r = b.refill_reserve(&mut || {});
    assert_eq!((r.reserved, r.handed, r.short), (1, 1, 0));
    assert_eq!(b.compact_stats().dropped, PAGEBLOCK_FRAMES, "one pageblock of cache, no more");
    let i = b.zone_info(0);
    assert_eq!((i.pageblocks[HA as usize], i.free_highatomic), (2, 2 * PAGEBLOCK_FRAMES - 8));
    assert_eq!(b.pageblock_type(top), Some(MigrateType::Unmovable), "the full one left");
    assert!(ha_type(b, top + PB) && ha_type(b, 2 * MIB), "the first pageblock of the cache joined");
    assert!(!b.has_free_block(PAGEBLOCK_ORDER), "the pageblock that was made is the reserve's, nobody else's");
    for p in taken {
        b.free(p, 0, Kind::Other);
    }
    b.check().unwrap();
}

#[test]
fn a_random_run_with_refills_keeps_the_reserve_at_its_size() {
    let (b, mut held) = reserved(Kind::User);
    let mut rng = Rng(0x1211_2026);
    let mut live: Vec<(u64, u32)> = Vec::new();
    let mut next_free = 0u64;
    for step in 0..30_000 {
        if rng.below(100) < 60 {
            let _lock = ksync::PreemptGuard::new();
            let order = rng.below(5) as u32;
            if let Some(p) = b.alloc(order, Zone::Normal, Kind::Other) {
                live.push((p, order));
            }
        } else if !live.is_empty() {
            let (p, o) = live.swap_remove(rng.below(live.len() as u64) as usize);
            b.free(p, o, Kind::Other);
        }
        if step % 250 == 0 {
            // A pageblock of program memory goes now and then: what a refill can take.
            if step % 1000 == 0 && next_free < 24 {
                free_pageblock(&b, &mut held, BASE + next_free * PB, Kind::User);
                next_free += 1;
            }
            b.refill_reserve(&mut || {});
            let i = b.zone_info(0);
            assert!(i.pageblocks[HA as usize] == 4 && i.free_highatomic <= 4 * PAGEBLOCK_FRAMES, "step {step}: {i:?}");
            b.check().unwrap();
        }
    }
    let s = b.reserve_stats();
    std::println!("random run: {} takes, {} refilled, {} handed over", s.total_takes(), s.refilled, s.handed);
    assert!(b.reserve_stats().handed > 0, "the run never handed a pageblock over");
    for (p, o) in live {
        b.free(p, o, Kind::Other);
    }
    for p in held {
        b.free(p, 0, Kind::User);
    }
    b.drain_all();
    let i = b.zone_info(0);
    assert_eq!((i.pageblocks[HA as usize], i.free_highatomic), (4, 4 * PAGEBLOCK_FRAMES));
    assert_eq!(b.stats().free_bytes, END - BASE, "every frame is free again");
    b.check().unwrap();
}

fn chain(seed: u64) -> [u64; DEPTH] {
    core::array::from_fn(|i| if i < 6 { 0xffff_ffff_8000_0000 + seed * 0x1000 + i as u64 * 0x40 } else { 0 })
}

#[test]
fn a_caller_table_keeps_each_chain_once_and_counts_what_does_not_fit() {
    let t: CallerTable<8> = CallerTable::new();
    assert!(t.note_chain(&chain(1), 3, 8));
    assert!(!t.note_chain(&chain(1), 3, 8));
    assert!(t.note_chain(&chain(2), 0, 1));
    let mut seen = Vec::new();
    t.each(&mut |s| seen.push((s.chain[0], s.count, s.amount, s.info)));
    seen.sort();
    assert_eq!(seen, [(chain(1)[0], 2, 16, 3), (chain(2)[0], 1, 1, 0)]);
    for k in 3..=8 {
        assert!(t.note_chain(&chain(k), 0, 1));
    }
    assert_eq!(t.dropped(), 0);
    assert!(!t.note_chain(&chain(9), 0, 1), "the table is full of other chains");
    assert_eq!(t.dropped(), 1);
    assert!(!t.note_chain(&chain(5), 0, 1), "a chain it has is still counted");
    assert_eq!(t.dropped(), 1);
}

#[test]
fn a_caller_table_tells_of_a_chain_when_new_and_when_its_count_doubled() {
    let t: CallerTable<8> = CallerTable::new();
    let mut told = Vec::new();
    for _ in 0..40 {
        t.note_chain(&chain(1), 0, 1);
        t.each_news(&mut |s| told.push(s.count));
    }
    assert_eq!(told, [1, 2, 4, 8, 16, 32]);
}

/// Until 12.11.2026 the table of waiting threads had 64 slots: the 65th thread waiting for memory at the same time
/// counted as a caller that cannot wait and took the reserves at once.
#[test]
fn hundreds_of_threads_waiting_at_once_all_keep_the_reserve() {
    use std::sync::{Arc, Barrier};
    const THREADS: usize = 300;
    let (ready, done) = (Arc::new(Barrier::new(THREADS + 1)), Arc::new(Barrier::new(THREADS + 1)));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (ready, done) = (ready.clone(), done.clone());
            std::thread::spawn(move || {
                let keep = reclaim::keep_reserve();
                let waits = keep.as_ref().is_some_and(|k| k.held()) && reclaim::keeps_reserve() && reclaim::heap_waits();
                ready.wait();
                done.wait();
                drop(keep);
                waits && !reclaim::heap_waits()
            })
        })
        .collect();
    ready.wait();
    done.wait();
    let waiting = handles.into_iter().map(|h| h.join().unwrap()).filter(|&w| w).count();
    assert!(waiting >= THREADS - 2, "{waiting} of {THREADS} threads waiting at once kept the reserve");
}
