//! Fence timelines (also across the 64-bit wrap), split 64-bit reads, writeback slots.

use std::cell::Cell;

use crate::fence::{FenceError, Half, Observed, Timeline, read_split};
use crate::wb::{Slot, WbError, WbPool};

#[test]
fn timeline_basics() {
    let mut t = Timeline::new(10, 3);
    assert_eq!(t.emit(), Ok(11));
    assert_eq!(t.emit(), Ok(12));
    assert_eq!(t.emit(), Ok(13));
    assert_eq!(t.emit(), Err(FenceError::Full));
    assert!(!t.is_signalled(11));
    assert!(t.is_signalled(10));
    assert_eq!(t.observe(12), Observed::Advanced { from: 10, to: 12 });
    assert!(t.is_signalled(11) && t.is_signalled(12) && !t.is_signalled(13));
    assert_eq!(t.observe(12), Observed::Unchanged);
    assert_eq!(t.observe(11), Observed::Stale(11));
    assert_eq!(t.observe(14), Observed::Ahead(14));
    assert!(!t.is_signalled(14), "never emitted");
    assert_eq!(t.in_flight(), 1);
    assert!(t.unemit());
    assert_eq!(t.force_complete(), Observed::Unchanged);
    t.emit().unwrap();
    assert_eq!(t.force_complete(), Observed::Advanced { from: 12, to: 13 });
}

#[test]
fn timeline_wraps_past_2_64() {
    let mut t = Timeline::new(u64::MAX - 2, 8);
    let seqs: std::vec::Vec<u64> = (0..6).map(|_| t.emit().unwrap()).collect();
    assert_eq!(seqs, [u64::MAX - 1, u64::MAX, 0, 1, 2, 3]);
    assert_eq!(t.observe(0), Observed::Advanced { from: u64::MAX - 2, to: 0 });
    assert!(t.is_signalled(u64::MAX) && t.is_signalled(0) && !t.is_signalled(1));
    assert_eq!(t.observe(u64::MAX), Observed::Stale(u64::MAX));
    assert_eq!(t.observe(3), Observed::Advanced { from: 0, to: 3 });
    assert_eq!(t.observe(4), Observed::Ahead(4));
}

/// The engine writes low then high. Every interleaving of the reader's three or four reads with the two
/// writes gives either the old or the new value, or an older one the timeline ignores.
#[test]
fn split_read_never_goes_backwards() {
    for (old, new) in [(0x0000_0000_FFFF_FFFFu64, 0x0000_0001_0000_0000u64), (5, 6), (0x1_FFFF_FFFF, 0x2_0000_0003)] {
        // Writes happen after `k` reads (k = 0..=4) for the low half and after `m >= k` reads for the high half.
        for k in 0..=4 {
            for m in k..=4 {
                let reads = Cell::new(0);
                let v = read_split(|h| {
                    let n = reads.get();
                    reads.set(n + 1);
                    let lo = if n >= k { new as u32 } else { old as u32 };
                    let hi = if n >= m { (new >> 32) as u32 } else { (old >> 32) as u32 };
                    match h {
                        Half::Low => lo,
                        Half::High => hi,
                    }
                });
                let mut t = Timeline::new(old, 8);
                while t.emitted() != new {
                    t.emit().unwrap();
                }
                let obs = t.observe(v);
                assert!(
                    v == old || v == new || matches!(obs, Observed::Stale(_)),
                    "old {old:#x} new {new:#x} k {k} m {m} read {v:#x} {obs:?}"
                );
                assert!(t.signalled() == old || t.signalled() == new);
            }
        }
    }
}

#[test]
fn wb_pool() {
    let mut p = WbPool::new(70);
    let slots: std::vec::Vec<Slot> = (0..70).map(|_| p.get().unwrap()).collect();
    assert_eq!(p.get(), Err(WbError::Full));
    assert_eq!(WbError::Full.errno(), crate::errno::ENOSPC);
    assert_eq!(slots[65].index(), 65);
    assert_eq!(slots[3].dw_offset(), 24);
    assert_eq!(slots[3].byte_offset(), 96);
    assert_eq!(slots[3].gpu_addr(0x1000), 0x1060);
    p.free(slots[40]).unwrap();
    assert_eq!(p.free(slots[40]), Err(WbError::NotAllocated(40)));
    assert_eq!(p.get(), Ok(slots[40]));
    assert_eq!(p.used(), 70);
    assert_eq!(p.slot_at(24), Some(slots[3]));
    assert_eq!(p.slot_at(25), None);
    assert_eq!(p.slot_at(70 * 8), None);
    assert_eq!(p.buffer_bytes(), 70 * 32);
    assert!(WbPool::new(0).get().is_err());
}
