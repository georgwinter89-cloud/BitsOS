//! Host tests: model-based random sequences (with and without power cuts), plus targeted cases.

use crate::layout::{LEAF_FANOUT, PAGE};
use crate::memdev::MemDev;
use crate::model::{RunCfg, Runner, fill, stamp_of};
use crate::{Config, Pool, PoolHandle, VolError};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::{BlockDevice, IoPriority::Normal, Snapshots};

mod window;

pub(crate) fn dev(bs: u32, chunks: u64, chunk: u32, max: u32) -> Arc<MemDev> {
    Arc::new(MemDev::new("t", bs, chunks * (chunk / bs) as u64, max).unwrap())
}

pub(crate) fn cfg(chunk: u32) -> Config {
    Config { chunk_size: chunk, reserve_chunks: Some(4), cache_pages: 32, commit_dirty_pages: 64, quiet: true }
}

fn run(c: RunCfg) {
    let r = Runner::new(c).and_then(|mut r| r.run());
    match r {
        Ok(rep) => std::println!("seed {}: {rep:?}", c.seed),
        Err(m) => panic!("{m}"),
    }
}

pub(crate) fn block(stamp: u64, bs: usize) -> Vec<u8> {
    let mut b = vec![0u8; bs];
    fill(&mut b, stamp);
    b
}

pub(crate) fn read_stamp(p: &mut Pool, id: u64, lba: u64) -> u64 {
    let mut b = vec![0u8; p.block_size() as usize];
    p.read(id, lba, &mut b, Normal).unwrap();
    stamp_of(&b).unwrap()
}

#[test]
fn model_random_4k_chunks() {
    // Odd seeds on a roomy pool, even ones on a tight pool that runs out of space often.
    for seed in 1..=8 {
        let dev_chunks = if seed % 2 == 0 { 64 } else { 160 };
        run(RunCfg { dev_chunks, ..RunCfg::small(seed, 3000) });
    }
}

#[test]
fn model_random_64k_chunks() {
    for seed in 11..=14 {
        let c = RunCfg { block_size: 512, chunk_size: 65536, dev_chunks: 64, vol_chunks: 12, max_req: 64, cache_pages: 8, ..RunCfg::small(seed, 1500) };
        run(c);
    }
}

#[test]
fn model_random_4k_blocks() {
    for seed in 21..=24 {
        let c = RunCfg { block_size: 4096, chunk_size: 8192, dev_chunks: 120, vol_chunks: 16, max_req: 3, ..RunCfg::small(seed, 2000) };
        run(c);
    }
}

#[test]
fn model_large_volumes_deep_trees() {
    // 600 chunks per volume need a second tree level (254 entries per leaf).
    let c = RunCfg { dev_chunks: 1400, vol_chunks: 600, commit_dirty_pages: 256, ..RunCfg::small(31, 1500) };
    run(c);
}

#[test]
fn crash_after_every_write() {
    for seed in 41..=48 {
        let dev_chunks = if seed % 2 == 0 { 44 } else { 96 };
        let c = RunCfg { dev_chunks, vol_chunks: 10, crash: true, ..RunCfg::small(seed, 300) };
        run(c);
    }
}

#[test]
fn crash_after_every_write_64k_chunks() {
    for seed in 51..=52 {
        let c = RunCfg { chunk_size: 65536, dev_chunks: 36, vol_chunks: 6, max_req: 32, crash: true, ..RunCfg::small(seed, 200) };
        run(c);
    }
}

#[test]
fn blank_device_is_not_a_pool() {
    let d = dev(512, 64, 4096, 8);
    assert_eq!(Pool::open(d, cfg(4096)).err(), Some(VolError::NotFound));
}

#[test]
fn geometry_is_checked() {
    let d = dev(512, 64, 4096, 8);
    assert_eq!(Pool::format(d.clone(), cfg(4096 + 512)).err(), Some(VolError::InvalidArgument));
    assert_eq!(Pool::format(d.clone(), cfg(2048)).err(), Some(VolError::InvalidArgument));
    let tiny = dev(512, 6, 4096, 8);
    assert_eq!(Pool::format(tiny, cfg(4096)).err(), Some(VolError::InvalidArgument));
}

#[test]
fn superblocks_alternate_and_a_torn_commit_falls_back() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 64 * 512, 1).unwrap();
    p.write(v, 3, &block(7, 512), false, Normal).unwrap();
    p.commit().unwrap();
    let g = p.generation();
    let img = d.image();
    let live = |s: usize| crate::layout::Super::decode(&img[s * PAGE..(s + 1) * PAGE]).map(|sb| sb.generation);
    assert_eq!((live((g % 2) as usize), live(((g + 1) % 2) as usize)), (Some(g), Some(g - 1)));
    p.write(v, 3, &block(8, 512), false, Normal).unwrap();
    p.write(v, 40, &block(9, 512), false, Normal).unwrap();
    // Cut the power at every write of the next commit, the superblock write torn included.
    d.set_capture(true, 5);
    p.commit().unwrap();
    d.set_capture(false, 0);
    assert_eq!(p.generation(), g + 1);
    let mut seen = std::collections::BTreeSet::new();
    for img in d.take_images() {
        let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, img).unwrap()), cfg(4096)).unwrap();
        seen.insert(q.generation());
        // Block 3's chunk was mapped before and is overwritten in place: either content is fine.
        assert!([7, 8].contains(&read_stamp(&mut q, v, 3)));
        assert_eq!(read_stamp(&mut q, v, 40), if q.generation() == g { 0 } else { 9 });
    }
    assert!(seen.contains(&g) && seen.iter().all(|&x| x == g || x == g + 1), "{seen:?}");
    let mut p3 = Pool::open(d, cfg(4096)).unwrap();
    assert_eq!(p3.generation(), g + 1);
    assert_eq!(read_stamp(&mut p3, v, 40), 9);
}

#[test]
fn corrupt_images_never_panic() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 32 * 4096, 1).unwrap();
    for i in 0..32u64 {
        p.write(v, i * 8, &block(i + 1, 512), false, Normal).unwrap();
    }
    p.snapshot(v, "s", 2, false).unwrap();
    p.commit().unwrap();
    let clean = d.image();
    let mut rng = 0x1234_5678u64;
    let mut opened = 0;
    for _ in 0..400 {
        let mut img = clean.clone();
        for _ in 0..1 + crate::memdev::xorshift(&mut rng) % 8 {
            let at = (crate::memdev::xorshift(&mut rng) % img.len() as u64) as usize;
            img[at] ^= 1 << (crate::memdev::xorshift(&mut rng) % 8);
        }
        let d2 = Arc::new(MemDev::from_image("c", 512, 8, img).unwrap());
        if let Ok(mut p2) = Pool::open(d2, cfg(4096)) {
            opened += 1;
            for info in p2.list().unwrap() {
                let mut buf = vec![0u8; info.size as usize];
                let _ = p2.read(info.id, 0, &mut buf, Normal);
            }
            let _ = p2.write(v, 0, &block(99, 512), true, Normal);
        }
    }
    assert!(opened > 0, "flips in data chunks leave the pool usable");
}

#[test]
fn enospc_drops_auto_snapshots_oldest_first() {
    let d = dev(512, 70, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    let all = |p: &mut Pool, s: u64| {
        for c in 0..16u64 {
            p.write(v, c * 8, &block(s + c, 512), false, Normal).unwrap();
        }
    };
    all(&mut p, 100);
    let s1 = p.snapshot(v, "oldest", 10, true).unwrap();
    all(&mut p, 200);
    let s2 = p.snapshot(v, "newer", 20, true).unwrap();
    all(&mut p, 300);
    let keep = p.snapshot(v, "kept", 30, false).unwrap();
    // The fourth rewrite does not fit while all three snapshots hold their chunks: the oldest automatic
    // snapshot goes, the newer one and the manual one stay.
    all(&mut p, 400);
    let ids: Vec<u64> = p.list().unwrap().iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![v, s2, keep]);
    assert_eq!(p.stats().dropped, 1);
    assert_eq!(read_stamp(&mut p, s2, 8), 201);
    assert_eq!(read_stamp(&mut p, keep, 8), 301);
    assert_eq!(read_stamp(&mut p, v, 8), 401);
    let _ = s1;
}

#[test]
fn enospc_reaches_the_writer() {
    let d = dev(512, 30, 4096, 8);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    let v = p.create_volume("a", 100 * 4096, 1).unwrap();
    let mut err = None;
    for c in 0..100u64 {
        if let Err(e) = p.write(v, c * 8, &block(c + 1, 512), false, Normal) {
            err = Some((c, e));
            break;
        }
    }
    let (c, e) = err.expect("the pool is smaller than the volume");
    assert_eq!(e, VolError::NoSpace);
    assert_eq!(e.errno(), 28);
    assert!(c > 10);
    // Everything written before is still there, and freeing space makes writes work again.
    assert_eq!(read_stamp(&mut p, v, 8), 2);
    p.discard(v, 0, 8 * 4).unwrap();
    p.commit().unwrap();
    p.write(v, c * 8, &block(500, 512), false, Normal).unwrap();
    assert_eq!(read_stamp(&mut p, v, c * 8), 500);
}

#[test]
fn discard_frees_chunks_and_partial_chunks_stay() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 32 * 4096, 1).unwrap();
    for c in 0..32u64 {
        p.write(v, c * 8, &block(c + 1, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    let before = p.stats().free;
    // Blocks 4..60: chunks 0 and 7 only in part, chunks 1..=6 whole.
    p.discard(v, 4, 56).unwrap();
    assert_eq!(p.stats().pending, 6);
    p.commit().unwrap();
    assert!(p.stats().free >= before + 6);
    assert_eq!(read_stamp(&mut p, v, 0), 1);
    assert_eq!(read_stamp(&mut p, v, 8), 0);
    assert_eq!(read_stamp(&mut p, v, 56), 8);
    let digest = p.alloc_digest();
    drop(p);
    let p2 = Pool::open(d, cfg(4096)).unwrap();
    assert_eq!(p2.alloc_digest(), digest);
}

#[test]
fn rollback_is_undoable() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    let s = p.snapshot(v, "yesterday", 2, false).unwrap();
    p.write(v, 0, &block(2, 512), false, Normal).unwrap();
    p.write(v, 9, &block(3, 512), false, Normal).unwrap();
    let now = p.rollback(v, s, "before rollback", 3).unwrap();
    assert_eq!((read_stamp(&mut p, v, 0), read_stamp(&mut p, v, 9)), (1, 0));
    // Writing after the rollback leaves the snapshot alone.
    p.write(v, 0, &block(4, 512), false, Normal).unwrap();
    assert_eq!(read_stamp(&mut p, s, 0), 1);
    let undo = p.rollback(v, now, "undo", 4).unwrap();
    assert_eq!((read_stamp(&mut p, v, 0), read_stamp(&mut p, v, 9)), (2, 3));
    assert_eq!(read_stamp(&mut p, undo, 0), 4);
    assert_eq!(p.rollback(v, v, "x", 5).err(), Some(VolError::InvalidArgument));
    assert_eq!(p.write(s, 0, &block(5, 512), false, Normal).err(), Some(VolError::ReadOnly));
}

/// VS-3: a rollback scheduled while the volume is in use happens at its next first open, keeps the state
/// before as "before-rollback" (so it is undone the same way), survives a reopen of the pool, and pins its
/// snapshot against deletes.
#[test]
fn scheduled_rollback_applies_at_next_open() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("data", 16 * 4096, 1).unwrap();
    let w = p.create_volume("other", 16 * 4096, 1).unwrap();
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    let s = p.snapshot(v, "15:00", 2, true).unwrap();
    let ws = p.snapshot(w, "w", 2, false).unwrap();
    p.write(v, 0, &block(2, 512), false, Normal).unwrap();
    assert_eq!(p.schedule_rollback(v, ws).err(), Some(VolError::InvalidArgument));
    assert_eq!(p.schedule_rollback(v, v).err(), Some(VolError::InvalidArgument));
    assert_eq!(p.schedule_rollback(s, s).err(), Some(VolError::InvalidArgument));
    assert_eq!(p.schedule_rollback(v, s), Ok(None));
    assert_eq!(p.schedule_rollback(v, s), Ok(Some(s)));
    assert!(p.info(s).unwrap().rollback_pending);
    assert_eq!(p.delete(s).err(), Some(VolError::Busy));
    // Writes go on until the switch: they are in the state kept before it.
    p.write(v, 9, &block(3, 512), false, Normal).unwrap();
    p.commit().unwrap();
    drop(p);

    let h = PoolHandle::new(Pool::open(d.clone(), cfg(4096)).unwrap(), "t");
    assert_eq!(h.lock().unwrap().pending_rollback(v), Some(s));
    // A snapshot's open carries out nothing.
    drop(h.open_volume(s).unwrap());
    assert_eq!(h.lock().unwrap().pending_rollback(v), Some(s));
    let dv = h.open_volume(v).unwrap();
    // A second open while the first is live: nothing to do.
    drop(h.open_volume(v).unwrap());
    drop(dv);
    let kept = {
        let mut p = h.lock().unwrap();
        assert_eq!(p.pending_rollback(v), None);
        assert!(!p.info(s).unwrap().rollback_pending);
        assert_eq!((read_stamp(&mut p, v, 0), read_stamp(&mut p, v, 9)), (1, 0));
        let k = p.list().unwrap().into_iter().find(|i| i.label == crate::BEFORE_ROLLBACK).unwrap();
        assert!(k.snapshot && !k.auto && k.origin == v);
        assert_eq!((read_stamp(&mut p, k.id, 0), read_stamp(&mut p, k.id, 9)), (2, 3));
        // The undo, then a change of mind.
        assert_eq!(p.schedule_rollback(v, k.id), Ok(None));
        assert_eq!(p.cancel_rollback(v), Ok(Some(k.id)));
        assert_eq!(p.cancel_rollback(v), Ok(None));
        assert_eq!(p.schedule_rollback(v, k.id), Ok(None));
        k.id
    };
    drop(h);
    let h = PoolHandle::new(Pool::open(d.clone(), cfg(4096)).unwrap(), "t");
    drop(h.open_volume(v).unwrap());
    let mut p = h.lock().unwrap();
    assert_eq!((read_stamp(&mut p, v, 0), read_stamp(&mut p, v, 9)), (2, 3));
    assert_eq!(p.pending_rollback(v), None);
    p.delete(s).unwrap();
    p.delete(kept).unwrap();
    assert_eq!(p.list().unwrap().iter().filter(|i| i.label == crate::BEFORE_ROLLBACK).count(), 1);
}

/// A read-only pool device opens the volume unchanged and keeps the schedule.
#[test]
fn scheduled_rollback_waits_on_a_read_only_device() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("data", 16 * 4096, 1).unwrap();
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    let s = p.snapshot(v, "s", 2, false).unwrap();
    p.write(v, 0, &block(2, 512), false, Normal).unwrap();
    p.schedule_rollback(v, s).unwrap();
    drop(p);
    let ro = Arc::new(MemDev::from_image("ro", 512, 8, d.image()).unwrap().read_only());
    let h = PoolHandle::new(Pool::open(ro, cfg(4096)).unwrap(), "t");
    drop(h.open_volume(v).unwrap());
    assert_eq!(h.rollback_failed(), None, "a read-only device does not try");
    let mut p = h.lock().unwrap();
    assert_eq!(p.pending_rollback(v), Some(s));
    assert_eq!(read_stamp(&mut p, v, 0), 2);
}

/// A rollback whose commit fails at the open (an I/O error) is switched in memory, which ext4 then reads, and
/// becomes durable with the next commit that succeeds, as any change of the pool; nothing is named as failed.
#[test]
fn a_scheduled_rollback_whose_commit_fails_is_durable_later() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("data", 16 * 4096, 1).unwrap();
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    let s = p.snapshot(v, "s", 2, false).unwrap();
    p.write(v, 0, &block(2, 512), false, Normal).unwrap();
    p.schedule_rollback(v, s).unwrap();
    drop(p);
    let h = PoolHandle::new(Pool::open(d.clone(), cfg(4096)).unwrap(), "t");
    d.fail_write_after(Some(0));
    let dv = h.open_volume(v);
    d.fail_write_after(None);
    assert!(dv.is_ok(), "the volume opens");
    assert_eq!(h.rollback_failed(), None);
    {
        let mut p = h.lock().unwrap();
        assert_eq!((p.pending_rollback(v), read_stamp(&mut p, v, 0)), (None, 1), "switched in memory");
        p.commit().unwrap();
    }
    drop(dv);
    drop(h);
    let h = PoolHandle::new(Pool::open(d.clone(), cfg(4096)).unwrap(), "t");
    drop(h.open_volume(v).unwrap());
    let mut p = h.lock().unwrap();
    assert_eq!((p.pending_rollback(v), read_stamp(&mut p, v, 0)), (None, 1), "durable, not carried out twice");
    assert_eq!(p.list().unwrap().iter().filter(|i| i.label == crate::BEFORE_ROLLBACK).count(), 1);
}

#[test]
fn many_snapshots_overflow_small_refcounts() {
    let d = dev(512, 1200, 4096, 8);
    let mut p = Pool::format(d.clone(), Config { commit_dirty_pages: 4096, ..cfg(4096) }).unwrap();
    let v = p.create_volume("a", 8 * 4096, 1).unwrap();
    for c in 0..8u64 {
        p.write(v, c * 8, &block(c + 1, 512), false, Normal).unwrap();
    }
    // Each snapshot followed by a write copies the shared leaf: chunks 1..7 end up in 301 leaves.
    let mut snaps = Vec::new();
    for i in 0..300u64 {
        snaps.push(p.snapshot(v, "s", i, false).unwrap());
        p.write(v, 0, &block(1000 + i, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    let digest = p.alloc_digest();
    drop(p);
    let mut p = Pool::open(d.clone(), cfg(4096)).unwrap();
    assert_eq!(p.alloc_digest(), digest);
    assert_eq!(read_stamp(&mut p, snaps[123], 0), 1122);
    assert_eq!(read_stamp(&mut p, snaps[299], 40), 6);
    let free0 = p.stats().free;
    for s in snaps {
        p.delete(s).unwrap();
    }
    p.commit().unwrap();
    assert!(p.stats().free > free0 + 290);
    let digest = p.alloc_digest();
    drop(p);
    let mut p = Pool::open(d, cfg(4096)).unwrap();
    assert_eq!(p.alloc_digest(), digest);
    assert_eq!(read_stamp(&mut p, v, 40), 6);
    assert_eq!(p.exclusive_chunks(v).unwrap(), 8);
}

#[test]
fn exclusive_chunks_counts_what_a_delete_frees() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    for c in 0..10u64 {
        p.write(v, c * 8, &block(c + 1, 512), false, Normal).unwrap();
    }
    let s = p.snapshot(v, "s", 2, false).unwrap();
    assert_eq!(p.exclusive_chunks(s).unwrap(), 0);
    for c in 0..3u64 {
        p.write(v, c * 8, &block(c + 50, 512), false, Normal).unwrap();
    }
    assert_eq!(p.exclusive_chunks(s).unwrap(), 3);
    assert_eq!(p.exclusive_chunks(v).unwrap(), 3);
    p.commit().unwrap();
    let free = p.stats().free;
    p.delete(s).unwrap();
    p.commit().unwrap();
    assert!(p.stats().free >= free + 3);
}

#[test]
fn exclusive_chunks_in_steps_add_up() {
    let d = dev(512, 1600, 4096, 8);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    // More chunks than one leaf holds: a tree of two levels.
    let n = 3 * LEAF_FANOUT;
    let v = p.create_volume("a", n * 4096, 1).unwrap();
    for c in 0..n {
        p.write(v, c * 8, &block(c + 1, 512), false, Normal).unwrap();
    }
    let s = p.snapshot(v, "s", 2, false).unwrap();
    for c in (0..n).step_by(3) {
        p.write(v, c * 8, &block(c + 9000, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    for id in [s, v] {
        let whole = p.exclusive_chunks(id).unwrap();
        assert_eq!(whole, n.div_ceil(3), "volume {id}");
        for budget in [0, 1, 2, 3, 7, 1000] {
            let (mut sum, mut from, mut steps) = (0, 0, 0);
            loop {
                let (k, next) = p.exclusive_chunks_step(id, from, budget).unwrap();
                sum += k;
                steps += 1;
                match next {
                    Some(f) => {
                        assert!(f > from || steps == 1, "no progress at {f}");
                        from = f;
                    }
                    None => break,
                }
                assert!(steps < 10_000);
            }
            assert_eq!(sum, whole, "volume {id}, budget {budget}, {steps} steps");
            assert!(budget > 3 || steps > 1, "budget {budget} took one step");
        }
    }
    let fresh = p.snapshot(v, "fresh", 3, false).unwrap();
    assert_eq!(p.exclusive_chunks_step(fresh, 0, 1).unwrap(), (0, None), "all shared: no page read");
}

#[test]
fn device_errors_leave_the_pool_usable() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    for n in 0..6u64 {
        p.write(v, n * 8 + 1, &block(n + 1, 512), false, Normal).unwrap();
        d.fail_write_after(Some(n % 3));
        assert!(p.commit().is_err());
        d.fail_write_after(None);
        p.commit().unwrap();
    }
    let digest = p.alloc_digest();
    drop(p);
    let mut p = Pool::open(d, cfg(4096)).unwrap();
    assert_eq!(p.alloc_digest(), digest);
    assert_eq!(read_stamp(&mut p, v, 41), 6);
}

#[test]
fn requests_split_to_the_device_limit() {
    let d = dev(512, 32, 65536, 1);
    let mut p = Pool::format(d.clone(), Config { chunk_size: 65536, ..cfg(65536) }).unwrap();
    let v = p.create_volume("a", 8 * 65536, 1).unwrap();
    let big: Vec<u8> = (0..300u64).flat_map(|i| block(i + 1, 512)).collect();
    p.write(v, 77, &big, true, Normal).unwrap();
    drop(p);
    let mut p = Pool::open(d, cfg(65536)).unwrap();
    let mut back = vec![0u8; big.len()];
    p.read(v, 77, &mut back, Normal).unwrap();
    assert!(back == big);
}

#[test]
fn stacked_device() {
    let d = dev(512, 64, 4096, 8);
    let pool = Pool::format(d.clone(), cfg(4096)).unwrap();
    let h = PoolHandle::new(pool, "data");
    let id = h.lock().unwrap().create_volume("data", 32 * 4096, 1).unwrap();
    let vd = h.open_volume(id).unwrap();
    let info = vd.info();
    assert_eq!((info.block_size, info.blocks, info.read_only, info.has_volatile_cache), (512, 256, false, true));
    let data: Vec<u8> = (0..20u64).flat_map(|i| block(i + 1, 512)).collect();
    vd.write_blocks(5, &data, false, Normal).unwrap();
    vd.flush().unwrap();
    let s = vd.snapshot("before").unwrap();
    vd.write_blocks(5, &block(77, 512), true, Normal).unwrap();
    assert_eq!(vd.list().iter().map(|x| (x.0, x.2.as_str())).collect::<Vec<_>>(), vec![(s, "before")]);
    assert_eq!(h.lock().unwrap().delete(id).err(), Some(VolError::Busy));
    let sd = h.open_volume(s).unwrap();
    assert!(sd.info().read_only);
    let mut b = vec![0u8; 512];
    sd.read_blocks(5, &mut b, Normal).unwrap();
    assert_eq!(stamp_of(&b), Some(1));
    vd.read_blocks(5, &mut b, Normal).unwrap();
    assert_eq!(stamp_of(&b), Some(77));
    assert!(sd.write_blocks(5, &b, false, Normal).is_err());
    assert!(vd.write_blocks(256, &b, false, Normal).is_err());
    vd.discard(0, 256).unwrap();
    vd.read_blocks(5, &mut b, Normal).unwrap();
    assert_eq!(stamp_of(&b), Some(0));
    drop(vd);
    drop(sd);
    h.lock().unwrap().delete(id).unwrap();
}

#[test]
fn shared_checks() {
    for (name, f) in [
        ("enospc", crate::checks::enospc_then_discard as fn() -> ktest::TestResult),
        ("rollback", crate::checks::rollback_and_undo),
        ("superblock", crate::checks::damaged_superblock),
    ] {
        if let Err(m) = f() {
            panic!("{name}: {m}");
        }
    }
}

#[test]
fn lock_wait_is_bounded_and_close_never_waits() {
    let d = dev(512, 64, 4096, 8);
    let h = PoolHandle::with_wait(Pool::format(d, cfg(4096)).unwrap(), "data", 50_000_000);
    let id = h.lock().unwrap().create_volume("data", 8 * 4096, 1).unwrap();
    let vd = h.open_volume(id).unwrap();
    let g = h.lock().unwrap();
    // Another thread waits the bounded time and gets Timeout instead of hanging.
    let h2 = h.clone();
    let t = std::thread::spawn(move || {
        (h2.open_volume(id).is_err(), h2.lock().err())
    });
    let (open_failed, err) = t.join().unwrap();
    assert!(open_failed);
    assert_eq!(err, Some(devmodel::DevError::Timeout));
    // Closing while the lock is held queues the release; the next lock applies it, so delete works.
    drop(vd);
    drop(g);
    h.lock().unwrap().delete(id).unwrap();
}

/// Long sweep, not in the gate: `BITSVOL_SEEDS=200 cargo test -p bitsvol --lib sweep -- --ignored`.
#[test]
#[ignore]
fn sweep() {
    let n: u64 = std::env::var("BITSVOL_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    for seed in 1000..1000 + n {
        let dev_chunks = [44, 64, 96, 160][(seed % 4) as usize];
        let chunk_size = [4096, 8192, 65536][(seed % 3) as usize];
        let dev_chunks = if chunk_size == 65536 { dev_chunks / 3 + 12 } else { dev_chunks };
        let crash = seed % 5 == 0;
        let steps = if crash { 300 } else { 2000 };
        let c = RunCfg { dev_chunks, chunk_size, vol_chunks: 10, crash, max_req: 1 + (seed % 9) as u32, ..RunCfg::small(seed, steps) };
        if let Err(m) = Runner::new(c).and_then(|mut r| r.run()) {
            panic!("{m} ({c:?})");
        }
    }
}

#[test]
fn changed_since_sees_every_write_and_discard() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    assert_eq!(p.latest_snapshot(v), None);
    let s0 = p.snapshot(v, "empty", 2, true).unwrap();
    assert!(!p.changed_since(v, s0).unwrap(), "an empty volume and its snapshot");
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    assert!(p.changed_since(v, s0).unwrap(), "first write");
    let s1 = p.snapshot(v, "one", 3, true).unwrap();
    assert_eq!(p.latest_snapshot(v), Some(s1));
    assert!(!p.changed_since(v, s1).unwrap());
    p.commit().unwrap();
    assert!(!p.changed_since(v, s1).unwrap(), "a commit alone is no change");
    // A write into a chunk mapped after the snapshot goes in place; the first one after a snapshot never does.
    p.write(v, 8, &block(2, 512), false, Normal).unwrap();
    assert!(p.changed_since(v, s1).unwrap(), "write into a new chunk");
    let s2 = p.snapshot(v, "two", 4, false).unwrap();
    p.discard(v, 8, 8).unwrap();
    assert!(p.changed_since(v, s2).unwrap(), "discard");
    let s3 = p.snapshot(v, "three", 5, false).unwrap();
    p.write(v, 0, &block(3, 512), false, Normal).unwrap();
    assert!(p.changed_since(v, s3).unwrap(), "overwrite of a shared chunk");
    // Not a snapshot of this volume.
    let w = p.create_volume("b", 4096, 6).unwrap();
    assert_eq!(p.changed_since(w, s3), Err(VolError::InvalidArgument));
    assert_eq!(p.changed_since(v, v), Err(VolError::InvalidArgument));
    assert_eq!(p.changed_since(v, 999), Err(VolError::NotFound));
    // The newest by creation time, not by id.
    let old = p.snapshot(v, "backdated", 1, false).unwrap();
    assert_eq!(p.latest_snapshot(v), Some(s3));
    p.delete(s3).unwrap();
    p.delete(s2).unwrap();
    assert_eq!(p.latest_snapshot(v), Some(s1));
    let _ = old;
    // Survives a reopen (roots are on-disk page numbers).
    let dev2 = p.device().clone();
    drop(p);
    let p = Pool::open(dev2, cfg(4096)).unwrap();
    assert!(p.changed_since(v, s1).unwrap());
    assert!(!p.changed_since(v, old).unwrap(), "nothing written since the backdated snapshot");
    assert_eq!(p.latest_snapshot(v), Some(s1));
}
