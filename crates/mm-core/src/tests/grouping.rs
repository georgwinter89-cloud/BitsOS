//! W35 MM-1: grouping by mobility: kinds land in pageblocks of their type, the fallback rules of Linux 7.x
//! (`try_to_claim_block`'s half-block claim, `__rmqueue_steal`), the MAX_ORDER 18 split, per-type per-CPU lists and
//! the watermark check with an order.
use super::*;
use crate::reclaim::watermarks;
use crate::{Buddy, MigrateType, PAGEBLOCK_FRAMES};
use core::sync::atomic::Ordering::Relaxed;

const MOV: MigrateType = MigrateType::Movable;
const UNMOV: MigrateType = MigrateType::Unmovable;

fn steals(b: &Buddy) -> u64 {
    b.reports().steals.load(Relaxed)
}
fn claims(b: &Buddy) -> u64 {
    b.reports().whole_blocks.load(Relaxed)
}
fn blocks_of(b: &Buddy, mt: MigrateType) -> u64 {
    b.zone_info(0).pageblocks[mt as usize] + b.zone_info(1).pageblocks[mt as usize]
}

/// Every held frame lies in a pageblock of its kind's type.
fn all_in_their_type(b: &Buddy, held: &[(u64, Kind)]) {
    for &(p, k) in held {
        assert_eq!(b.pageblock_type(p), Some(MigrateType::of(k)), "frame {p:#x} of {k:?}");
    }
}

#[test]
fn mixed_kinds_stay_in_pageblocks_of_their_type() {
    // 64 MiB: 32 pageblocks; the mix below never needs more than 28 of them.
    let b = staged(&[free(16 * MIB, 80 * MIB)]);
    let mut rng = Rng(7);
    let mut held = Vec::new();
    for _ in 0..12_000 {
        let k = if rng.below(2) == 0 { Kind::User } else { Kind::Other };
        held.push((b.alloc(0, Zone::Normal, k).unwrap(), k));
    }
    all_in_their_type(&b, &held);
    // Half of them back at random, then a new mix: the partly used pageblocks of each type serve it first.
    let mut kept = Vec::new();
    for (p, k) in held {
        if rng.below(2) == 0 {
            b.free(p, 0, k);
        } else {
            kept.push((p, k));
        }
    }
    for _ in 0..6000 {
        let k = if rng.below(3) == 0 { Kind::Heap } else { Kind::PageCache };
        kept.push((b.alloc(0, Zone::Normal, k).unwrap(), k));
    }
    all_in_their_type(&b, &kept);
    assert_eq!((steals(&b), claims(&b)), (0, 0), "free pageblocks were left: nothing was mixed");
    assert_eq!(blocks_of(&b, MOV) + blocks_of(&b, UNMOV), 32);
    b.check().unwrap();
}

/// 4 MiB at 2 MiB: pageblock A = 2..4 MiB, B = 4..6 MiB, every frame of both taken by `kind`.
fn two_full_blocks(kind: Kind) -> (BuddyFrames, Vec<u64>) {
    let b = staged(&[free(2 * MIB, 6 * MIB)]);
    let mut all = Vec::new();
    while let Some(p) = b.alloc(0, Zone::Normal, kind) {
        all.push(p);
    }
    assert_eq!(all.len(), 1024);
    (b, all)
}

const A: u64 = 2 * MIB;
const B: u64 = 4 * MIB;

#[test]
fn an_unmovable_fallback_claims_a_pageblock_at_least_half_free() {
    let (b, _) = two_full_blocks(Kind::User);
    assert_eq!(blocks_of(&b, MOV), 2);
    for i in 0..300 {
        b.free(A + i * PAGE, 0, Kind::User);
    }
    b.drain_all();
    let p = b.alloc(0, Zone::Normal, Kind::Other).unwrap();
    assert!((A..B).contains(&p), "{p:#x}");
    assert_eq!(b.pageblock_type(A), Some(UNMOV), "300 of 512 frames free: the whole block is claimed");
    assert_eq!((claims(&b), steals(&b), blocks_of(&b, UNMOV)), (1, 0, 1));
    // The claimed block's 299 other free frames serve the next ones; B (100 free, below half) is stolen from.
    for i in 0..100 {
        b.free(B + i * 4 * PAGE, 0, Kind::User);
    }
    b.drain_all();
    for _ in 0..299 {
        let q = b.alloc(0, Zone::Normal, Kind::Other).unwrap();
        assert!((A..B).contains(&q), "{q:#x} left the claimed block");
    }
    assert_eq!(steals(&b), 0);
    let s = b.alloc(0, Zone::Normal, Kind::Other).unwrap();
    assert!((B..B + 2 * MIB).contains(&s));
    assert_eq!((b.pageblock_type(B), steals(&b), claims(&b)), (Some(MOV), 1, 1), "100 of 512 free: stolen, B stays Movable");
    // A frame goes back to the list of its pageblock's type, not of the kind it was allocated for.
    b.free(s, 0, Kind::Other);
    assert_eq!(b.alloc(0, Zone::Normal, Kind::User), Some(s));
    b.check().unwrap();
}

#[test]
fn small_movable_fallbacks_steal_and_large_ones_claim() {
    let (b, _) = two_full_blocks(Kind::Other);
    assert_eq!(blocks_of(&b, UNMOV), 2);
    // A: every other frame free, so no free block of order 4 or more anywhere.
    for i in (0..512).step_by(2) {
        b.free(A + i * PAGE, 0, Kind::Other);
    }
    b.drain_all();
    // Order 0 Movable: no claim below order 4 (Linux `should_try_claim_block`), a steal; A stays Unmovable.
    let p = b.alloc(0, Zone::Normal, Kind::User).unwrap();
    assert!((A..B).contains(&p));
    assert_eq!((b.pageblock_type(A), steals(&b), claims(&b)), (Some(UNMOV), 1, 0));
    // B: 300 contiguous frames free. Order 4 Movable: the largest free block first, and 300 of 512 free claim B.
    for i in 0..300 {
        b.free(B + i * PAGE, 0, Kind::Other);
    }
    b.drain_all();
    let q = b.alloc(4, Zone::Normal, Kind::User).unwrap();
    assert!((B..B + 2 * MIB).contains(&q));
    assert_eq!((b.pageblock_type(B), b.pageblock_type(A), claims(&b)), (Some(MOV), Some(UNMOV), 1));
    b.check().unwrap();
}

#[test]
fn a_one_gib_block_gives_only_the_pageblocks_it_must() {
    let b = staged(&[free(GIB, 2 * GIB)]);
    assert_eq!(b.zone_info(0).free_blocks[crate::MAX_ORDER as usize], 1);
    let p = b.alloc(0, Zone::Normal, Kind::Other).unwrap();
    assert_eq!((blocks_of(&b, UNMOV), blocks_of(&b, MOV)), (1, 511), "one frame changes one pageblock");
    let z = b.zone_info(0);
    assert!((9..crate::MAX_ORDER as usize).all(|o| z.free_blocks[o] == 1), "the halves stay whole: {:?}", z.free_blocks);
    let u = b.alloc(0, Zone::Normal, Kind::User).unwrap();
    assert_ne!(u / (PAGEBLOCK_FRAMES * PAGE), p / (PAGEBLOCK_FRAMES * PAGE), "user memory in another pageblock");
    // An order-10 heap run claims exactly its two pageblocks.
    let r = b.alloc(10, Zone::Normal, Kind::Heap).unwrap();
    assert_eq!(blocks_of(&b, UNMOV), 3);
    assert_eq!((b.pageblock_type(r), b.pageblock_type(r + 2 * MIB)), (Some(UNMOV), Some(UNMOV)));
    b.free(p, 0, Kind::Other);
    b.free(u, 0, Kind::User);
    b.free(r, 10, Kind::Heap);
    b.drain_all();
    assert_eq!(b.zone_info(0).free_blocks[crate::MAX_ORDER as usize], 1, "everything merges back to 1 GiB");
    b.check().unwrap();
}

#[test]
fn per_cpu_lists_are_per_type() {
    let b = staged(&[free(16 * MIB, 80 * MIB)]);
    let u = b.alloc(0, Zone::Normal, Kind::User).unwrap();
    b.free(u, 0, Kind::User);
    let h = b.alloc(0, Zone::Normal, Kind::Heap).unwrap();
    assert_ne!(h, u, "a heap frame never comes from the Movable list");
    assert_eq!(b.pageblock_type(h), Some(UNMOV));
    b.free(h, 0, Kind::Heap);
    assert_eq!(b.alloc(0, Zone::Normal, Kind::PageCache), Some(u), "the Movable list hands back its frame");
    assert_eq!(b.alloc(0, Zone::Normal, Kind::PageTable), Some(h), "and the Unmovable list its own");
    // A drain empties every list of every type.
    b.free(u, 0, Kind::PageCache);
    b.free(h, 0, Kind::PageTable);
    assert!(b.drain_all() >= 2);
    assert_eq!(b.zone_info(0).cached_frames, 0);
    b.check().unwrap();
}

#[test]
fn the_watermark_check_counts_blocks_of_the_order() {
    let b = staged(&[free(16 * MIB, 24 * MIB)]);
    let total = 2048;
    assert_eq!(b.zone_watermarks(0), watermarks(total), "all memory is in DMA32");
    assert_eq!(b.zone_watermarks(1), Default::default());
    assert!(b.zone_watermark_ok(0, 9, 100) && b.order_balanced(9));
    // Every frame taken, every other one back: 1024 free frames and no two of them buddies.
    let all: Vec<u64> = std::iter::from_fn(|| b.alloc(0, Zone::Normal, Kind::Other)).collect();
    for &p in all.iter().filter(|&&p| (p / PAGE) % 2 == 1) {
        b.free(p, 0, Kind::Other);
    }
    b.drain_all();
    assert!(b.zone_watermark_ok(0, 0, 1023) && !b.zone_watermark_ok(0, 0, 1024), "order 0: the count alone");
    assert!(!b.zone_watermark_ok(0, 1, 100), "no free block of order 1");
    assert!(!b.order_balanced(1) && b.order_balanced(0));
    // One frame more makes an order-1 block; the mark still counts the frames the block takes (Linux: 2^order - 1).
    b.free(16 * MIB, 0, Kind::Other);
    b.drain_all();
    assert!(b.zone_watermark_ok(0, 1, 1023) && !b.zone_watermark_ok(0, 1, 1024));
    assert!(!b.zone_watermark_ok(0, 2, 0));
    assert!(!b.zone_watermark_ok(1, 0, 0), "an empty zone is never ok");
}

/// Whole free pageblocks (free blocks of the pageblock order and up, in pageblocks), both zones.
fn free_pageblocks(b: &Buddy) -> u64 {
    let pb = crate::PAGEBLOCK_ORDER as usize;
    (0..2).map(|z| b.zone_info(z).free_blocks.iter().enumerate().skip(pb).map(|(o, &n)| n << (o - pb)).sum::<u64>()).sum()
}

#[test]
fn freed_movable_memory_gives_whole_pageblocks_back() {
    // The Mini-PC's failure ("no free 2 MiB block" with GBs free): user and kernel frames interleaved in time.
    // Grouped, freeing the user frames gives back every pageblock they used; only the kernel's stay broken.
    let b = staged(&[free(16 * MIB, 80 * MIB)]);
    let mut users = Vec::new();
    let mut heap = Vec::new();
    for i in 0..12_000 {
        if i % 2 == 0 {
            users.push(b.alloc(0, Zone::Normal, Kind::User).unwrap());
        } else {
            heap.push(b.alloc(0, Zone::Normal, Kind::Heap).unwrap());
        }
    }
    let unmovable = blocks_of(&b, UNMOV);
    assert!(unmovable <= 13, "6000 heap frames fill 12 pageblocks: {unmovable}");
    for p in users {
        b.free(p, 0, Kind::User);
    }
    b.drain_all();
    assert_eq!(free_pageblocks(&b), 32 - unmovable, "every pageblock without a heap frame is whole again");
    for p in heap {
        b.free(p, 0, Kind::Heap);
    }
    b.drain_all();
    assert_eq!(free_pageblocks(&b), 32);
    b.check().unwrap();
}

#[test]
fn threads_mixing_kinds_with_a_drainer_keep_the_lists_sound() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    // 32 MiB: small enough that four threads filling it make claims and steals happen.
    let (s, e) = (16 * MIB, 48 * MIB);
    let b = Arc::new(staged(&[free(s, e)]));
    let free_before = b.stats().free_bytes;
    let stop = Arc::new(AtomicBool::new(false));
    let drainer = {
        let (b, stop) = (b.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Relaxed) {
                b.drain_all();
                std::thread::yield_now();
            }
        })
    };
    const KINDS: [Kind; 4] = [Kind::User, Kind::Heap, Kind::PageTable, Kind::PageCache];
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let b = b.clone();
            std::thread::spawn(move || {
                let mut rng = Rng(0x5eed_0000 + t);
                let mut mine: Vec<(u64, u32, Kind)> = Vec::new();
                for _ in 0..15_000 {
                    if mine.is_empty() || rng.below(3) != 0 {
                        let (o, k) = (rng.below(4) as u32, KINDS[rng.below(4) as usize]);
                        match b.alloc(o, Zone::Normal, k) {
                            Some(p) => mine.push((p, o, k)),
                            None if !mine.is_empty() => {
                                let (p, o, k) = mine.swap_remove(rng.below(mine.len() as u64) as usize);
                                b.free(p, o, k);
                            }
                            None => {}
                        }
                    } else {
                        let (p, o, k) = mine.swap_remove(rng.below(mine.len() as u64) as usize);
                        b.free(p, o, k);
                    }
                }
                mine
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in workers {
        all.extend(h.join().unwrap());
    }
    stop.store(true, Relaxed);
    drainer.join().unwrap();
    let blocks: Vec<(u64, u32)> = all.iter().map(|&(p, o, _)| (p, o)).collect();
    assert_disjoint_inside(&blocks, &[(s, e)]);
    assert!(steals(&b) + claims(&b) > 0, "the pool was too big: no fallback ran");
    b.check().unwrap();
    for (p, o, k) in all {
        b.free(p, o, k);
    }
    b.drain_all();
    b.check().unwrap();
    assert_eq!(b.stats().free_bytes, free_before);
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0);
    assert_eq!(free_pageblocks(&b), 16, "everything merged back");
}

#[test]
fn exact_runs_and_address_limits_follow_the_types() {
    let b = staged(&[free(16 * MIB, 48 * MIB)]);
    let mut users = Vec::new();
    for _ in 0..100 {
        users.push(b.alloc(0, Zone::Normal, Kind::User).unwrap());
    }
    let run = b.alloc_exact(5, 1, Zone::Normal, Kind::Heap).unwrap();
    let low = b.alloc_below(0, 24 * MIB, Kind::Dma).unwrap();
    let low_run = b.alloc_exact_below(3, 1, 20 * MIB, Kind::Driver).unwrap();
    let low_user = b.alloc_below(2, 20 * MIB, Kind::User).unwrap();
    let light = b.alloc_light(0, Zone::Normal, Kind::PageTable).unwrap();
    assert!(low + PAGE <= 24 * MIB && low_run + 3 * PAGE <= 20 * MIB && low_user + 4 * PAGE <= 20 * MIB);
    for p in (0..5).map(|i| run + i * PAGE).chain((0..3).map(|i| low_run + i * PAGE)).chain([low, light]) {
        assert_eq!(b.pageblock_type(p), Some(UNMOV), "kernel frame {p:#x}");
    }
    for &p in users.iter().chain([&low_user]) {
        assert_eq!(b.pageblock_type(p), Some(MOV), "user frame {p:#x}");
    }
    assert_eq!((steals(&b), claims(&b)), (0, 0));
    b.check().unwrap();
    assert!(b.free_exact(run, 5, Kind::Heap) && b.free_exact(low_run, 3, Kind::Driver));
    b.free(low, 0, Kind::Dma);
    b.free(low_user, 2, Kind::User);
    b.free(light, 0, Kind::PageTable);
    for p in users {
        b.free(p, 0, Kind::User);
    }
    b.drain_all();
    assert_eq!(free_pageblocks(&b), 16);
    b.check().unwrap();
}

#[test]
fn a_rebuilt_allocator_starts_movable_and_regroups() {
    // Reload: the new kernel builds the buddy from the map; what the old one keeps (a module here) sits in a
    // pageblock that reads Movable, as every pageblock does after the build.
    let b = staged(&[free(16 * MIB, 23 * MIB), module(23 * MIB, 23 * MIB + 64 * 1024), free(23 * MIB + 64 * 1024, 32 * MIB)]);
    assert_eq!((blocks_of(&b, MOV), blocks_of(&b, UNMOV)), (8, 0), "a fresh build is all Movable");
    assert_eq!(b.pageblock_type(23 * MIB), Some(MOV));
    let heap: Vec<u64> = (0..600).map(|_| b.alloc(0, Zone::Normal, Kind::Heap).unwrap()).collect();
    let user: Vec<u64> = (0..600).map(|_| b.alloc(0, Zone::Normal, Kind::User).unwrap()).collect();
    for &p in &heap {
        assert_eq!(b.pageblock_type(p), Some(UNMOV));
    }
    for &p in &user {
        assert_eq!(b.pageblock_type(p), Some(MOV));
    }
    b.check().unwrap();
    assert!(b.release_module(23 * MIB, 23 * MIB + 64 * 1024).is_ok());
    for p in heap {
        b.free(p, 0, Kind::Heap);
    }
    for p in user {
        b.free(p, 0, Kind::User);
    }
    b.drain_all();
    assert_eq!(free_pageblocks(&b), 8);
    b.check().unwrap();
}
