//! Allocations below an address limit (MM-4's DMA masks) and exact runs below it.
use super::*;

#[test]
fn limit_inside_dma32_is_honoured() {
    // Free memory at 1-64 MiB and 4 GiB+; a 16 MiB limit must only use the low part below 16 MiB.
    let b = staged(&[free(MIB, 64 * MIB), free(4 * GIB, 4 * GIB + 64 * MIB)]);
    let mut got = Vec::new();
    while let Some(p) = b.alloc_below(2, 16 * MIB, Kind::Dma) {
        assert!(p + 4 * PAGE <= 16 * MIB, "block {p:#x} ends above the limit");
        assert_eq!(p % (4 * PAGE), 0);
        got.push(p);
    }
    assert_eq!(got.len() as u64, (15 * MIB) / (4 * PAGE), "every block below the limit was used");
    assert!(b.alloc(2, Zone::Dma32, Kind::Dma).is_some(), "memory above the limit is still there");
    for p in got {
        b.free(p, 2, Kind::Dma);
    }
    b.check().unwrap();
}

#[test]
fn limit_above_4g_prefers_normal_then_dma32() {
    let b = staged(&[free(MIB, 64 * MIB), free(4 * GIB, 4 * GIB + 64 * MIB)]);
    let p = b.alloc_below(0, 4 * GIB + 32 * MIB, Kind::Dma).unwrap();
    assert!((4 * GIB..4 * GIB + 32 * MIB).contains(&p), "NORMAL below the limit first: {p:#x}");
    // Use up NORMAL below the limit: the next block comes from DMA32, never from above the limit.
    let mut normal = std::vec![p];
    while let Some(q) = b.alloc_below(0, 4 * GIB + 32 * MIB, Kind::Dma) {
        if q < 4 * GIB {
            b.free(q, 0, Kind::Dma);
            break;
        }
        assert!(q < 4 * GIB + 32 * MIB);
        normal.push(q);
    }
    assert_eq!(normal.len() as u64, 32 * MIB / PAGE);
    for q in normal {
        b.free(q, 0, Kind::Dma);
    }
    b.check().unwrap();
}

#[test]
fn no_limit_is_the_ordinary_path_and_hostile_limits_fail_cleanly() {
    let b = staged(&[free(MIB, 64 * MIB)]);
    assert!(b.alloc_below(0, u64::MAX, Kind::Dma).is_some());
    assert_eq!(b.alloc_below(0, 0, Kind::Dma), None, "limit 0");
    assert_eq!(b.alloc_below(0, MIB, Kind::Dma), None, "nothing free below 1 MiB");
    assert_eq!(b.alloc_below(crate::MAX_ORDER + 1, 32 * MIB, Kind::Dma), None, "order too big");
    assert_eq!(b.alloc_below(14, 32 * MIB, Kind::Dma), None, "64 MiB block cannot end below 32 MiB");
    b.check().unwrap();
}

#[test]
fn exact_runs_below_a_limit_stay_inside_their_block() {
    let b = staged(&[free(MIB, 64 * MIB), free(4 * GIB, 4 * GIB + 64 * MIB)]);
    let used0 = b.stats().used_by_kind[Kind::Dma as usize];
    let mut runs = Vec::new();
    for pages in [1u64, 3, 5, 15, 16, 17, 100] {
        let p = b.alloc_exact_below(pages, 1, 32 * MIB, Kind::Dma).unwrap();
        assert!(p + pages * PAGE <= 32 * MIB);
        let block = PAGE << crate::buddy::exact_order(pages, 1).unwrap();
        assert_eq!(p % block, 0, "run starts at its covering block");
        assert!(b.run_allocated(p, pages, Kind::Dma));
        runs.push((p, pages));
    }
    assert_eq!(b.stats().used_by_kind[Kind::Dma as usize] - used0, runs.iter().map(|r| r.1 * PAGE).sum::<u64>(), "only the run is charged");
    for (p, pages) in runs {
        assert!(b.free_exact(p, pages, Kind::Dma));
    }
    assert_eq!(b.stats().used_by_kind[Kind::Dma as usize], used0);
    b.check().unwrap();
}
