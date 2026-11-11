//! W35 MM-3: what compaction costs, a plain timed loop (no criterion). Run for numbers with
//! `cargo test -p mm-core --release compact_bench -- --nocapture`; a debug run only checks the loops.
use super::compact::cached;
use super::*;
use crate::buddy::{Control, How};
use std::time::Instant;

#[test]
fn compact_bench() {
    let mib = if cfg!(debug_assertions) { 64 } else { 1024 };
    let (b, c) = cached(mib);
    let n = if cfg!(debug_assertions) { 4 } else { 64 };
    // 1. A direct run that clears one 2 MiB block of clean cache (512 drops), the block taken back as cache.
    let t = Instant::now();
    for _ in 0..n {
        assert!(b.compact_direct(9));
        let p = b.alloc(9, Zone::Normal, Kind::Other).unwrap();
        b.free(p, 9, Kind::Other);
        for _ in 0..512 {
            let f = b.alloc(0, Zone::Normal, Kind::PageCache).unwrap();
            b.set_owner(f, f / PAGE + 1);
        }
    }
    let direct_us = t.elapsed().as_nanos() as f64 / n as f64 / 1000.0;
    // 2. The migrate scanner over memory where nothing can move (one refused frame per pageblock): the cost of
    //    reading frame states.
    for f in 0..(mib << 8) {
        c.pin(2 * MIB + f * PAGE);
    }
    b.reset_skip_hints();
    let _inside = crate::reclaim::enter();
    let mut ctl = Control::new(Some(9), How::Daemon, 0);
    let t = Instant::now();
    let _ = b.compact_zone(&mut ctl, u64::MAX);
    let scanned = ctl.stats().migrate_scanned.max(1);
    let scan_ns = t.elapsed().as_nanos() as f64 / scanned as f64;
    std::println!("compact_bench: direct run clearing one 2 MiB block {direct_us:.1} us, migrate scanner {scan_ns:.2} ns/frame over {} MiB", mib);
    b.check().unwrap();
}
