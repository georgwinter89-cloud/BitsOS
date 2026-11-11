//! The census of aligned blocks: free, free after a per-CPU drain, cache-only (clearable) and pinned, with what pins.
use super::*;
use crate::buddy::HOLDERS;
use crate::BlockHolder;
use std::format;

/// 8 MiB from 1 MiB: three whole 2 MiB blocks at 2, 4 and 6 MiB, every frame a clean cache page with an owner.
fn all_cache() -> BuddyFrames {
    let b = staged(&[free(MIB, 9 * MIB)]);
    while let Some(p) = b.alloc(0, Zone::Normal, Kind::PageCache) {
        b.set_owner(p, p / PAGE + 1);
    }
    b
}

/// The frames of the 2 MiB block at `at_mib`.
fn block(at_mib: u64) -> impl Iterator<Item = u64> {
    (0..512).map(move |i| at_mib * MIB + i * PAGE)
}

#[test]
fn a_fresh_allocator_is_all_free_blocks() {
    let b = staged(&[free(MIB, 9 * MIB)]);
    let c = b.census(9);
    assert_eq!((c.blocks, c.free, c.free_pcp, c.cache_only, c.pinned), (3, 3, 0, 0, 0), "{c}");
    assert_eq!(c.free_frames, 2048);
    assert_eq!(c.largest_free, Some(10), "4..8 MiB is one free 4 MiB block");
    assert_eq!(c.holders, [0; HOLDERS]);
}

#[test]
fn every_block_is_sorted_by_what_holds_it() {
    let b = all_cache();
    let c = b.census(9);
    assert_eq!((c.blocks, c.cache_only, c.pinned, c.free_frames, c.largest_free), (3, 3, 0, 0, None), "{c}");
    // 4 MiB: one frame becomes a heap frame (W35 MM-1: the only free frame, so the heap allocation steals it out
    // of its Movable pageblock after the per-CPU drain).
    let f = 4 * MIB + 7 * PAGE;
    b.free(f, 0, Kind::PageCache);
    assert_eq!(b.alloc(0, Zone::Normal, Kind::Heap), Some(f));
    let c = b.census(9);
    assert_eq!((c.free + c.free_pcp, c.cache_only, c.pinned, c.pinned_few), (0, 2, 1, 1), "{c}");
    assert_eq!(c.holders[BlockHolder::Heap as usize], 1);
    assert_eq!(c.sole[BlockHolder::Heap as usize], 1, "only the heap frame pins it");
    // 6 MiB: a cache page without an owner (tmpfs) and a page table: pinned by two holders, neither alone.
    b.set_owner(6 * MIB, 0);
    let t = 6 * MIB + 9 * PAGE;
    b.free(t, 0, Kind::PageCache);
    assert_eq!(b.alloc(0, Zone::Normal, Kind::PageTable), Some(t));
    // 2 MiB: all freed (first onto this CPU's list, the rest into the buddy lists).
    for p in block(2) {
        b.free(p, 0, Kind::PageCache);
    }
    let c = b.census(9);
    assert_eq!((c.free + c.free_pcp, c.cache_only, c.pinned, c.pinned_few), (1, 0, 2, 2), "{c}");
    b.drain_all();
    let c = b.census(9);
    assert_eq!((c.free, c.free_pcp, c.largest_free), (1, 0, Some(9)), "after the drain it is one free block: {c}");
    assert_eq!((c.cache_only, c.pinned, c.pinned_few), (0, 2, 2), "{c}");
    assert_eq!((c.holders[BlockHolder::CacheKept as usize], c.holders[BlockHolder::PageTable as usize]), (1, 1));
    assert_eq!((c.sole[BlockHolder::CacheKept as usize], c.sole[BlockHolder::PageTable as usize]), (0, 0));
    let s = format!("{c}");
    assert!(s.contains("2048 KiB blocks 3") && s.contains("heap=1(only 1)") && s.ends_with("cache-kept=1") && s.contains("table=1"), "{s}");
}

#[test]
fn a_block_of_many_pinning_frames_is_not_few() {
    let b = all_cache();
    let frames: Vec<u64> = block(2).take(crate::buddy::FEW as usize + 1).collect();
    for &p in &frames {
        b.free(p, 0, Kind::PageCache);
    }
    for _ in &frames {
        assert!(b.alloc(0, Zone::Normal, Kind::User).is_some_and(|p| frames.contains(&p)));
    }
    let c = b.census(9);
    assert_eq!((c.pinned, c.pinned_few, c.sole[BlockHolder::User as usize]), (1, 0, 1), "{c}");
}

#[test]
fn frames_never_handed_out_and_partial_blocks() {
    // A module inside the region: never handed out, it pins its block; the partial 1..2 MiB and 9..9.5 MiB are not
    // blocks of order 9.
    let b = BuddyFrames::staged("test", &[free(MIB, 3 * MIB), module(3 * MIB, 3 * MIB + 4 * PAGE), free(3 * MIB + 4 * PAGE, 9 * MIB + MIB / 2)], Storage::Heap).unwrap();
    let c = b.census(9);
    assert_eq!(c.blocks, 3, "{c}");
    assert_eq!((c.free, c.pinned, c.pinned_few, c.sole[BlockHolder::Reserved as usize]), (2, 1, 1, 1), "{c}");
    // Orders above the largest are clamped, order 0 counts frames.
    assert_eq!(b.census(99).order, crate::MAX_ORDER);
    let z = b.census(0);
    assert_eq!(z.blocks, z.free + z.pinned, "{z}");
}
