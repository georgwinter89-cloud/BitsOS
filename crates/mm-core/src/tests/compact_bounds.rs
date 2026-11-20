//! The scanners' bounds (`buddy::compact`): a run for an order takes the targets its chunk needs and stops at its
//! block, the next one goes on where both scanners stopped, and the free scanner has a budget as the migrate
//! scanner has. 12.11.2026, Mini-PC generation 137: a free scanner without a budget and without a cached position
//! walked down from the top of the zone in every run, 1.2 billion frames in 30 minutes.
use super::compact_hints::{holes, Mover};
use super::*;
use crate::buddy::FREE_SCAN;
use std::boxed::Box;

const BASE: u64 = 2 * MIB;

/// Two free frames at the end of each pageblock in [from, to) of `cached` memory (no buddies).
fn two_at_the_end(b: &crate::Buddy, from: u64, to: u64) {
    for pb in from..to {
        for i in [509, 511] {
            let p = BASE + (pb * 512 + i) * PAGE;
            b.set_owner(p, 0);
            b.free(p, 0, Kind::PageCache);
        }
    }
    b.drain_all();
}

#[test]
fn a_run_takes_the_targets_its_chunk_needs_and_stops_at_its_block() {
    let (b, _c) = super::compact::cached(16);
    assert!(b.set_migrator(Box::leak(Box::new(Mover(b)))));
    holes(b, 4, 8);
    let free = b.stats().free_bytes;
    assert!(b.compact_direct(3));
    let s = b.compact_stats();
    assert_eq!((s.moved, s.cleared), (8, 1), "one chunk of eight frames, though the run's budget allowed thousands");
    assert_eq!(s.isolated, 16, "eight targets taken for eight frames moved, none taken and given back");
    assert_eq!(b.stats().free_bytes, free);
    assert_eq!(b.alloc(3, Zone::Normal, Kind::Other), Some(BASE));
    b.check().unwrap();
}

#[test]
fn the_next_run_goes_on_where_the_free_scanner_stopped() {
    // The four pageblocks at the top hold two free frames each, at their ends: a run for eight frames reads all
    // four to find its eight targets.
    let (b, _c) = super::compact::cached(16);
    assert!(b.set_migrator(Box::leak(Box::new(Mover(b)))));
    two_at_the_end(b, 4, 8);
    holes(b, 2, 4);
    assert!(b.compact_direct(3));
    let first = b.compact_stats();
    assert_eq!((first.moved, first.free_scanned), (8, 4 * 512), "the first run read the four pageblocks at the top");
    assert_eq!(b.alloc(3, Zone::Normal, Kind::Other), Some(BASE));
    // The second run: its targets are in the pageblock below. From the top again it would read those four first.
    assert!(b.compact_direct(3));
    let second = b.compact_stats();
    assert_eq!(second.moved, 16);
    assert!(second.free_scanned - first.free_scanned <= 16, "read {} frames for eight targets in a pageblock with holes", second.free_scanned - first.free_scanned);
    assert_eq!(second.migrate_scanned - first.migrate_scanned, 8, "and the migrate scanner read the next chunk only");
    b.check().unwrap();
}

#[test]
fn the_free_scanner_stops_at_its_budget_and_a_later_run_goes_on() {
    // 384 MiB of cache, free frames only in pageblocks 1..9: the free scanner has 183 pageblocks without a free
    // frame above them, more than a direct run may read.
    let (b, _c) = super::compact::cached(384);
    assert!(b.set_migrator(Box::leak(Box::new(Mover(b)))));
    holes(b, 1, 9);
    assert!(b.compaction_suitable(0, 3));
    assert!(!b.compact_direct(3), "the run's budget ends before the free scanner reaches a free frame");
    let s = b.compact_stats();
    assert_eq!((s.moved, s.fail), (0, 1));
    assert!(s.free_scanned <= FREE_SCAN, "the free scanner read {} frames in one run", s.free_scanned);
    // The next asker goes on from where the scanner stopped.
    assert!(b.compact_direct(3), "{:?}", b.compact_stats());
    let s = b.compact_stats();
    assert_eq!(s.moved, 8);
    assert!(s.free_scanned <= 192 * 512, "no pageblock read twice: {} frames", s.free_scanned);
    assert_eq!(b.alloc(3, Zone::Normal, Kind::Other), Some(BASE));
    b.check().unwrap();
}

/// kcompactd's work is passes (`PASS_SLICES` slices of `SLICE` frames: 524288 frames, 2 GiB, per scanner). A
/// proactive run was a run from end to end of the zone: on the Mini-PC twelve of them a minute read 83 million
/// frames. Here 4 GiB of cache with free frames only in pageblocks 8..40: the free scanner has more than a pass
/// to read before its first target.
#[test]
fn a_proactive_run_is_one_pass_and_the_next_goes_on() {
    const PASS: u64 = 8 * 65536;
    let (b, _c) = super::compact::cached(4094);
    assert!(b.set_migrator(Box::leak(Box::new(Mover(b)))));
    holes(b, 8, 40);
    let s = b.compact_all(crate::buddy::How::Proactive, &mut || {});
    assert!(s.free_scanned <= PASS && s.migrate_scanned <= PASS, "one run read {} and {} frames", s.free_scanned, s.migrate_scanned);
    assert_eq!(s.moved, 0, "no target within the first pass");
    // The second run reads on below the first and reaches the free frames.
    let t = b.compact_all(crate::buddy::How::Proactive, &mut || {});
    assert!(t.free_scanned <= PASS && t.migrate_scanned <= PASS);
    assert!(t.moved > 0 && t.cleared > 0, "the second pass moved nothing: {t:?}");
    assert!(s.free_scanned + t.free_scanned <= 2047 * 512, "no pageblock read twice");
    b.check().unwrap();
}
