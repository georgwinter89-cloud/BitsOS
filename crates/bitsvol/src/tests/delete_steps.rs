//! VS-H: a delete in steps, the pool's other work between its steps, and how long the volume's I/O waits for the
//! pool lock. Counts and orders, no clock is judged.

use super::{block, cfg, dev, read_stamp};
use crate::layout::LEAF_FANOUT;
use crate::memdev::MemDev;
use crate::{Pool, PoolHandle, VolError};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, IoPriority::Normal};

const LEAVES: u64 = 200;
const BS: u64 = 512;
const PER_CHUNK: u64 = 8;

/// A volume with one chunk under each of `LEAVES` leaves, a snapshot `s` of it, then all of them rewritten, so the
/// snapshot alone holds a tree of about that many pages. One more snapshot `shared` holds half of its tree too.
fn with_tree(cache_pages: usize) -> (Arc<MemDev>, Pool, u64, u64, u64) {
    let d = dev(512, 2048, 4096, 8);
    let mut p = Pool::format(d.clone(), crate::Config { cache_pages, ..cfg(4096) }).unwrap();
    let lba = |i: u64| i * LEAF_FANOUT * PER_CHUNK;
    let v = p.create_volume("a", (LEAVES + 1) * LEAF_FANOUT * PER_CHUNK * BS, 1).unwrap();
    for i in 0..LEAVES {
        p.write(v, lba(i), &block(i + 1, BS as usize), false, Normal).unwrap();
    }
    let s = p.snapshot(v, "s", 1, false).unwrap();
    for i in 0..LEAVES / 2 {
        p.write(v, lba(i), &block(i + 1000, BS as usize), false, Normal).unwrap();
    }
    let shared = p.snapshot(v, "shared", 2, false).unwrap();
    for i in 0..LEAVES {
        p.write(v, lba(i), &block(i + 2000, BS as usize), false, Normal).unwrap();
    }
    p.commit().unwrap();
    (d, p, v, s, shared)
}

fn check_rest(p: &mut Pool, v: u64, shared: u64) {
    let lba = |i: u64| i * LEAF_FANOUT * PER_CHUNK;
    for i in 0..LEAVES {
        assert_eq!(read_stamp(p, v, lba(i)), i + 2000);
        assert_eq!(read_stamp(p, shared, lba(i)), if i < LEAVES / 2 { i + 1000 } else { i + 1 });
    }
}

#[test]
fn a_big_delete_goes_in_as_many_holds_as_its_tree_needs() {
    let (_d, p, v, s, shared) = with_tree(4096);
    let h = PoolHandle::new(p, "vsh-holds");
    let before = h.behind_counts().0;
    h.delete(s).unwrap();
    let holds = h.behind_counts().0 - before;
    // Its own 100 leaves at 64 pages a step: two steps and the commit. In one hold it would read them all.
    assert!(holds >= 3, "{holds} holds");
    let mut g = h.lock().unwrap();
    assert!(g.info(s).is_err());
    check_rest(&mut g, v, shared);
}

#[test]
fn one_step_opens_at_most_its_pages() {
    let (_d, mut p, v, s, shared) = with_tree(4096);
    let mut d = p.delete_begin(s).unwrap();
    let mut steps = 0;
    while !p.delete_step(&mut d, 1) {
        steps += 1;
    }
    // Half of the snapshot's leaves are its own (the rest count down in the pages `shared` also holds).
    assert!(steps >= LEAVES / 2, "{steps} steps");
    p.commit().unwrap();
    check_rest(&mut p, v, shared);
}

#[test]
fn a_write_and_a_commit_between_two_steps_keep_every_power_cut_clean() {
    let (mem, mut p, v, s, shared) = with_tree(4096);
    let (start, before) = (p.stats(), p.alloc_digest());
    let mut d = p.delete_begin(s).unwrap();
    // A cut before any commit still has the snapshot.
    let again = Pool::open(Arc::new(MemDev::from_image("cut0", 512, 8, mem.durable()).unwrap()), cfg(4096)).unwrap();
    assert!(again.info(s).is_ok());
    assert_eq!(again.alloc_digest(), before);
    drop(again);

    assert!(!p.delete_step(&mut d, 10));
    // Foreground work: a new chunk and a rewrite under the deleted tree, a new snapshot, and a commit.
    let last = LEAVES * LEAF_FANOUT * PER_CHUNK;
    p.write(v, last, &block(7777, BS as usize), false, Normal).unwrap();
    p.write(v, 0, &block(2000, BS as usize), false, Normal).unwrap();
    let mid = p.snapshot(v, "mid", 3, false).unwrap();
    p.commit().unwrap();
    // A cut now: the deleted snapshot is gone, the rest of its tree is free space of that open.
    let mut cut = Pool::open(Arc::new(MemDev::from_image("cut1", 512, 8, mem.durable()).unwrap()), cfg(4096)).unwrap();
    assert!(cut.info(s).is_err());
    check_rest(&mut cut, v, shared);
    assert_eq!(read_stamp(&mut cut, mid, last), 7777);
    let free_after_cut = cut.stats().free;
    assert!(free_after_cut > start.free, "the cut's open finds the tree's chunks free");
    drop(cut);

    while !p.delete_step(&mut d, 10) {}
    p.commit().unwrap();
    check_rest(&mut p, v, shared);
    assert_eq!(p.stats().leaks, 0);
    // Finished, the live pool and a fresh open agree on every count.
    let digest = p.alloc_digest();
    let reopened = Pool::open(Arc::new(MemDev::from_image("end", 512, 8, mem.durable()).unwrap()), cfg(4096)).unwrap();
    assert_eq!(reopened.alloc_digest(), digest);
    assert_eq!(reopened.stats().free, p.stats().free);
}

#[test]
fn a_refused_delete_changes_nothing() {
    let (_d, p, v, s, shared) = with_tree(4096);
    let h = PoolHandle::new(p, "vsh-refused");
    let open = h.open_volume(s).unwrap();
    let digest = h.lock().unwrap().alloc_digest();
    assert!(matches!(h.delete(s), Err(VolError::Busy)));
    assert!(matches!(h.delete(9999), Err(VolError::NotFound)));
    assert_eq!(h.lock().unwrap().alloc_digest(), digest);
    drop(open);
    // The close is applied by `lock_behind` too.
    h.delete(s).unwrap();
    check_rest(&mut h.lock().unwrap(), v, shared);
}

#[test]
fn the_io_of_a_volume_outlasts_one_round_of_the_lock_bound() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    let v = p.create_volume("a", 8 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::with_wait(p, "vsh-rounds", 20_000_000);
    let vd = h.open_volume(v).unwrap();
    let g = h.lock().unwrap();
    let started = Arc::new(AtomicU64::new(0));
    let (s2, h2) = (started.clone(), h.clone());
    let w = std::thread::spawn(move || {
        s2.store(1, Ordering::Release);
        // A bare lock gives up after one round, the volume's write goes on through the next ones.
        let bare = h2.lock().err();
        s2.store(2, Ordering::Release);
        (bare, vd.write_blocks(0, &block(5, 512), false, Normal))
    });
    while started.load(Ordering::Acquire) != 2 {
        core::hint::spin_loop();
    }
    drop(g);
    let (bare, written) = w.join().unwrap();
    assert_eq!(bare, Some(DevError::Timeout));
    assert_eq!(written, Ok(()));
}

#[test]
fn a_pool_that_stands_still_ends_in_a_timeout() {
    let d = dev(512, 64, 4096, 8);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    let v = p.create_volume("a", 8 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::with_wait(p, "vsh-stuck", 1_000_000);
    let vd = h.open_volume(v).unwrap();
    let g = h.lock().unwrap();
    let w = std::thread::spawn(move || vd.write_blocks(0, &block(5, 512), false, Normal));
    assert_eq!(w.join().unwrap(), Err(DevError::Timeout));
    drop(g);
}

#[test]
fn background_work_lets_a_foreground_waiter_go_first() {
    let d = dev(512, 64, 4096, 8);
    let p = Pool::format(d, cfg(4096)).unwrap();
    let h = PoolHandle::new(p, "vsh-first");
    let order = Arc::new(AtomicU64::new(0));
    let g = h.lock().unwrap();
    let (h1, o1) = (h.clone(), order.clone());
    let fg = std::thread::spawn(move || {
        let k = h1.lock().unwrap();
        let at = o1.fetch_add(1, Ordering::SeqCst);
        drop(k);
        at
    });
    while h.ahead() == 0 {
        core::hint::spin_loop();
    }
    let (h2, o2) = (h.clone(), order.clone());
    let bg = std::thread::spawn(move || {
        let k = h2.lock_behind().unwrap();
        let at = o2.fetch_add(1, Ordering::SeqCst);
        drop(k);
        at
    });
    while h.behind_counts().1 == 0 {
        core::hint::spin_loop();
    }
    drop(g);
    assert_eq!(fg.join().unwrap(), 0, "the foreground waiter first");
    assert_eq!(bg.join().unwrap(), 1);
}
