//! VS-2b: barriers per commit, the copy window (every power cut a prefix of the writes) and the handle's deferred
//! flushes.

use super::{block, cfg, dev, read_stamp};
use crate::memdev::MemDev;
use crate::{Pool, PoolHandle};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::block::{BlockDevice, IoPriority::Normal};

const BLOCKS: [u64; 4] = [0, 1, 2, 8];

fn state(p: &mut Pool, id: u64, at: &[u64]) -> Vec<u64> {
    at.iter().map(|&b| read_stamp(p, id, b)).collect()
}

#[test]
fn a_commit_costs_one_flush_and_an_idle_one_none() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    let (f0, b0) = (d.flushes(), p.barriers().0);
    p.commit().unwrap();
    // Pages, one flush, the superblock with FUA (one barrier more on a device that emulates FUA).
    assert_eq!((d.flushes() - f0, p.barriers().0 - b0), (1, 2));
    p.commit().unwrap();
    assert_eq!(d.flushes() - f0, 1, "nothing written since: no flush");
    // In place into a committed chunk (no metadata change): the flush alone.
    p.write(v, 1, &block(2, 512), false, Normal).unwrap();
    p.commit().unwrap();
    assert_eq!(d.flushes() - f0, 2);
    p.commit().unwrap();
    assert_eq!(d.flushes() - f0, 2);
    // Durable without the flush after the superblock: a power cut now keeps both writes.
    let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, d.durable()).unwrap()), cfg(4096)).unwrap();
    assert_eq!(q.generation(), p.generation());
    assert_eq!(state(&mut q, v, &[0, 1]), [1, 2]);
}

/// Writes (block, stamp) into `v`; with `mid` a commit after the third one.
fn window_run(window: bool) -> (Vec<Vec<u8>>, u64, u64, u64, Vec<u8>) {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    // Blocks 0 and 8 in chunks 0 and 1, committed and shared with nothing: normally written in place.
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    p.write(v, 8, &block(2, 512), false, Normal).unwrap();
    p.commit().unwrap();
    let g0 = p.generation();
    if window {
        p.begin_copy_window(v).unwrap();
    }
    d.set_capture(true, 11);
    p.write(v, 1, &block(11, 512), false, Normal).unwrap();
    p.write(v, 8, &block(12, 512), false, Normal).unwrap();
    p.write(v, 1, &block(13, 512), false, Normal).unwrap();
    p.commit().unwrap();
    // Chunk 0 is committed again now: the window copies it once more.
    p.write(v, 2, &block(14, 512), false, Normal).unwrap();
    p.write(v, 0, &block(15, 512), false, Normal).unwrap();
    let s = p.snapshot_uncommitted(v, "w", 9, true).unwrap();
    p.end_copy_window();
    p.commit().unwrap();
    d.set_capture(false, 0);
    assert_eq!(state(&mut p, s, &BLOCKS), [15, 13, 14, 12]);
    (d.take_images(), g0, v, s, d.durable())
}

#[test]
fn a_power_cut_in_a_copy_window_leaves_a_commit_exactly() {
    let (mut images, g0, v, s, end) = window_run(true);
    // A cut at each write lands before it; the cut after the last one is the durable image.
    images.push(end);
    let want = [(g0, [1, 0, 0, 2]), (g0 + 1, [1, 13, 0, 12]), (g0 + 2, [15, 13, 14, 12])];
    let mut seen = alloc::collections::BTreeSet::new();
    for img in images {
        let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, img).unwrap()), cfg(4096)).unwrap();
        let g = q.generation();
        let (_, w) = want.iter().find(|x| x.0 == g).unwrap_or_else(|| panic!("generation {g}"));
        assert_eq!(state(&mut q, v, &BLOCKS), w, "generation {g}: not the state of that commit");
        assert_eq!(q.info(s).is_ok(), g == g0 + 2, "the snapshot exists with its commit only");
        seen.insert(g);
    }
    assert_eq!(seen.len(), 3, "{seen:?}");
}

#[test]
fn without_the_window_a_cut_mixes_states() {
    // The control: the same writes in place show a state no commit had (what the window rules out).
    let (images, g0, v, _, _) = window_run(false);
    let want = [[1, 0, 0, 2], [1, 13, 0, 12], [15, 13, 14, 12]];
    let mixed = images.into_iter().any(|img| {
        let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, img).unwrap()), cfg(4096)).unwrap();
        let _ = g0;
        !want.contains(&state(&mut q, v, &BLOCKS).try_into().unwrap())
    });
    assert!(mixed);
}

#[test]
fn a_window_defers_only_the_flushes_it_is_told_to() {
    let d = dev(512, 64, 4096, 8);
    let h = PoolHandle::new(Pool::format(d.clone(), cfg(4096)).unwrap(), "data");
    let id = h.lock().unwrap().create_volume("data", 16 * 4096, 1).unwrap();
    let vd = h.open_volume(id).unwrap();
    vd.write_blocks(0, &block(1, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    let g = h.lock().unwrap().generation();
    let yes = Arc::new(AtomicBool::new(true));
    let y2 = yes.clone();
    let w = h.window(id, Arc::new(move || y2.load(Ordering::Relaxed))).unwrap();
    assert_eq!(h.lock().unwrap().window, Some(id));
    vd.write_blocks(0, &block(2, 512), false, Normal).unwrap();
    let f = d.flushes();
    vd.flush().unwrap();
    assert_eq!((h.lock().unwrap().generation(), d.flushes(), h.deferred_flushes()), (g, f, 1), "deferred");
    // The cut now is the commit before the window, not a half-written chunk.
    let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, d.durable()).unwrap()), cfg(4096)).unwrap();
    assert_eq!(state(&mut q, id, &[0]), [1]);
    yes.store(false, Ordering::Relaxed);
    vd.flush().unwrap();
    assert_eq!(h.lock().unwrap().generation(), g + 1, "a flush the window is not told to defer commits");
    yes.store(true, Ordering::Relaxed);
    drop(w);
    vd.write_blocks(0, &block(3, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    let p = h.lock().unwrap();
    assert_eq!((p.generation(), p.window, h.deferred_flushes()), (g + 2, None, 1), "after the window everything is real");
}

#[test]
fn a_window_never_defers_another_volumes_flush() {
    let d = dev(512, 64, 4096, 8);
    let h = PoolHandle::new(Pool::format(d.clone(), cfg(4096)).unwrap(), "data");
    let a = h.lock().unwrap().create_volume("data", 16 * 4096, 1).unwrap();
    let b = h.lock().unwrap().create_volume("other", 16 * 4096, 1).unwrap();
    let vb = h.open_volume(b).unwrap();
    let w = h.window(a, Arc::new(|| true)).unwrap();
    let g = h.lock().unwrap().generation();
    vb.write_blocks(0, &block(7, 512), false, Normal).unwrap();
    vb.flush().unwrap();
    assert_eq!((h.lock().unwrap().generation(), h.deferred_flushes()), (g + 1, 0), "the other volume's flush commits");
    drop(w);
}

/// `snapshots::take`'s order: window, the freeze's write and deferred flush, the snapshot in memory, a write after
/// the thaw, the commit, the window's end. Every cut is the commit before or the one after; the snapshot holds the
/// frozen state and the volume the later write.
#[test]
fn a_take_with_writes_after_the_thaw_commits_once() {
    let d = dev(512, 64, 4096, 8);
    let h = PoolHandle::new(Pool::format(d.clone(), cfg(4096)).unwrap(), "data");
    let v = h.lock().unwrap().create_volume("data", 16 * 4096, 1).unwrap();
    let vd = h.open_volume(v).unwrap();
    vd.write_blocks(0, &block(1, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    let g0 = h.lock().unwrap().generation();
    let frozen = Arc::new(AtomicBool::new(true));
    let f2 = frozen.clone();
    let w = h.window(v, Arc::new(move || f2.load(Ordering::Relaxed))).unwrap();
    d.set_capture(true, 21);
    vd.write_blocks(0, &block(2, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    let s = h.lock().unwrap().snapshot_uncommitted(v, "t", 1, true).unwrap();
    frozen.store(false, Ordering::Relaxed);
    vd.write_blocks(0, &block(3, 512), false, Normal).unwrap();
    h.lock().unwrap().commit().unwrap();
    drop(w);
    d.set_capture(false, 0);
    assert_eq!(h.deferred_flushes(), 1);
    let mut images = d.take_images();
    images.push(d.durable());
    let mut last = 0;
    for img in images {
        let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, img).unwrap()), cfg(4096)).unwrap();
        let g = q.generation();
        if g == g0 {
            assert_eq!(state(&mut q, v, &[0]), [1]);
            assert!(q.info(s).is_err());
        } else {
            assert_eq!(g, g0 + 1);
            assert_eq!((state(&mut q, v, &[0]), state(&mut q, s, &[0])), (alloc::vec![3], alloc::vec![2]));
        }
        last = g;
    }
    assert_eq!(last, g0 + 1);
    assert!(h.lock().unwrap().copy_window().is_none());
}

/// VS-2b fixer: a deferred flush is owed until a commit covers it; `settle` (fsync's answer) returns at once
/// after the take's commit, and commits itself when that commit failed or never came.
#[test]
fn settle_waits_for_a_commit_that_covers_the_deferred_flushes() {
    let d = dev(512, 64, 4096, 8);
    let h = PoolHandle::with_wait(Pool::format(d.clone(), cfg(4096)).unwrap(), "data", 1_000_000);
    let id = h.lock().unwrap().create_volume("data", 16 * 4096, 1).unwrap();
    let vd = h.open_volume(id).unwrap();
    h.settle().unwrap();
    // The take's order: deferred flush, commit, window end: settle needs no commit of its own.
    let w = h.window(id, Arc::new(|| true)).unwrap();
    vd.write_blocks(0, &block(1, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    assert_eq!(h.owed_flushes(), 1);
    h.commit().unwrap();
    drop(w);
    let g = h.lock().unwrap().generation();
    h.settle().unwrap();
    assert_eq!((h.owed_flushes(), h.lock().unwrap().generation()), (0, g));
    // The take's commit fails (one write error): the flush stays owed; settle commits and the data is durable.
    let w = h.window(id, Arc::new(|| true)).unwrap();
    vd.write_blocks(0, &block(2, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    d.fail_write_after(Some(0));
    assert!(h.commit().is_err());
    drop(w);
    assert_eq!(h.owed_flushes(), 1, "a failed commit covers nothing");
    h.settle().unwrap();
    assert_eq!(h.owed_flushes(), 0);
    let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, d.durable()).unwrap()), cfg(4096)).unwrap();
    assert_eq!(state(&mut q, id, &[0]), [2], "durable once settle returned");
    // Its own commit fails too: settle reports the error, and the flush stays owed for the next one.
    let w = h.window(id, Arc::new(|| true)).unwrap();
    vd.write_blocks(0, &block(3, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    drop(w);
    d.fail_write_after(Some(0));
    assert!(h.settle().is_err(), "the fsync told \"done\" too early hears of the error");
    assert_eq!(h.owed_flushes(), 1);
    // A window left open past the lock bound: settle stops waiting and commits.
    let _w = h.window(id, Arc::new(|| true)).unwrap();
    h.settle().unwrap();
    assert_eq!(h.owed_flushes(), 0);
    // A real flush of the volume covers what was owed, too.
    vd.write_blocks(0, &block(4, 512), false, Normal).unwrap();
    vd.flush().unwrap();
    assert_eq!(h.owed_flushes(), 1);
    drop(_w);
    vd.flush().unwrap();
    assert_eq!(h.owed_flushes(), 0);
}
