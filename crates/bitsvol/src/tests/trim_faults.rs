//! H1 and L2 (review of b478fe4c): a discard that failed in a way that does not prove it never reached the
//! device keeps its chunks out of the pool until the next open. Before, they were free at once, and a device that
//! still held a timed-out deallocate could carry it out after the chunks went to another volume (block's
//! `a_waiter_returns_by_its_deadline_while_the_device_still_holds_its_frames`): its data read back as zeros. Also
//! the queue's bound and the accounting of every chunk on its way back.

use super::trim::{CHUNKS, PER, Watch, WatchDev, drain, reopens_alike, scattered, scattered_stamp};
use super::{block, cfg, dev, read_stamp};
use crate::{Pool, PoolHandle, VolError};
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::block::IoPriority::Normal;

/// Writes a new volume until the pool is full, lets the device carry out every discard it held, and checks that
/// nothing written lost its data (`v` is `scattered`'s volume). The written chunks.
fn fill_then_land(p: &mut Pool, w: &Watch, v: u64) -> u64 {
    let f = p.create_volume("fill", 2048 * PER * 512, 9).unwrap();
    let mut n = 0;
    loop {
        match p.write(f, n * PER, &block(n + 80000, 512), false, Normal) {
            Ok(()) => n += 1,
            Err(VolError::NoSpace) => break,
            Err(e) => panic!("fill: {e:?}"),
        }
    }
    let _ = p.commit();
    assert!(w.land() > 0, "the device held no discard");
    for i in 0..n {
        assert_eq!(read_stamp(p, f, i * PER), i + 80000, "chunk {i} of the new volume: a late discard landed on it");
    }
    for i in 0..2 * CHUNKS {
        assert_eq!(read_stamp(p, v, i * PER), scattered_stamp(i));
    }
    n
}

#[test]
fn chunks_of_a_discard_that_timed_out_stay_out_of_the_pool() {
    let w = Watch::new(dev(512, 1024, 4096, 8));
    let (p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
    let free0 = p.stats().free;
    let h = PoolHandle::new(p, "trim-h1");
    w.answer(Some(DevError::Timeout));
    h.delete(s).unwrap();
    let mut g = h.lock().unwrap();
    let st = g.stats();
    assert!(st.trim_lost >= CHUNKS, "{st:?}");
    assert!(st.free < free0 + CHUNKS, "the timed-out chunks were given back: {st:?} (free before {free0})");
    assert_eq!((g.trim_queued(), st.pending), (0, 0), "lost chunks count as neither queued nor pending: {st:?}");
    // Those lost are free on the device: the reopen counts them.
    let again = reopens_alike(&g, &w.mem);
    assert_eq!(again.stats().free, st.free + st.trim_lost);
    fill_then_land(&mut g, &w, v);
}

/// The same for a pool without a handle, whose commit discards inline.
#[test]
fn an_inline_discard_that_timed_out_keeps_its_chunks_out_as_well() {
    let w = Watch::new(dev(512, 1024, 4096, 8));
    let (mut p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
    let free0 = p.stats().free;
    w.answer(Some(DevError::Timeout));
    p.delete(s).unwrap();
    let st = p.stats();
    assert!(st.trim_lost >= CHUNKS && st.free < free0 + CHUNKS, "{st:?} (free before {free0})");
    reopens_alike(&p, &w.mem);
    fill_then_land(&mut p, &w, v);
}

/// An I/O error or a device gone may have left the request at the device too; a refusal before it was sent
/// (no discard, a range refused, the write window) did not.
#[test]
fn only_a_refused_discard_gives_its_chunks_back() {
    for (e, back) in [(DevError::Io, false), (DevError::Gone, false), (DevError::Refused, true), (DevError::Unsupported, true), (DevError::InvalidArgument, true)] {
        let w = Watch::new(dev(512, 1024, 4096, 8));
        let (p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
        let free0 = p.stats().free;
        let h = PoolHandle::new(p, "trim-err");
        w.answer(Some(e));
        h.delete(s).unwrap();
        let mut g = h.lock().unwrap();
        let st = g.stats();
        assert_eq!(st.trim_lost == 0, back, "{e:?}: {st:?}");
        assert_eq!(st.free >= free0 + CHUNKS, back, "{e:?}: {st:?} (free before {free0})");
        reopens_alike(&g, &w.mem);
        for i in 0..2 * CHUNKS {
            assert_eq!(read_stamp(&mut g, v, i * PER), scattered_stamp(i));
        }
    }
}

/// L2: a round whose pool lock does not come back after its discards: its chunks are lost (counted by the next
/// holder), not left under way forever.
#[test]
fn a_round_that_lost_the_lock_counts_its_chunks_lost() {
    let w = Watch::new(dev(512, 1024, 4096, 8));
    let (p, _v, s) = scattered(Arc::new(WatchDev(w.clone())));
    // Lock waits of 1 ms: the volume's I/O gives up after ten of them.
    let h = PoolHandle::with_wait(p, "trim-l2", 1_000_000);
    let h2 = h.clone();
    w.hook(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let h3 = h2.clone();
        std::thread::spawn(move || {
            let g = h3.lock().unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(g);
        });
        rx.recv().unwrap();
        false
    });
    h.delete(s).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let g = h.lock().unwrap();
    let st = g.stats();
    assert_eq!((g.trimming, st.trim_lost), (0, g.round_chunks()), "{st:?}");
    assert!(g.trim_queued() >= CHUNKS - g.round_chunks(), "the rest still waits: {}", g.trim_queued());
    reopens_alike(&g, &w.mem);
}

/// L3: beyond the queue's bound a commit's freed chunks are free at once, undiscarded; every chunk is counted.
#[test]
fn beyond_the_queues_bound_freed_chunks_are_free_at_once() {
    let w = Watch::new(dev(512, 1024, 4096, 8));
    let (mut p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
    p.set_trim_later();
    p.set_trim_max(50);
    let (free0, n0) = (p.stats().free, w.count());
    p.delete(s).unwrap();
    assert_eq!(w.count(), n0, "a queued pool discards nothing in its commit");
    assert_eq!(p.trim_queued(), 50);
    assert!(p.stats().free >= free0 + CHUNKS - 50, "{:?} (free before {free0})", p.stats());
    reopens_alike(&p, &w.mem);
    let h = PoolHandle::new(p, "trim-max");
    let c0 = w.chunks();
    drain(&h);
    let mut g = h.lock().unwrap();
    assert_eq!(w.chunks() - c0, 50, "the queued chunks went out");
    assert!(g.stats().free >= free0 + CHUNKS);
    for i in 0..2 * CHUNKS {
        assert_eq!(read_stamp(&mut g, v, i * PER), scattered_stamp(i));
    }
    reopens_alike(&g, &w.mem);
}

/// L3: an allocation steps over a run of held chunks in one walk, and lands right behind it.
#[test]
fn an_allocation_steps_over_a_run_of_held_chunks() {
    let mut p = Pool::format(dev(512, 256, 4096, 8), cfg(4096)).unwrap();
    p.pend_set.extend((40..60).chain([61, 63]));
    assert_eq!(p.past_held(40), 60);
    assert_eq!(p.past_held(50), 60);
    assert_eq!(p.past_held(60), 60);
    assert_eq!(p.past_held(61), 62);
    assert_eq!(p.past_held(10), 10);
    p.pend_set.clear();
    // Held chunks right where the next allocation would look: it takes the first one after them.
    let v = p.create_volume("a", 8 * PER * 512, 1).unwrap();
    p.write(v, 0, &block(1, 512), false, Normal).unwrap();
    let probe = p.alloc_chunk(0).unwrap();
    p.unalloc_chunk(probe).unwrap();
    // The cursor stands right behind `probe`.
    let held: Vec<u64> = (probe + 1..probe + 31).collect();
    p.pend_set.extend(held.iter().copied());
    p.free -= 30;
    let c = p.alloc_chunk(0).unwrap();
    assert_eq!(c, probe + 31);
    p.unalloc_chunk(c).unwrap();
    for x in held {
        p.pend_set.remove(&x);
    }
    p.free += 30;
}
