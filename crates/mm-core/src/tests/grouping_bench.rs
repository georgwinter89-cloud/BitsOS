//! W35 MM-1: the allocation cost of the buddy before and after mobility grouping, a plain timed loop (no criterion).
//! Run for numbers with `cargo test -p mm-core --release grouping_bench -- --nocapture`; in a debug run it only
//! checks that the loops work (the numbers then mean nothing).
use super::*;
use std::time::Instant;

const MIXED: [Kind; 2] = [Kind::User, Kind::Other];

fn per_op(t: Instant, ops: u64) -> f64 {
    t.elapsed().as_nanos() as f64 / ops as f64
}

#[test]
fn grouping_bench() {
    let b = staged(&[free(16 * MIB, 16 * MIB + 2 * GIB)]);
    let n: u64 = if cfg!(debug_assertions) { 20_000 } else { 400_000 };
    // 1. One frame taken and given back: the per-CPU list only.
    let t = Instant::now();
    for _ in 0..n {
        let p = b.alloc(0, Zone::Normal, Kind::User).unwrap();
        b.free(p, 0, Kind::User);
    }
    let pcp = per_op(t, n);
    // 2. Many single frames, then all back: per-CPU refills and returns through the zone's bitmaps.
    let mut held = Vec::with_capacity(n as usize);
    let t = Instant::now();
    for i in 0..n {
        held.push(b.alloc(0, Zone::Normal, MIXED[(i % 2) as usize]).unwrap());
    }
    for (i, &p) in held.iter().enumerate() {
        b.free(p, 0, MIXED[i % 2]);
    }
    let bulk = per_op(t, 2 * n);
    held.clear();
    // 3. Order-3 blocks taken and given back (no per-CPU list).
    let t = Instant::now();
    for i in 0..n {
        let k = MIXED[(i % 2) as usize];
        let p = b.alloc(3, Zone::Normal, k).unwrap();
        b.free(p, 3, k);
    }
    let o3 = per_op(t, n);
    // 4. Order-3 blocks held in a mix, then all back (splits, merges).
    let m = n / 8;
    let t = Instant::now();
    for i in 0..m {
        held.push(b.alloc(3, Zone::Normal, MIXED[(i % 2) as usize]).unwrap());
    }
    for (i, &p) in held.iter().enumerate() {
        b.free(p, 3, MIXED[i % 2]);
    }
    let o3_bulk = per_op(t, 2 * m);
    std::println!("grouping_bench ns/op: pcp pair {pcp:.1}, order-0 bulk {bulk:.1}, order-3 pair {o3:.1}, order-3 bulk {o3_bulk:.1}");
    b.check().unwrap();
}
