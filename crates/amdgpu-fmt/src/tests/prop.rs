//! Property tests: random map/unmap/clear sequences, with failing stores, checked after every step.

use std::vec::Vec;

use super::model::{self, MKind, MMap, Model};
use super::{MemStore, Rng};
use crate::pte::{MapFlags, Mtype};
use crate::vm::{Vm, VmError};
use crate::{GPU_PAGE, addr};

/// The VA window in pages: 16 GiB, so 1 GiB leaves, collisions and splits all happen.
const WINDOW: u64 = 1 << 22;

fn flags(rng: &mut Rng) -> MapFlags {
    MapFlags {
        read: rng.chance(90),
        write: rng.chance(70),
        exec: rng.chance(20),
        mtype: [Mtype::Nc, Mtype::Wc, Mtype::Cc, Mtype::Uc][rng.below(4) as usize],
        tmz: rng.chance(5),
        noalloc: rng.chance(5),
    }
}

/// A start and length (pages) that like big aligned blocks.
fn shape(rng: &mut Rng, max_pages: u64) -> (u64, u64) {
    let align = match rng.below(10) {
        0..=3 => 1,
        4..=5 => 16,
        6..=7 => 512,
        _ => 1 << 18,
    };
    let start = rng.below(WINDOW / align) * align;
    let len = match rng.below(6) {
        0 => align,
        1 => align * (1 + rng.below(4)),
        _ => 1 + rng.below(max_pages.min(2048)),
    };
    (start, len.min(max_pages).min(WINDOW - start).max(1))
}

fn kind(rng: &mut Rng, start: u64, pages: u64) -> MKind {
    match rng.below(10) {
        0..=5 => {
            // Physical offset: same alignment as the VA for huge leaves, or a smaller one.
            let a = 1u64 << rng.below(20);
            let base = 0x80_0000_0000u64 + (rng.below(64) << 30) + rng.below(4) * GPU_PAGE * 512;
            let phys = if rng.chance(70) { base + start * GPU_PAGE % (1 << 30) } else { base + a * GPU_PAGE };
            MKind::Local(phys)
        }
        6..=8 if pages <= 2048 => {
            let v = (0..pages).map(|_| (1 + rng.below(1 << 30)) * GPU_PAGE).collect();
            MKind::System(v, rng.chance(80))
        }
        _ => MKind::Prt,
    }
}

/// Counts: leaf levels PDB1/PDB0/PTB, PRT, out-of-memory refusals, overlaps, splitting clears.
fn run(seed: u64, steps: usize, stats: &mut [usize; 7]) {
    let mut rng = Rng::new(seed);
    let mut store = MemStore::new();
    let mut vm = Vm::new(&mut store, super::whole()).unwrap();
    let mut model = Model::default();
    let mut flushed = 0;
    for _ in 0..steps {
        // Sometimes the store runs dry part way: the operation fails and changes nothing.
        let fail = rng.chance(10).then(|| rng.below(4) as usize);
        store.fail_after = fail;
        let before_seq = vm.tlb_seq();
        let before_tables = store.tables.len();
        let r: Result<u64, VmError> = match rng.below(10) {
            0..=4 => {
                let (s, n) = shape(&mut rng, 1 << 19);
                let k = kind(&mut rng, s, n);
                let f = flags(&mut rng);
                let va = addr::sign_extend(s * GPU_PAGE);
                let r = vm.map(&mut store, va, n * GPU_PAGE, model::backing(&k), f);
                if model.overlaps(s, s + n) {
                    assert!(matches!(r, Err(VmError::Overlap(_))), "{r:?}");
                } else if r.is_ok() {
                    model.map(s, MMap { pages: n, kind: k, flags: f });
                }
                r
            }
            5..=6 => {
                let keys: Vec<u64> = model.maps.keys().copied().collect();
                let s = if !keys.is_empty() && rng.chance(85) {
                    keys[rng.below(keys.len() as u64) as usize]
                } else {
                    rng.below(WINDOW)
                };
                let r = vm.unmap(&mut store, addr::sign_extend(s * GPU_PAGE));
                match (&r, model.maps.contains_key(&s)) {
                    (Ok(_), true) => {
                        model.maps.remove(&s);
                    }
                    (Err(VmError::NotMapped(_)), false) => {}
                    other => panic!("unmap {s:#x}: {other:?}"),
                }
                r
            }
            7..=8 => {
                let (s, n) = shape(&mut rng, 1 << 19);
                if model.maps.iter().any(|(&ms, m)| (ms < s && s < ms + m.pages) || (ms < s + n && s + n < ms + m.pages)) {
                    stats[6] += 1;
                }
                let r = vm.clear(&mut store, addr::sign_extend(s * GPU_PAGE), n * GPU_PAGE);
                if r.is_ok() {
                    model.clear(s, s + n);
                }
                r
            }
            _ => {
                let to = flushed + rng.below(vm.tlb_seq() - flushed + 1);
                vm.reclaim(&mut store, to);
                flushed = to;
                Ok(vm.tlb_seq())
            }
        };
        store.fail_after = None;
        match r {
            Err(VmError::NoMemory) => {
                stats[4] += 1;
                assert!(fail.is_some());
                assert_eq!(vm.tlb_seq(), before_seq, "a failed commit moved the sequence");
                assert_eq!(store.tables.len(), before_tables, "a failed commit kept tables");
            }
            Err(VmError::Overlap(_)) => stats[5] += 1,
            Err(VmError::NotMapped(_)) | Ok(_) => {}
            Err(e) => panic!("seed {seed}: {e:?}"),
        }
        let seen = model::check(&vm, &store, &model, &mut rng, WINDOW);
        for (k, n) in seen.into_iter().enumerate() {
            stats[k] += n;
        }
    }
    // Unmapping everything leaves only the root, and every table returns after the last flush.
    let keys: Vec<u64> = model.maps.keys().copied().collect();
    for s in keys {
        vm.unmap(&mut store, addr::sign_extend(s * GPU_PAGE)).unwrap();
    }
    assert_eq!(vm.table_count(), 1);
    vm.reclaim(&mut store, vm.tlb_seq());
    assert_eq!(store.tables.len(), 1);
    vm.destroy(&mut store);
    assert!(store.tables.is_empty());
}

#[test]
fn random_sequences() {
    let mut stats = [0; 7];
    for seed in 1..=300 {
        run(seed, 40, &mut stats);
    }
    // Every interesting case happened (PDB1/PDB0/PTB leaves, PRT, out of memory, overlap, split).
    std::eprintln!("gr1 prop stats {stats:?}");
    assert!(stats.iter().all(|&n| n > 0), "{stats:?}");
}

#[test]
fn long_sequences() {
    let mut stats = [0; 7];
    for seed in 1000..1010 {
        run(seed, 400, &mut stats);
    }
    std::eprintln!("gr1 prop stats {stats:?}");
    assert!(stats.iter().all(|&n| n > 0), "{stats:?}");
}
