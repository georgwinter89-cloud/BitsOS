//! Random alloc/free runs against a model (no overlap, accounting exact, everything merges back), and
//! several threads hammering one allocator (the per-CPU list and the zone locks under contention).
use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;

#[test]
fn random_run_against_a_model() {
    let r = [free(0x10_0000, 0x7F0_0000), rsv(0x200_0000, 0x200_3000), free(4 * GIB, 4 * GIB + 96 * MIB), free(0x900_0000, 0xA00_0000)];
    let b = staged(&r);
    let total_free = b.stats().free_bytes;
    let initial = (b.zone_info(0).free_blocks, b.zone_info(1).free_blocks);
    let kinds = [Kind::Heap, Kind::User, Kind::Dma, Kind::Stack, Kind::PageTable];
    let mut live: BTreeMap<u64, (u32, Kind)> = BTreeMap::new();
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut used = [0u64; crate::KINDS];
    for step in 0..60_000 {
        if live.is_empty() || rng.below(100) < 55 {
            let order = match rng.below(10) {
                0..=5 => 0,
                6 | 7 => rng.below(4) as u32,
                8 => rng.below(10) as u32,
                _ => rng.below(crate::MAX_ORDER as u64 + 1) as u32,
            };
            let zone = if rng.below(4) == 0 { Zone::Dma32 } else { Zone::Normal };
            let kind = kinds[rng.below(kinds.len() as u64) as usize];
            if let Some(p) = b.alloc(order, zone, kind) {
                assert_eq!(p % (PAGE << order), 0);
                if zone == Zone::Dma32 {
                    assert!(p + (PAGE << order) <= 4 * GIB);
                }
                let end = p + (PAGE << order);
                if let Some((&q, &(qo, _))) = live.range(..end).next_back() {
                    assert!(q + (PAGE << qo) <= p, "step {step}: {p:#x} overlaps live {q:#x}");
                }
                live.insert(p, (order, kind));
                used[kind as usize] += PAGE << order;
            }
        } else {
            let i = rng.below(live.len() as u64) as usize;
            let (&p, &(o, k)) = live.iter().nth(i).unwrap();
            live.remove(&p);
            b.free(p, o, k);
            used[k as usize] -= PAGE << o;
        }
        if step % 5000 == 0 {
            assert_eq!(b.stats().used_by_kind, used, "step {step}");
            b.check().unwrap();
        }
    }
    for (p, (o, k)) in std::mem::take(&mut live) {
        b.free(p, o, k);
    }
    b.check().unwrap();
    assert_eq!(b.stats().free_bytes, total_free);
    assert_eq!((b.zone_info(0).free_blocks, b.zone_info(1).free_blocks), initial, "the same blocks as at the start");
    assert_eq!(b.reports().bad_frees.load(core::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn threads_share_one_allocator() {
    let b = Arc::new(staged(&[free(0x10_0000, 0x10_0000 + 256 * MIB)]));
    let free_before = b.stats().free_bytes;
    let handles: Vec<_> = (0..4u64)
        .map(|t| {
            let b = b.clone();
            std::thread::spawn(move || {
                let mut rng = Rng(0xABCD_0000 + t);
                let mut mine = Vec::new();
                for _ in 0..20_000 {
                    if mine.is_empty() || rng.below(2) == 0 {
                        let o = if rng.below(4) == 0 { rng.below(6) as u32 } else { 0 };
                        if let Some(p) = b.alloc(o, Zone::Normal, Kind::User) {
                            mine.push((p, o));
                        }
                    } else {
                        let (p, o) = mine.swap_remove(rng.below(mine.len() as u64) as usize);
                        b.free(p, o, Kind::User);
                    }
                }
                mine
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    assert_disjoint_inside(&all, &[(0x10_0000, 0x10_0000 + 256 * MIB)]);
    for (p, o) in all {
        b.free(p, o, Kind::User);
    }
    b.check().unwrap();
    assert_eq!(b.stats().free_bytes, free_before);
    assert_eq!(b.reports().bad_frees.load(core::sync::atomic::Ordering::Relaxed), 0);
}
