//! Exact runs out of the HighAtomic reserve (`buddy::exact` over `buddy::highatomic`): the heap's 5-page block for
//! a caller under a spin lock is an order-3 block trimmed to 4 + 1 frames. Desk 12.11.2026: does each such take
//! cost the reserve frames or an order-3 block for good (the trimmed tail, the pieces, the merge)?
use super::*;
use crate::{Buddy, MigrateType, PAGEBLOCK_FRAMES, PAGEBLOCK_ORDER};

const HA: usize = MigrateType::HighAtomic as usize;
const END: u64 = 80 * MIB;

/// 64 MiB with a reserve of two pageblocks; everything else held as single frames, so every block of two frames
/// or more comes from the reserve.
fn full_but_the_reserve() -> (BuddyFrames, Vec<u64>) {
    let b = staged(&[free(16 * MIB, END)]);
    assert_eq!(b.reserve_highatomic_in(0, 2), 2);
    let held: Vec<u64> = core::iter::from_fn(|| b.alloc(0, Zone::Normal, Kind::Other)).collect();
    assert_eq!(held.len() as u64, 30 * PAGEBLOCK_FRAMES);
    (b, held)
}

fn in_reserve(b: &Buddy, p: u64) -> bool {
    b.pageblock_type(p) == Some(MigrateType::HighAtomic)
}

#[test]
fn a_short_lived_run_gives_the_reserve_everything_back() {
    let (b, _held) = full_but_the_reserve();
    let whole = b.zone_info(0);
    let _lock = ksync::PreemptGuard::new();
    for round in 0..5000 {
        let p = b.alloc_exact(5, 1, Zone::Normal, Kind::Heap).expect("a 5-page run out of the reserve");
        assert!(in_reserve(&b, p));
        // While it lives: its five frames are gone, the three trimmed ones are back in the reserve.
        assert_eq!(b.zone_info(0).free_highatomic, 2 * PAGEBLOCK_FRAMES - 5, "round {round}");
        assert!(b.free_exact(p, 5, Kind::Heap));
        let i = b.zone_info(0);
        assert_eq!((i.free_highatomic, i.free_by_type[HA], i.free_blocks), (whole.free_highatomic, whole.free_by_type[HA], whole.free_blocks), "round {round}");
    }
    assert_eq!(b.stats().used_by_kind[Kind::Heap as usize], 0);
    let s = b.reserve_stats();
    assert_eq!((s.takes[3], s.pages[Kind::Heap as usize], s.total_takes()), (5000, 5000 * 8, 5000));
    b.check().unwrap();
}

#[test]
fn runs_of_every_size_merge_back_whatever_the_order_of_their_frees() {
    let (b, _held) = full_but_the_reserve();
    let whole = b.zone_info(0);
    let _lock = ksync::PreemptGuard::new();
    let mut rng = Rng(0x12_11_2026);
    let mut live: Vec<(u64, u64)> = Vec::new();
    for step in 0..60_000 {
        if live.len() < 40 && rng.below(100) < 55 {
            let pages = 2 + rng.below(30);
            if let Some(p) = b.alloc_exact(pages, 1, Zone::Normal, Kind::Heap) {
                assert!(in_reserve(&b, p), "step {step}");
                live.push((p, pages));
            }
        } else if !live.is_empty() {
            let (p, pages) = live.swap_remove(rng.below(live.len() as u64) as usize);
            assert!(b.free_exact(p, pages, Kind::Heap), "step {step}");
        }
        let held: u64 = live.iter().map(|l| l.1).sum();
        assert_eq!(b.zone_info(0).free_highatomic, 2 * PAGEBLOCK_FRAMES - held, "step {step}: frames lost or found");
    }
    for (p, pages) in live {
        assert!(b.free_exact(p, pages, Kind::Heap));
    }
    let i = b.zone_info(0);
    assert_eq!((i.free_highatomic, i.free_by_type[HA]), (whole.free_highatomic, whole.free_by_type[HA]), "the reserve is two pageblocks in halves again");
    assert_eq!(i.free_by_type[HA][PAGEBLOCK_ORDER as usize - 1], 4);
    b.check().unwrap();
}

/// What does cost an order-3 block: a long-lived small block that settles in the trimmed tail of a run while the
/// run lives (the smallest free block of the reserve is that tail).
#[test]
fn a_small_block_taken_beside_a_live_run_settles_in_its_tail() {
    let (b, _held) = full_but_the_reserve();
    let _lock = ksync::PreemptGuard::new();
    let run = b.alloc_exact(5, 1, Zone::Normal, Kind::Heap).unwrap();
    let slab = b.alloc(1, Zone::Normal, Kind::Heap).expect("an order-1 block out of the reserve");
    assert_eq!(slab, run + 6 * PAGE, "the run's trimmed order-1 tail");
    assert!(b.free_exact(run, 5, Kind::Heap));
    let i = b.zone_info(0);
    assert_eq!(i.free_highatomic, 2 * PAGEBLOCK_FRAMES - 2);
    // The order-3 block of the run cannot form again: one order-2, one order-1 block instead.
    assert_eq!((i.free_by_type[HA][1], i.free_by_type[HA][2], i.free_by_type[HA][3]), (1, 1, 1));
    b.free(slab, 1, Kind::Heap);
    b.check().unwrap();
}
