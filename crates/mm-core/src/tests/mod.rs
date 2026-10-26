//! Host tests of the buddy allocator: math (split, merge, zones, alignment), bad frees, per-CPU lists,
//! fragmentation, staged firmware maps (V1 `k7-model.mjs` style) and a random run against a model.
extern crate std;

mod basic;
mod guards;
mod heap;
mod heap_guard;
mod limit;
mod maps;
mod reclaim;
mod stress;

use crate::{BuddyFrames, FrameAlloc, Kind, MapRange, RangeUse, Storage, Zone, PAGE};
use std::vec::Vec;

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

pub fn free(s: u64, e: u64) -> MapRange {
    MapRange::new(s, e, RangeUse::Free)
}
pub fn rsv(s: u64, e: u64) -> MapRange {
    MapRange::new(s, e, RangeUse::Reserved)
}
pub fn module(s: u64, e: u64) -> MapRange {
    MapRange::new(s, e, RangeUse::Module)
}

pub fn staged(r: &[MapRange]) -> BuddyFrames {
    BuddyFrames::staged("test", r, Storage::Heap).unwrap()
}

/// Takes everything, biggest blocks first; returns (phys, order).
pub fn take_all(b: &BuddyFrames, zone: Zone) -> Vec<(u64, u32)> {
    let mut got = Vec::new();
    for order in (0..=crate::MAX_ORDER).rev() {
        while let Some(p) = b.alloc(order, zone, Kind::Other) {
            got.push((p, order));
        }
    }
    got
}

/// Panics if two blocks overlap or one touches a byte outside `allowed` (sorted, disjoint byte ranges).
pub fn assert_disjoint_inside(blocks: &[(u64, u32)], allowed: &[(u64, u64)]) {
    let mut v: Vec<(u64, u64)> = blocks.iter().map(|&(p, o)| (p, p + (PAGE << o))).collect();
    v.sort();
    for w in v.windows(2) {
        assert!(w[0].1 <= w[1].0, "blocks overlap: {:#x?} {:#x?}", w[0], w[1]);
    }
    for &(s, e) in &v {
        assert_eq!(s % (e - s), 0, "block {s:#x} not aligned to its size");
        let i = allowed.partition_point(|a| a.0 <= s);
        assert!(i > 0 && e <= allowed[i - 1].1, "block {s:#x}..{e:#x} outside free memory");
    }
}

/// Small xorshift for repeatable random runs.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
