//! The guards added after the MM-1 review: one-shot boot build and heap seed, quarantine of an inconsistent
//! per-CPU list entry, kind mismatches counted apart from refused frees.

use super::{free, staged, MIB};
use crate::{boot_buddy, BuddyFrames, FrameAlloc, KernelHeap, Kind, Zone, PAGE};
use core::sync::atomic::Ordering::Relaxed;
use alloc::vec;

#[test]
fn boot_build_is_one_shot() {
    let first = BuddyFrames::new(vec![(MIB, 64 * MIB)]);
    let regions = first.region_count();
    let meta = first.meta_range();
    // A second boot build (e.g. a forged range) must not plan or carve anything: it gets the first one back.
    let second = BuddyFrames::new(vec![(64 * MIB, 512 * MIB)]);
    assert_eq!(second.region_count(), regions);
    assert_eq!(second.meta_range(), meta);
    assert!(core::ptr::eq(&*second as *const _, boot_buddy().unwrap()));
    assert!(core::ptr::eq(&*first as *const _, boot_buddy().unwrap()));
}

#[test]
fn inconsistent_cached_frame_is_quarantined() {
    let b = staged(&[free(MIB, 64 * MIB)]);
    let total = b.stats().total_bytes;
    let p = b.alloc(0, Zone::Dma32, Kind::User).unwrap();
    let bad = b.corrupt_cached_frame(0).unwrap();
    let q = b.alloc(0, Zone::Dma32, Kind::User).unwrap();
    assert_ne!(q, bad, "the corrupted frame is never handed out");
    assert_eq!(b.reports().corrupt.load(Relaxed), 1);
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0);
    let s = b.stats();
    assert_eq!(s.leaked_bytes, PAGE);
    assert_eq!(s.free_bytes + s.used_by_kind.iter().sum::<u64>() + s.leaked_bytes, total);
    b.free(p, 0, Kind::User);
    b.free(q, 0, Kind::User);
    b.check().unwrap();
}

#[test]
fn kind_mismatch_is_not_a_refused_free() {
    let b = staged(&[free(MIB, 16 * MIB)]);
    let p = b.alloc(1, Zone::Dma32, Kind::Dma).unwrap();
    b.free(p, 1, Kind::Heap);
    assert_eq!(b.reports().kind_mismatches.load(Relaxed), 1);
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0);
    assert_eq!(b.reports().last_bad_free(), None);
    b.check().unwrap();
}

#[test]
fn heap_seed_is_one_shot_and_direct_mapped() {
    let base = bootinfo::layout::DIRECT_MAP_BASE;
    let h = KernelHeap::new();
    h.seed(0x40_0000, 8 * MIB); // not a direct-map address
    assert_eq!(h.seed_range(), (0, 0));
    h.seed(base + 16 * MIB, 8 * MIB);
    assert_eq!(h.seed_range(), (16 * MIB, 24 * MIB));
    h.seed(base + 128 * MIB, 8 * MIB); // a second seed
    assert_eq!(h.seed_range(), (16 * MIB, 24 * MIB), "the first seed stays");
}
