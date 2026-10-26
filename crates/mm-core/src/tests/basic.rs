//! Split, merge, zones, alignment, bad frees, per-CPU lists, modules.
use super::*;
use crate::{BadFree, MAX_ORDER};

#[test]
fn every_order_aligned_and_merges_back() {
    let b = staged(&[free(0x10_0000, 0x10_0000 + 2 * GIB)]);
    let before = b.zone_info(0);
    b.check().unwrap();
    for order in 0..=MAX_ORDER {
        let p = b.alloc(order, Zone::Dma32, Kind::Driver).unwrap_or_else(|| panic!("order {order}"));
        assert_eq!(p % (PAGE << order), 0, "order {order} aligned to its size");
        assert_eq!(b.stats().used_by_kind[Kind::Driver as usize], PAGE << order);
        b.free(p, order, Kind::Driver);
        assert_eq!(b.stats().used_by_kind[Kind::Driver as usize], 0);
    }
    b.check().unwrap();
    b.drain_all();
    let after = b.zone_info(0);
    assert_eq!(before.free_blocks, after.free_blocks, "every split merged back");
    assert_eq!(b.stats().leaked_bytes, 0);
}

#[test]
fn split_gives_lowest_block_and_buddies_merge() {
    // Exactly one 2 MiB block (order 9) at 2 MiB.
    let b = staged(&[free(2 * MIB, 4 * MIB)]);
    assert_eq!(b.zone_info(0).free_blocks[9], 1);
    let a = b.alloc(3, Zone::Dma32, Kind::Other).unwrap();
    assert_eq!(a, 2 * MIB);
    let z = b.zone_info(0);
    assert_eq!(&z.free_blocks[3..9], &[1, 1, 1, 1, 1, 1], "one buddy left at each order below 9");
    let c = b.alloc(3, Zone::Dma32, Kind::Other).unwrap();
    assert_eq!(c, 2 * MIB + 8 * PAGE, "the buddy of the first block");
    b.free(a, 3, Kind::Other);
    assert_eq!(b.zone_info(0).free_blocks[3], 1, "a's buddy is taken: no merge yet");
    b.free(c, 3, Kind::Other);
    assert_eq!(b.zone_info(0).free_blocks[9], 1, "merged all the way up");
    b.check().unwrap();
}

#[test]
fn zones_prefer_and_fall_back() {
    let b = staged(&[free(0x10_0000, 64 * MIB), free(4 * GIB, 4 * GIB + 64 * MIB)]);
    let n = b.alloc(0, Zone::Normal, Kind::User).unwrap();
    assert!(n >= 4 * GIB, "NORMAL prefers memory above 4 GiB");
    let d = b.alloc(4, Zone::Dma32, Kind::Dma).unwrap();
    assert!(d + 16 * PAGE <= 4 * GIB);
    // Empty NORMAL: then NORMAL requests come from DMA32 (counted), DMA32 requests never from NORMAL.
    let big: Vec<(u64, u32)> = std::iter::from_fn(|| b.alloc(10, Zone::Normal, Kind::User).map(|p| (p, 10))).take_while(|p| p.0 >= 4 * GIB).collect();
    assert_eq!(big.len(), 15, "64 MiB above 4 GiB minus the partly used first 4 MiB block");
    assert_eq!(b.reports().fallbacks.load(core::sync::atomic::Ordering::Relaxed), 1, "the 16th came from DMA32");
    let rest = take_all(&b, Zone::Dma32);
    assert!(rest.iter().all(|&(p, o)| p + (PAGE << o) <= 4 * GIB));
    b.check().unwrap();
}

#[test]
fn dma32_never_above_4g() {
    let b = staged(&[free(4 * GIB, 8 * GIB)]);
    assert_eq!(b.alloc(0, Zone::Dma32, Kind::Dma), None);
    assert!(b.alloc(0, Zone::Normal, Kind::Dma).is_some());
    assert_eq!(b.reports().alloc_fails.load(core::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn region_straddling_4g_is_split() {
    let b = staged(&[free(4 * GIB - 8 * MIB, 4 * GIB + 8 * MIB)]);
    let d = take_all(&b, Zone::Dma32);
    assert_eq!(d.iter().map(|&(_, o)| PAGE << o).sum::<u64>(), 8 * MIB);
    assert!(d.iter().all(|&(p, o)| p + (PAGE << o) <= 4 * GIB), "no block crosses 4 GiB");
    let n = take_all(&b, Zone::Normal);
    assert_eq!(n.iter().map(|&(_, o)| PAGE << o).sum::<u64>(), 8 * MIB);
    assert!(n.iter().all(|&(p, _)| p >= 4 * GIB));
}

#[test]
fn bad_frees_are_refused_and_change_nothing() {
    let b = staged(&[free(0x10_0000, 0x7F_F000), rsv(0x40_0000, 0x41_0000), free(0x100_0000, 0x200_0000)]);
    let p0 = b.alloc(0, Zone::Dma32, Kind::Heap).unwrap();
    let p4 = b.alloc(4, Zone::Dma32, Kind::Dma).unwrap();
    let before = b.stats();
    let cases: [(u64, u32, BadFree); 9] = [
        (0x40_0000, 0, BadFree::OutOfRange), // the reserved page (KEEP-like)
        (0x1000, 0, BadFree::OutOfRange),    // below 1 MiB
        (0x900_0000, 0, BadFree::OutOfRange),
        (0x7F_E000, 1, BadFree::OutOfRange), // block would run past the region
        (p4 + PAGE, 0, BadFree::NotHead),
        (p4 + PAGE, 4, BadFree::Misaligned),
        (p4, 3, BadFree::OrderMismatch),
        (p0, MAX_ORDER + 1, BadFree::BadOrder),
        (0x180_0000, 0, BadFree::NotHead), // free memory: inside a free block
    ];
    for (i, &(phys, order, why)) in cases.iter().enumerate() {
        b.free(phys, order, Kind::Heap);
        assert_eq!(b.reports().last_bad_free(), Some((phys, why)), "case {i}");
        assert_eq!(b.reports().bad_frees.load(core::sync::atomic::Ordering::Relaxed), i as u64 + 1);
    }
    let after = b.stats();
    assert_eq!((before.free_bytes, before.used_by_kind), (after.free_bytes, after.used_by_kind));
    b.check().unwrap();
    // Double frees: order 0 (parked in the per-CPU list) and order 4 (back in the buddy lists).
    b.free(p0, 0, Kind::Heap);
    b.free(p0, 0, Kind::Heap);
    assert_eq!(b.reports().last_bad_free(), Some((p0, BadFree::DoubleFree)));
    b.free(p4, 4, Kind::Dma);
    b.free(p4, 4, Kind::Dma);
    assert_eq!(b.reports().last_bad_free(), Some((p4, BadFree::DoubleFree)));
    b.check().unwrap();
    assert_eq!(b.stats().used_by_kind, [0; crate::KINDS]);
}

#[test]
fn wrong_kind_is_accounted_to_the_recorded_owner() {
    let b = staged(&[free(0x10_0000, 0x200_0000)]);
    let p = b.alloc(2, Zone::Dma32, Kind::Stack).unwrap();
    b.free(p, 2, Kind::User);
    assert_eq!(b.reports().last_kind_mismatch(), Some((p, Kind::User as usize, Kind::Stack as usize)));
    assert_eq!(b.reports().bad_frees.load(core::sync::atomic::Ordering::Relaxed), 0, "accepted, not a refused free");
    assert_eq!(b.stats().used_by_kind, [0; crate::KINDS], "no wrap-around in User, Stack back to 0");
    b.check().unwrap();
}

#[test]
fn per_cpu_list_batches_and_drains() {
    let b = staged(&[free(0x10_0000, 0x10_0000 + 64 * MIB)]);
    let total = b.stats().free_bytes;
    let p = b.alloc(0, Zone::Dma32, Kind::User).unwrap();
    let z = b.zone_info(0);
    assert_eq!(z.cached_frames, crate::pcp::BATCH as u64 - 1, "one batch moved to the list, one handed out");
    assert_eq!(b.stats().free_bytes, total - PAGE, "cached frames count as free");
    b.free(p, 0, Kind::User);
    // Free far more than HIGH single frames: the list gives batches back and never grows past its storage.
    let many: Vec<u64> = (0..1000).map(|_| b.alloc(0, Zone::Dma32, Kind::User).unwrap()).collect();
    for &f in &many {
        b.free(f, 0, Kind::User);
        assert!(b.zone_info(0).cached_frames <= (crate::pcp::HIGH + crate::pcp::BATCH) as u64);
    }
    assert!(b.drain_all() > 0);
    assert_eq!(b.zone_info(0).cached_frames, 0);
    assert_eq!(b.stats().free_bytes, total);
    b.check().unwrap();
}

#[test]
fn exhaustion_drains_the_lists_and_returns_none() {
    let b = staged(&[free(4 * MIB, 8 * MIB)]);
    let one = b.alloc(0, Zone::Dma32, Kind::User).unwrap();
    b.free(one, 0, Kind::User); // now BATCH frames sit in the list, the 4 MiB block is split
    let all = take_all(&b, Zone::Dma32);
    assert_eq!(all.iter().map(|&(_, o)| PAGE << o).sum::<u64>(), 4 * MIB, "everything, including the cached frames");
    assert_eq!(b.alloc(0, Zone::Dma32, Kind::User), None);
    assert_eq!(b.stats().free_bytes, 0);
    for &(p, o) in &all {
        b.free(p, o, Kind::Other);
    }
    b.drain_all();
    assert_eq!(b.zone_info(0).free_blocks[10], 1, "back to one 4 MiB block");
    b.check().unwrap();
}

#[test]
fn modules_stay_reserved_until_released() {
    let m = (0x100_0000, 0x110_0000);
    let b = staged(&[free(0x10_0000, 0x400_0000), module(m.0, m.1), rsv(0x200_0000, 0x200_1000)]);
    let all = take_all(&b, Zone::Dma32);
    assert!(all.iter().all(|&(p, o)| p + (PAGE << o) <= m.0 || p >= m.1), "module frames never handed out");
    let total = b.stats().total_bytes;
    b.free(m.0, 0, Kind::Other);
    assert_eq!(b.reports().last_bad_free(), Some((m.0, BadFree::OutOfRange)), "a module frame was never handed out");
    assert_eq!(b.release_module(0x200_0000, 0x200_1000), Err(crate::ReleaseError::NotAModule), "KEEP is not a module");
    assert_eq!(b.release_module(m.0, m.1 + PAGE), Err(crate::ReleaseError::NotAModule));
    assert_eq!(b.release_module(m.0, m.0 + 8 * PAGE), Ok(8 * PAGE), "part of a module");
    assert_eq!(b.release_module(m.0 + 8 * PAGE, m.1), Ok(m.1 - m.0 - 8 * PAGE), "the rest");
    assert_eq!(b.release_module(m.0, m.1), Err(crate::ReleaseError::NotAModule), "twice");
    assert_eq!(b.stats().total_bytes, total + (m.1 - m.0));
    let got = take_all(&b, Zone::Dma32);
    assert_eq!(got.iter().map(|&(_, o)| PAGE << o).sum::<u64>(), m.1 - m.0);
    assert_eq!(got, std::vec![(m.0, 8)], "released halves merged into one 1 MiB block");
    for &(p, o) in all.iter().chain(got.iter()) {
        b.free(p, o, Kind::Other);
    }
    b.check().unwrap();
}

#[test]
fn fragmentation_and_recovery() {
    let b = staged(&[free(8 * MIB, 16 * MIB)]);
    let frames: Vec<u64> = (0..2048).map(|_| b.alloc(0, Zone::Dma32, Kind::User).unwrap()).collect();
    assert_eq!(b.alloc(0, Zone::Dma32, Kind::User), None);
    for (i, &f) in frames.iter().enumerate() {
        if i % 2 == 0 {
            b.free(f, 0, Kind::User);
        }
    }
    assert_eq!(b.stats().free_bytes, 4 * MIB);
    assert_eq!(b.alloc(1, Zone::Dma32, Kind::User), None, "half free, but no two neighbours");
    for (i, &f) in frames.iter().enumerate() {
        if i % 2 == 1 {
            b.free(f, 0, Kind::User);
        }
    }
    let p = b.alloc(11, Zone::Dma32, Kind::User).expect("everything merged again: 8 MiB block");
    assert_eq!(p % (8 * MIB), 0);
    b.free(p, 11, Kind::User);
    b.check().unwrap();
}

#[test]
fn bad_order_alloc_is_refused() {
    let b = staged(&[free(0x10_0000, 0x200_0000)]);
    assert_eq!(b.alloc(MAX_ORDER + 1, Zone::Normal, Kind::Other), None);
    assert_eq!(b.alloc(63, Zone::Dma32, Kind::Other), None);
    b.check().unwrap();
}
