//! Freed chunks are discarded without the pool lock and at Normal priority (trim.rs). Before, the commit after a
//! snapshot's delete sent one Background discard per run of the chunks it freed with the pool lock held: each one
//! waited out the block queues' aging while readers kept them busy, and every read and write of the pool waited
//! for all of them (plan/spec/storage.md §5.2). Here: the delete's discards, space during a round, the
//! work a commit, a settle and a delete do, and a delete beside writers. Failed discards: trim_faults.rs.

use super::{block, cfg, dev, read_stamp};
use crate::memdev::MemDev;
use crate::{Pool, PoolHandle, VolError};
use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority, IoPriority::Normal};
use std::sync::{Mutex, OnceLock};

pub(super) const CHUNKS: u64 = 300;
pub(super) const PER: u64 = 8;

type Hook = Box<dyn FnMut() -> bool + Send>;

/// A `MemDev` that notes every discard, and those sent while its pool's lock was held; it may answer them with an
/// error (a timeout: the device keeps the discard until `land`), and runs a hook at each one.
pub(super) struct Watch {
    pub mem: Arc<MemDev>,
    pub h: OnceLock<Weak<PoolHandle>>,
    pub discards: AtomicU64,
    pub blocks: AtomicU64,
    pub under_lock: AtomicU64,
    pub background: AtomicU64,
    /// The answer to every discard from now on (None: done).
    pub answer: Mutex<Option<DevError>>,
    /// Discards answered with a timeout that the device still holds.
    pub held: Mutex<Vec<(u64, u64)>>,
    /// Runs at each discard with the hook taken out (a discard inside it does not run it); false: once is enough.
    pub hook: Mutex<Option<Hook>>,
    /// Microseconds each discard takes.
    pub delay_us: AtomicU64,
}

impl Watch {
    pub(super) fn new(mem: Arc<MemDev>) -> Arc<Watch> {
        Arc::new(Watch {
            mem,
            h: OnceLock::new(),
            discards: AtomicU64::new(0),
            blocks: AtomicU64::new(0),
            under_lock: AtomicU64::new(0),
            background: AtomicU64::new(0),
            answer: Mutex::new(None),
            held: Mutex::new(Vec::new()),
            hook: Mutex::new(None),
            delay_us: AtomicU64::new(0),
        })
    }

    pub(super) fn answer(&self, e: Option<DevError>) {
        *self.answer.lock().unwrap() = e;
    }

    pub(super) fn hook(&self, f: impl FnMut() -> bool + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(f));
    }

    /// The device carries out the discards it still held (late, after their waiters gave up).
    pub(super) fn land(&self) -> u64 {
        let held = core::mem::take(&mut *self.held.lock().unwrap());
        for &(lba, n) in &held {
            self.mem.discard(lba, n).unwrap();
        }
        held.len() as u64
    }

    pub(super) fn count(&self) -> u64 {
        self.discards.load(Ordering::Relaxed)
    }

    pub(super) fn chunks(&self) -> u64 {
        self.blocks.load(Ordering::Relaxed) / PER
    }
}

pub(super) struct WatchDev(pub Arc<Watch>);

impl BlockDevice for WatchDev {
    fn name(&self) -> &str {
        "trim-watch"
    }
    fn info(&self) -> BlockInfo {
        self.0.mem.info()
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.0.mem.read_blocks(lba, buf, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.0.mem.write_blocks(lba, buf, fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        self.0.mem.flush()
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        self.discard_at(lba, blocks, IoPriority::Background)
    }
    fn discard_at(&self, lba: u64, blocks: u64, prio: IoPriority) -> Result<(), DevError> {
        if prio == IoPriority::Background {
            self.0.background.fetch_add(1, Ordering::Relaxed);
        }
        if self.0.h.get().and_then(Weak::upgrade).is_some_and(|h| h.pool_locked()) {
            self.0.under_lock.fetch_add(1, Ordering::Relaxed);
        }
        self.0.discards.fetch_add(1, Ordering::Relaxed);
        self.0.blocks.fetch_add(blocks, Ordering::Relaxed);
        let hook = self.0.hook.lock().unwrap().take();
        if let Some(mut f) = hook
            && f()
        {
            self.0.hook.lock().unwrap().get_or_insert(f);
        }
        let us = self.0.delay_us.load(Ordering::Relaxed);
        if us != 0 {
            std::thread::sleep(std::time::Duration::from_micros(us));
        }
        let answer = *self.0.answer.lock().unwrap();
        match answer {
            None => self.0.mem.discard(lba, blocks),
            Some(DevError::Timeout) => {
                self.0.held.lock().unwrap().push((lba, blocks));
                Err(DevError::Timeout)
            }
            Some(e) => Err(e),
        }
    }
}

/// A volume with `CHUNKS` chunks two apart, a snapshot `s`, then every one of them rewritten: `s` alone holds
/// `CHUNKS` chunks, none next to another.
pub(super) fn scattered(d: Arc<dyn BlockDevice>) -> (Pool, u64, u64) {
    let mut p = Pool::format(d, crate::Config { cache_pages: 4096, ..cfg(4096) }).unwrap();
    let v = p.create_volume("a", 2 * CHUNKS * PER * 512, 1).unwrap();
    for i in 0..2 * CHUNKS {
        p.write(v, i * PER, &block(i + 1, 512), false, Normal).unwrap();
    }
    let s = p.snapshot(v, "s", 1, true).unwrap();
    for i in (0..2 * CHUNKS).step_by(2) {
        p.write(v, i * PER, &block(i + 5000, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    (p, v, s)
}

/// What `scattered`'s volume holds at chunk `i`.
pub(super) fn scattered_stamp(i: u64) -> u64 {
    if i.is_multiple_of(2) { i + 5000 } else { i + 1 }
}

/// The pool's allocation state matches what a reopen of its device counts (every chunk waiting for, under way to
/// or kept out after a discard is free on the device).
pub(super) fn reopens_alike(p: &Pool, mem: &Arc<MemDev>) -> Pool {
    let again = Pool::open(mem.clone(), cfg(4096)).unwrap();
    assert_eq!(again.alloc_digest(), p.alloc_digest(), "what the pool reopens as: {}", p.alloc_diff(&again));
    again
}

/// Rounds of owed discards until none is left (bounded).
pub(super) fn drain(h: &PoolHandle) {
    for _ in 0..10_000 {
        if h.lock().unwrap().trim_queued() == 0 {
            return;
        }
        h.trim_owed();
    }
    panic!("discards still owed: {}", h.lock().unwrap().trim_queued());
}

#[test]
fn a_delete_discards_what_it_freed_with_the_pool_lock_released() {
    let w = Watch::new(dev(512, 1024, 4096, 8));
    let (p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
    let free_before = p.stats().free;
    let h = PoolHandle::new(p, "trim");
    w.h.set(Arc::downgrade(&h)).ok().unwrap();
    // The setup's own commits discarded inline (no handle yet).
    let (n0, b0, bg0) = (w.count(), w.blocks.load(Ordering::Relaxed), w.background.load(Ordering::Relaxed));
    h.delete(s).unwrap();
    let (n, b) = (w.count() - n0, w.blocks.load(Ordering::Relaxed) - b0);
    assert!(n >= CHUNKS, "{n} discards for {CHUNKS} chunks apart");
    assert!(b >= CHUNKS * PER, "{b} blocks discarded");
    assert_eq!(w.under_lock.load(Ordering::Relaxed), 0, "discards sent with the pool lock held");
    assert_eq!(w.background.load(Ordering::Relaxed) - bg0, 0, "discards queued as Background below a waiting deleter");
    let mut g = h.lock().unwrap();
    assert_eq!(g.trim_queued(), 0, "the delete left discards owed");
    assert!(g.stats().free >= free_before + CHUNKS, "free {} before {free_before}", g.stats().free);
    for i in 0..2 * CHUNKS {
        assert_eq!(read_stamp(&mut g, v, i * PER), scattered_stamp(i));
    }
    reopens_alike(&g, &w.mem);
}

/// Chunks waiting for their discard are free space: an allocation that finds none free takes them (undiscarded)
/// before an automatic snapshot is dropped for room.
#[test]
fn queued_chunks_serve_an_allocation_before_a_snapshot_goes() {
    let d = dev(512, 1024, 4096, 8);
    let (mut p, v, s) = scattered(d);
    p.set_trim_later();
    let keep = p.snapshot(v, "keep", 2, true).unwrap();
    p.delete(s).unwrap();
    assert!(p.trim_queued() >= CHUNKS, "{} queued", p.trim_queued());
    let st = p.stats();
    assert!(st.pending >= CHUNKS, "the queued chunks count as pending: {st:?}");
    // A second volume as large as the free space plus a quarter of the queue (the rest leaves room for its tree).
    let want = st.free.saturating_sub(st.reserve) + CHUNKS / 4;
    let w2 = p.create_volume("b", want * PER * 512, 3).unwrap();
    for i in 0..want {
        p.write(w2, i * PER, &block(i + 9000, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    assert_eq!(p.stats().dropped, 0, "a snapshot went while freed chunks waited");
    assert!(p.info(keep).is_ok());
    // Only what the last commits freed waits now.
    assert!(p.trim_queued() < CHUNKS / 4, "{} still queued", p.trim_queued());
    assert_eq!(read_stamp(&mut p, w2, (want - 1) * PER), want - 1 + 9000);
}

/// M1 (review of b478fe4c): a writer that needs more space than is free while a delete's discards are under way.
/// Before, the round took all of the delete's 300 contiguous chunks at once and left the queue empty: the
/// writer's room went to the automatic snapshot "keep" (dropped), and the write failed anyway. A round takes at
/// most a quarter of the reserve; the rest waits in the queue, where an allocation takes it at once.
#[test]
fn a_writer_during_a_deletes_discards_drops_no_snapshot() {
    const RESERVE: u64 = 16;
    let w = Watch::new(dev(512, 1024, 4096, 8));
    let pcfg = crate::Config { cache_pages: 4096, reserve_chunks: Some(RESERVE), ..cfg(4096) };
    let mut p = Pool::format(Arc::new(WatchDev(w.clone())), pcfg).unwrap();
    let v = p.create_volume("a", CHUNKS * PER * 512, 1).unwrap();
    for i in 0..CHUNKS {
        p.write(v, i * PER, &block(i + 1, 512), false, Normal).unwrap();
    }
    let s = p.snapshot(v, "s", 1, false).unwrap();
    for i in 0..CHUNKS {
        p.write(v, i * PER, &block(i + 5000, 512), false, Normal).unwrap();
    }
    let keep = p.snapshot(v, "keep", 2, true).unwrap();
    let wv = p.create_volume("w", 64 * PER * 512, 3).unwrap();
    let fill = p.create_volume("f", 1024 * PER * 512, 4).unwrap();
    // Free space down to about 12 chunks above the reserve.
    let mut filled = 0;
    while p.stats().free > RESERVE + 12 {
        p.write(fill, filled * PER, &block(filled + 20000, 512), false, Normal).unwrap();
        filled += 1;
        if p.stats().free <= RESERVE + 12 {
            p.commit().unwrap();
        }
    }
    assert!(p.stats().free >= RESERVE + 4, "{:?}", p.stats());
    let h = PoolHandle::new(p, "trim-m1");
    w.h.set(Arc::downgrade(&h)).ok().unwrap();
    let got: Arc<Mutex<Option<Result<(), VolError>>>> = Arc::new(Mutex::new(None));
    let (h2, got2) = (h.clone(), got.clone());
    w.hook(move || {
        let mut g = h2.lock().unwrap();
        let r = (0..40).try_for_each(|i| g.write(wv, i * PER, &block(i + 70000, 512), false, Normal)).and_then(|()| g.commit());
        *got2.lock().unwrap() = Some(r);
        false
    });
    h.delete(s).unwrap();
    assert_eq!(*got.lock().unwrap(), Some(Ok(())), "the writer during the discards");
    let mut g = h.lock().unwrap();
    assert_eq!(g.stats().dropped, 0, "an automatic snapshot went while freed chunks were on their way back");
    assert!(g.info(keep).is_ok());
    for i in 0..40 {
        assert_eq!(read_stamp(&mut g, wv, i * PER), i + 70000);
    }
    for i in 0..CHUNKS {
        assert_eq!(read_stamp(&mut g, v, i * PER), i + 5000);
        assert_eq!(read_stamp(&mut g, keep, i * PER), i + 5000);
    }
    for i in 0..filled {
        assert_eq!(read_stamp(&mut g, fill, i * PER), i + 20000);
    }
    assert_eq!(g.trim_queued(), 0);
    reopens_alike(&g, &w.mem);
}

/// M2: a commit (a take's, right after its thaw with the copy window still open) and `settle` (fsync's answer)
/// send no discards; `trim_owed` sends a bounded part (the take calls it after its window closed).
#[test]
fn a_commit_and_settle_leave_the_discards_to_trim_owed() {
    let w = Watch::new(dev(512, 1024, 4096, 8));
    let (p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
    let h = PoolHandle::new(p, "trim-m2");
    h.lock().unwrap().delete(s).unwrap();
    let queued = h.lock().unwrap().trim_queued();
    assert!(queued >= CHUNKS, "{queued} queued");
    let (n0, c0) = (w.count(), w.chunks());
    h.lock().unwrap().write(v, 0, &block(1, 512), false, Normal).unwrap();
    h.commit().unwrap();
    h.commit().unwrap();
    h.settle().unwrap();
    assert_eq!(w.count(), n0, "a commit or settle sent discards");
    assert_eq!(h.lock().unwrap().trim_queued(), queued);
    h.trim_owed();
    let (sent, round) = (w.count() - n0, h.lock().unwrap().round_chunks());
    assert!((1..=4 * round).contains(&sent), "{sent} discards, rounds of at most {round} chunks");
    assert_eq!(h.lock().unwrap().trim_queued(), queued - (w.chunks() - c0));
}

/// L1: a delete sends what was queued when its commit ended, not what commits queue meanwhile (before, it went on
/// as long as anyone freed chunks).
#[test]
fn a_delete_does_not_chase_what_commits_free_during_its_discards() {
    // Room enough that no allocation takes the queue undiscarded (`untrim`).
    let w = Watch::new(dev(512, 2048, 4096, 8));
    let (mut p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
    let v2 = p.create_volume("b", 40 * PER * 512, 2).unwrap();
    for i in 0..40 {
        p.write(v2, i * PER, &block(i + 30000, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    let h = PoolHandle::new(p, "trim-l1");
    let (h2, mut n) = (h.clone(), 0u64);
    w.hook(move || {
        let mut g = h2.lock().unwrap();
        g.discard(v2, n * PER, PER).unwrap();
        g.commit().unwrap();
        n += 1;
        n < 40
    });
    h.delete(s).unwrap();
    let left = h.lock().unwrap().trim_queued();
    assert!(left >= 1, "the delete sent the discards of chunks freed after its commit");
    // The rest goes out with later rounds; then the pool is what it reopens as.
    drain(&h);
    let mut g = h.lock().unwrap();
    for i in 0..2 * CHUNKS {
        assert_eq!(read_stamp(&mut g, v, i * PER), scattered_stamp(i));
    }
    for i in 0..40 {
        assert_eq!(read_stamp(&mut g, v2, i * PER), 0, "discarded by the volume");
    }
    reopens_alike(&g, &w.mem);
}

/// A delete through the handle while another thread rewrites chunks, commits and sends owed discards itself: no
/// chunk is given out while its discard is under way (the data stays), and the pool is what it reopens as.
#[test]
fn a_delete_beside_writers_commits_and_other_rounds() {
    let w = Watch::new(dev(512, 1024, 4096, 8));
    w.delay_us.store(100, Ordering::Relaxed);
    let (p, v, s) = scattered(Arc::new(WatchDev(w.clone())));
    let h = PoolHandle::new(p, "trim-mt");
    let writer = {
        let h = h.clone();
        std::thread::spawn(move || {
            let mut last = alloc::collections::BTreeMap::new();
            for i in 0..200u64 {
                let c = (i * 37 % CHUNKS) * 2;
                let stamp = 100_000 + i;
                {
                    let mut g = h.lock().unwrap();
                    g.discard(v, c * PER, PER).unwrap();
                    g.write(v, c * PER, &block(stamp, 512), false, Normal).unwrap();
                }
                last.insert(c, stamp);
                if i % 5 == 0 {
                    h.commit().unwrap();
                }
                if i % 17 == 0 {
                    h.trim_owed();
                }
            }
            last
        })
    };
    h.delete(s).unwrap();
    let last = writer.join().unwrap();
    h.commit().unwrap();
    drain(&h);
    let mut g = h.lock().unwrap();
    assert_eq!(g.stats().trim_lost, 0);
    for i in 0..2 * CHUNKS {
        let want = last.get(&i).copied().unwrap_or(scattered_stamp(i));
        assert_eq!(read_stamp(&mut g, v, i * PER), want, "chunk {i}");
    }
    reopens_alike(&g, &w.mem);
}

/// M1, the other half: chunks under way count as free space against what an allocation must leave, so a write
/// that finds the queue empty and the free space at the reserve takes what will come back in a moment instead of
/// dropping an automatic snapshot (at most `round_chunks`, a quarter of the reserve, are ever under way).
#[test]
fn chunks_under_way_count_against_the_reserve() {
    const RESERVE: u64 = 16;
    let mut p = Pool::format(dev(512, 1024, 4096, 8), crate::Config { cache_pages: 4096, reserve_chunks: Some(RESERVE), ..cfg(4096) }).unwrap();
    p.set_trim_later();
    let v = p.create_volume("a", 200 * PER * 512, 1).unwrap();
    for i in 0..200 {
        p.write(v, i * PER, &block(i + 1, 512), false, Normal).unwrap();
    }
    let keep = p.snapshot(v, "keep", 2, true).unwrap();
    let x = p.create_volume("x", 8 * PER * 512, 3).unwrap();
    for i in 0..8 {
        p.write(x, i * PER, &block(i + 500, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    p.delete(x).unwrap();
    let runs = p.take_trim(64, u64::MAX);
    assert_eq!(p.trimming, p.round_chunks());
    let fill = p.create_volume("f", 1024 * PER * 512, 4).unwrap();
    let mut filled = 0;
    loop {
        while p.stats().free > RESERVE {
            p.write(fill, filled * PER, &block(filled + 20000, 512), false, Normal).unwrap();
            filled += 1;
        }
        p.commit().unwrap();
        p.untrim();
        if p.stats().free <= RESERVE && p.trim_len() == 0 {
            break;
        }
    }
    assert_eq!(p.trimming, p.round_chunks());
    for i in 0..2 {
        p.write(fill, (filled + i) * PER, &block(filled + i + 20000, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    assert_eq!(p.stats().dropped, 0, "an automatic snapshot went while chunks were on their way back");
    assert!(p.info(keep).is_ok());
    assert!(p.stats().free > 0);
    p.trim_done(&runs, &[]);
    for i in 0..filled + 2 {
        assert_eq!(read_stamp(&mut p, fill, i * PER), i + 20000);
    }
    let again = Pool::open(p.device().clone(), crate::Config { reserve_chunks: Some(RESERVE), ..cfg(4096) }).unwrap();
    assert_eq!(again.alloc_digest(), p.alloc_digest(), "{}", p.alloc_diff(&again));
}
