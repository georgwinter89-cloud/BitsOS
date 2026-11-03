//! MM-14: the block walk of scattered DMA frames (largest fitting order first, smaller on a refusal) and the
//! page-to-block lookup, against a staged buddy.
use super::*;
use crate::dma::DmaAllocError;
use crate::dma_pages::{fill, next_order, run_index};

#[test]
fn next_order_is_the_largest_fitting_power_capped() {
    assert_eq!(next_order(0, 9), 0);
    assert_eq!(next_order(1, 9), 0);
    assert_eq!(next_order(300, 9), 8);
    assert_eq!(next_order(512, 9), 9);
    assert_eq!(next_order(u64::MAX, 9), 9);
    assert_eq!(next_order(300, 0), 0);
}

#[test]
fn run_index_finds_the_block_of_a_page() {
    let firsts = [0u64, 256, 288, 296, 300];
    let f = |i: usize| firsts[i];
    assert_eq!(run_index(firsts.len(), f, 0), Some(0));
    assert_eq!(run_index(firsts.len(), f, 255), Some(0));
    assert_eq!(run_index(firsts.len(), f, 256), Some(1));
    assert_eq!(run_index(firsts.len(), f, 295), Some(2));
    assert_eq!(run_index(firsts.len(), f, 299), Some(3));
    assert_eq!(run_index(firsts.len(), f, 1000), Some(4), "past the end is the caller's bound check");
    assert_eq!(run_index(0, f, 0), None);
}

/// Takes blocks of the staged buddy for `fill` and records them.
fn walk(b: &BuddyFrames, pages: u64, cap: u32, fail_after: u64) -> (Result<(), DmaAllocError>, Vec<(u64, u32)>) {
    let mut got = Vec::new();
    let r = fill(pages, cap, fail_after, |order| {
        let p = if order > 0 { b.alloc_light(order, Zone::Normal, Kind::Dma) } else { b.alloc(0, Zone::Normal, Kind::Dma) };
        Ok(p.map(|p| got.push((p, order))).is_some())
    });
    (r, got)
}

#[test]
fn fill_takes_the_largest_blocks_first() {
    let b = staged(&[free(4 * GIB, 4 * GIB + 64 * MIB)]);
    let (r, got) = walk(&b, 300, 9, 0);
    assert!(r.is_ok());
    assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), [8, 5, 3, 2], "256 + 32 + 8 + 4 pages");
    assert_disjoint_inside(&got, &[(4 * GIB, 4 * GIB + 64 * MIB)]);
    for (p, o) in got {
        b.free(p, o, Kind::Dma);
    }
    b.check().unwrap();
}

#[test]
fn fill_falls_back_to_single_frames_when_fragmented() {
    // Every other frame taken: no block above order 0 is free, the walk must step down to single frames.
    let b = staged(&[free(4 * GIB, 4 * GIB + 4 * MIB)]);
    let mut frames: Vec<u64> = Vec::new();
    while let Some(p) = b.alloc(0, Zone::Normal, Kind::Other) {
        frames.push(p);
    }
    frames.sort();
    let mut keep = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        if i % 2 == 0 {
            b.free(*f, 0, Kind::Other);
        } else {
            keep.push(*f);
        }
    }
    let (r, got) = walk(&b, 100, 9, 0);
    assert!(r.is_ok());
    assert_eq!(got.len(), 100);
    assert!(got.iter().all(|g| g.1 == 0));
    // Asking for more than is free: NoMemory, the caller gives back what was taken.
    let (r2, got2) = walk(&b, frames.len() as u64, 9, 0);
    assert_eq!(r2, Err(DmaAllocError::NoMemory));
    for (p, o) in got.into_iter().chain(got2) {
        b.free(p, o, Kind::Dma);
    }
    for f in keep {
        b.free(f, 0, Kind::Other);
    }
    b.check().unwrap();
}

#[test]
fn fill_stops_at_the_test_limit() {
    let b = staged(&[free(4 * GIB, 4 * GIB + 64 * MIB)]);
    let (r, got) = walk(&b, 300, 9, 260);
    assert_eq!(r, Err(DmaAllocError::NoMemory));
    let held: u64 = got.iter().map(|g| 1u64 << g.1).sum();
    assert!(held <= 260, "{held} pages held past the limit");
    for (p, o) in got {
        b.free(p, o, Kind::Dma);
    }
    b.check().unwrap();
}
