//! BL-2: reads mapped under the pool lock and read without it (`VolumeDevice::read_mapped`).

use super::{block, cfg, dev};
use crate::memdev::MemDev;
use crate::model::stamp_of;
use crate::volume::Piece;
use crate::{Pool, PoolHandle};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority, IoPriority::Normal};
use ksync::SpinLock;

const CS: u64 = 4096;

#[test]
fn a_read_plan_joins_adjacent_chunks_and_holes_and_refuses_hostile_ranges() {
    // 64 blocks per request: up to 8 chunks of 4 KiB join.
    let d = dev(512, 64, 4096, 64);
    let mut p = Pool::format(d, cfg(4096)).unwrap();
    let id = p.create_volume("data", 32 * CS, 1).unwrap();
    let data: Vec<u8> = (0..4 * 8u64).flat_map(|i| block(i + 1, 512)).collect();
    p.write(id, 0, &data, false, Normal).unwrap();
    p.write(id, 6 * 8, &block(99, 512).repeat(8), false, Normal).unwrap();
    let plan = p.read_plan(id, 0, 8 * CS as usize).unwrap();
    // The pieces cover the request in order; holes are exactly the unwritten chunks; neighbours that could have
    // been one piece (both holes, or the device bytes continue) are one; chunks allocated one after another join.
    let mut at = 0usize;
    for (i, x) in plan.iter().enumerate() {
        assert_eq!(x.at, at, "{plan:?}");
        at += x.len;
        if let Some(n) = plan.get(i + 1) {
            let joinable = match (x.src, n.src) {
                (None, None) => true,
                (Some(a), Some(b)) => a + x.len as u64 == b && x.len + n.len <= 8 * CS as usize,
                _ => false,
            };
            assert!(!joinable, "pieces {i} and {} should be one: {plan:?}", i + 1);
        }
    }
    assert_eq!(at, 8 * CS as usize);
    let holes: Vec<(usize, usize)> = plan.iter().filter(|x| x.src.is_none()).map(|x| (x.at, x.len)).collect();
    assert_eq!(holes, [(4 * CS as usize, 2 * CS as usize), (7 * CS as usize, CS as usize)], "{plan:?}");
    assert!(plan.iter().any(|x| x.src.is_some() && x.len > CS as usize), "adjacent chunks joined: {plan:?}");
    // A piece that starts inside a chunk keeps the offset.
    let plan = p.read_plan(id, 3, 1024).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].src.map(|s| s % CS), Some(3 * 512));
    // The bytes the plan reads are the volume's.
    let mut b = vec![0u8; 8 * CS as usize];
    p.read(id, 0, &mut b, Normal).unwrap();
    assert_eq!(&b[..data.len()], &data[..]);
    assert!(b[4 * CS as usize..6 * CS as usize].iter().all(|&x| x == 0));
    assert_eq!(stamp_of(&b[6 * CS as usize..][..512]), Some(99));
    // Past the end, an odd length, an LBA that overflows: refused.
    for (lba, len) in [(255u64, 1024usize), (0, 100), (u64::MAX, 512), (u64::MAX / 512, 512)] {
        assert!(p.read_plan(id, lba, len).is_err(), "lba {lba} len {len}");
    }
    let empty: Vec<Piece> = p.read_plan(id, 0, 0).unwrap();
    assert!(empty.is_empty());
}

/// A device that runs a hook once, just before it reads the bytes at `at` (inside a reader's read without the
/// pool lock), and counts the reads that touch `at`.
struct Hook {
    inner: Arc<MemDev>,
    at: AtomicU64,
    hook: SpinLock<Option<Box<dyn FnOnce() + Send>>>,
    reads_at: AtomicU64,
}

impl BlockDevice for Hook {
    fn name(&self) -> &str {
        "hook"
    }
    fn info(&self) -> BlockInfo {
        self.inner.info()
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        let (lo, at) = (lba * 512, self.at.load(Ordering::SeqCst));
        if lo <= at && at < lo + buf.len() as u64 {
            self.reads_at.fetch_add(1, Ordering::SeqCst);
            let h = self.hook.lock().take();
            if let Some(h) = h {
                h();
            }
        }
        self.inner.read_blocks(lba, buf, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.inner.write_blocks(lba, buf, fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        self.inner.flush()
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        self.inner.discard(lba, blocks)
    }
}

#[test]
fn a_read_whose_chunk_a_commit_released_meanwhile_reads_again() {
    let hook = Arc::new(Hook { inner: dev(512, 64, 4096, 8), at: AtomicU64::new(u64::MAX), hook: SpinLock::new(None), reads_at: AtomicU64::new(0) });
    let pool = Pool::format(hook.clone(), cfg(4096)).unwrap();
    // A short lock bound: a reader that (wrongly) held the pool lock makes the hook's lock fail at once.
    let h = PoolHandle::with_wait(pool, "data", 200_000_000);
    let (a, s, c) = {
        let mut p = h.lock().unwrap();
        let a = p.create_volume("data", 8 * CS, 1).unwrap();
        p.write(a, 0, &block(1, 512).repeat(8), false, Normal).unwrap();
        let s = p.snapshot(a, "s", 2, false).unwrap();
        let c = p.read_plan(a, 0, CS as usize).unwrap()[0].src.unwrap();
        (a, s, c)
    };
    let vd = h.open_volume(a).unwrap();
    hook.at.store(c, Ordering::SeqCst);
    let (h2, ran) = (h.clone(), Arc::new(AtomicBool::new(false)));
    let ran2 = ran.clone();
    *hook.hook.lock() = Some(Box::new(move || {
        // While the reader is in its device read: the chunk is copied away from (the snapshot held it), the
        // snapshot deleted and committed, so the chunk the reader mapped is free for anyone.
        let mut p = h2.lock().expect("the reader must not hold the pool lock across its device read");
        p.write(a, 0, &block(2, 512).repeat(8), false, Normal).unwrap();
        p.delete(s).unwrap();
        p.commit().unwrap();
        ran2.store(true, Ordering::SeqCst);
    }));
    let mut b = vec![0u8; CS as usize];
    vd.read_blocks(0, &mut b, Normal).unwrap();
    assert!(ran.load(Ordering::SeqCst), "the hook ran inside the read");
    assert!(b.chunks(512).all(|x| stamp_of(x) == Some(2)), "the read went again on the new mapping: {:?}", stamp_of(&b[..512]));
    assert_eq!(hook.reads_at.load(Ordering::SeqCst), 1, "the second read went to the new chunk, not the freed one");
}

#[test]
fn readers_beside_a_writer_that_frees_and_reuses_chunks_never_see_another_volumes_bytes() {
    // Volume A is overwritten under snapshots that come and go (copy, free, reuse); volume B writes stamps of
    // its own into whatever chunks come free. Readers of A see A's stamps (old or new), never B's.
    let d = dev(512, 96, 4096, 8);
    let h = PoolHandle::new(Pool::format(d, cfg(4096)).unwrap(), "data");
    let (a, b) = {
        let mut p = h.lock().unwrap();
        let a = p.create_volume("a", 8 * CS, 1).unwrap();
        let b = p.create_volume("b", 16 * CS, 1).unwrap();
        for vc in 0..8u64 {
            p.write(a, vc * 8, &block(vc + 1, 512).repeat(8), false, Normal).unwrap();
        }
        p.commit().unwrap();
        (a, b)
    };
    const B_BASE: u64 = 1_000_000;
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for t in 0..6u64 {
        let (vd, stop) = (h.open_volume(a).unwrap(), stop.clone());
        readers.push(std::thread::spawn(move || {
            let mut buf = vec![0u8; 2 * CS as usize];
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) || n < 50 {
                let vc = (n + t) % 7;
                vd.read_blocks(vc * 8, &mut buf, Normal).unwrap();
                for x in buf.chunks(512) {
                    let st = stamp_of(x).expect("a block no write produced");
                    assert!(st < B_BASE, "volume B's bytes in a read of A (stamp {st})");
                }
                n += 1;
            }
            n
        }));
    }
    for round in 0..60u64 {
        let mut p = h.lock().unwrap();
        let s = p.snapshot(a, "s", round, false).unwrap();
        let vc = round % 8;
        p.write(a, vc * 8, &block(100 + round, 512).repeat(8), false, Normal).unwrap();
        p.delete(s).unwrap();
        p.commit().unwrap();
        let bvc = round % 16;
        p.write(b, bvc * 8, &block(B_BASE + round, 512).repeat(8), false, Normal).unwrap();
        p.commit().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(reads >= 300);
}

/// A device that moves the pool's reuse count on every read of the bytes at `at` (a commit that released chunks
/// during each try) and notes whether the pool lock was free then.
struct Bumper {
    inner: Arc<MemDev>,
    at: AtomicU64,
    reuse: SpinLock<Option<Arc<AtomicU64>>>,
    handle: SpinLock<Option<Arc<PoolHandle>>>,
    reads_at: AtomicU64,
    locked_reads: AtomicU64,
}

impl BlockDevice for Bumper {
    fn name(&self) -> &str {
        "bumper"
    }
    fn info(&self) -> BlockInfo {
        self.inner.info()
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        let (lo, at) = (lba * 512, self.at.load(Ordering::SeqCst));
        if lo <= at && at < lo + buf.len() as u64 {
            self.reads_at.fetch_add(1, Ordering::SeqCst);
            let h = self.handle.lock().clone();
            if h.is_some_and(|h| h.pool_locked()) {
                self.locked_reads.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(r) = self.reuse.lock().as_ref() {
                r.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.inner.read_blocks(lba, buf, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.inner.write_blocks(lba, buf, fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        self.inner.flush()
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        self.inner.discard(lba, blocks)
    }
}

#[test]
fn a_read_whose_chunks_move_on_every_try_reads_under_the_lock_in_the_end() {
    let dv = Arc::new(Bumper {
        inner: dev(512, 64, 4096, 8),
        at: AtomicU64::new(u64::MAX),
        reuse: SpinLock::new(None),
        handle: SpinLock::new(None),
        reads_at: AtomicU64::new(0),
        locked_reads: AtomicU64::new(0),
    });
    let pool = Pool::format(dv.clone(), cfg(4096)).unwrap();
    *dv.reuse.lock() = Some(pool.reuse_counter());
    let h = PoolHandle::new(pool, "data");
    *dv.handle.lock() = Some(h.clone());
    let (a, c) = {
        let mut p = h.lock().unwrap();
        let a = p.create_volume("data", 8 * CS, 1).unwrap();
        p.write(a, 0, &block(7, 512).repeat(8), false, Normal).unwrap();
        p.commit().unwrap();
        (a, p.read_plan(a, 0, CS as usize).unwrap()[0].src.unwrap())
    };
    let vd = h.open_volume(a).unwrap();
    dv.at.store(c, Ordering::SeqCst);
    let mut b = vec![0u8; CS as usize];
    vd.read_blocks(0, &mut b, Normal).unwrap();
    assert!(b.chunks(512).all(|x| stamp_of(x) == Some(7)), "the right bytes in the end");
    let tries = crate::device::READ_TRIES as u64;
    assert_eq!(dv.reads_at.load(Ordering::SeqCst), tries + 1, "every try, then the locked read");
    assert_eq!(dv.locked_reads.load(Ordering::SeqCst), 1, "only the last read held the pool lock");
    *dv.handle.lock() = None;
}

#[test]
fn a_rollback_never_runs_beside_reads_of_the_open_volume() {
    // Pitfall 5 of BL-2: a rollback switches the whole mapping. It is refused while the volume is open (the
    // `busy` rule), so no lock-free reader of that volume can see the switch; after the close it goes, and the
    // next reader maps the rolled-back chunks under the lock.
    let h = PoolHandle::new(Pool::format(dev(512, 64, 4096, 8), cfg(4096)).unwrap(), "data");
    let (a, s) = {
        let mut p = h.lock().unwrap();
        let a = p.create_volume("data", 8 * CS, 1).unwrap();
        p.write(a, 0, &block(1, 512).repeat(8), false, Normal).unwrap();
        let s = p.snapshot(a, "s", 2, false).unwrap();
        p.write(a, 0, &block(2, 512).repeat(8), false, Normal).unwrap();
        p.commit().unwrap();
        (a, s)
    };
    let vd = h.open_volume(a).unwrap();
    let mut b = vec![0u8; CS as usize];
    vd.read_blocks(0, &mut b, Normal).unwrap();
    assert_eq!(stamp_of(&b[..512]), Some(2));
    assert!(matches!(h.lock().unwrap().rollback(a, s, "before", 3), Err(crate::VolError::Busy)), "refused while open");
    drop(vd);
    let kept = h.lock().unwrap().rollback(a, s, "before", 3).unwrap();
    h.lock().unwrap().delete(kept).unwrap();
    h.lock().unwrap().commit().unwrap();
    let vd = h.open_volume(a).unwrap();
    vd.read_blocks(0, &mut b, Normal).unwrap();
    assert!(b.chunks(512).all(|x| stamp_of(x) == Some(1)), "the rolled-back bytes");
}
