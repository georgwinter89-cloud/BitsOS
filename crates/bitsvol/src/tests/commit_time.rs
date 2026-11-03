//! VS-7: what a commit reports (bytes, flushes, lock wait, I/O time), its slow budget, and the priority of its
//! writes. Times here are lower bounds the device's sleeps guarantee, never upper bounds (host load varies).

use super::{block, cfg, dev};
use crate::layout::PAGE;
use crate::memdev::MemDev;
use crate::timing::{CommitTime, IoStats, PREFLUSH_BYTES, SLOW_BASE_NS, SLOW_MIN_BPS};
use crate::{Pool, PoolHandle};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use std::sync::Mutex;
use std::time::Duration;

/// A `MemDev` whose cache flush takes `flush_ms` (and fails `fail_flushes` times), recording the priority of
/// every write.
struct SlowDev {
    mem: Arc<MemDev>,
    flush_ms: AtomicU64,
    fail_flushes: AtomicU64,
    prios: Mutex<Vec<IoPriority>>,
}

impl BlockDevice for SlowDev {
    fn name(&self) -> &str {
        "slow"
    }
    fn info(&self) -> BlockInfo {
        self.mem.info()
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.mem.read_blocks(lba, buf, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.prios.lock().unwrap().push(prio);
        self.mem.write_blocks(lba, buf, fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        std::thread::sleep(Duration::from_millis(self.flush_ms.load(Ordering::Relaxed)));
        if self.fail_flushes.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok() {
            return Err(DevError::Io);
        }
        self.mem.flush()
    }
}

fn slow() -> Arc<SlowDev> {
    slow_on(dev(512, 64, 4096, 8))
}

fn slow_on(mem: Arc<MemDev>) -> Arc<SlowDev> {
    Arc::new(SlowDev { mem, flush_ms: AtomicU64::new(0), fail_flushes: AtomicU64::new(0), prios: Mutex::new(Vec::new()) })
}

/// The pool's records of `name` since `seq` (tests run in parallel: pool names are unique, none a prefix of another).
fn events(seq: u64, pool: &str, name: &str) -> Vec<klog::Record> {
    klog::read_since(seq, 100_000).into_iter().filter(|r| r.name == name && r.text.contains(pool)).collect()
}

const MS: u64 = 1_000_000;

#[test]
fn a_flush_reports_bytes_flushes_and_io_time() {
    let d = slow();
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-fields");
    let vd = h.open_volume(v).unwrap();
    for b in 0..3 {
        vd.write_blocks(b, &block(b + 1, 512), false, IoPriority::Normal).unwrap();
    }
    d.flush_ms.store(20, Ordering::Relaxed);
    vd.flush().unwrap();
    let t = h.last_commit();
    assert_eq!(t.why, "flush");
    // The data's flush and the superblock (MemDev has no FUA: written, then nothing more to flush).
    assert!(t.io.flushes >= 1, "{t:?}");
    assert!(t.io.covered >= 3 * 512, "the flush made the three writes durable: {t:?}");
    assert!(t.io.bytes >= 2 * PAGE as u64, "metadata pages and the superblock: {t:?}");
    assert!(t.io.flush_ns >= 20 * MS && t.ns >= t.io_ns(), "{t:?}");
    // Nothing written since: an idle commit, no flush, no bytes.
    vd.flush().unwrap();
    let t = h.last_commit();
    assert_eq!((t.io.flushes, t.io.bytes, t.io.covered), (0, 0, 0), "{t:?}");
    // A FUA write is a commit too, and a take's commit goes through the handle's timing as well.
    vd.write_blocks(4, &block(9, 512), true, IoPriority::Normal).unwrap();
    assert_eq!(h.last_commit().why, "fua");
    assert!(h.last_commit().io.covered >= 512);
    h.commit().unwrap();
    assert_eq!(h.last_commit().why, "commit");
}

#[test]
fn the_wait_for_the_pool_lock_is_its_own_field() {
    let d = slow();
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-lock");
    let vd = h.open_volume(v).unwrap();
    vd.write_blocks(0, &block(1, 512), false, IoPriority::Normal).unwrap();
    let held = h.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::scope(|s| {
        let j = s.spawn(|| {
            tx.send(()).unwrap();
            vd.flush().unwrap();
            h.last_commit()
        });
        rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(40));
        drop(held);
        j.join().unwrap()
    });
    // The flusher reads its clock right after the signal: it waited (almost) the whole sleep.
    assert!(t.lock_wait_ns >= 30 * MS, "{t:?}");
    assert!(t.ns < t.lock_wait_ns + 1_000 * MS, "the wait is not in the held time: {t:?}");
}

#[test]
fn the_budget_grows_with_the_bytes_the_flushes_carry() {
    let t = |ns, bytes, covered| CommitTime { why: "flush", ns, io: IoStats { bytes, covered, ..IoStats::default() }, ..CommitTime::default() };
    assert_eq!(t(0, 0, 0).budget_ns(), SLOW_BASE_NS);
    // The 24.10. commit: 2.01 s is slow only when it carried less than ~1 MiB.
    assert!(t(2_011_927_481, 4096, 0).slow());
    assert!(!t(2_011_927_481, 4096, SLOW_MIN_BPS + SLOW_MIN_BPS / 8).slow());
    assert!(!t(SLOW_BASE_NS, 0, 0).slow() && t(SLOW_BASE_NS + 1, 0, 0).slow());
    // 8 MiB of write-back at the floor rate: 9 s.
    assert_eq!(t(0, 0, 8 * SLOW_MIN_BPS).budget_ns(), SLOW_BASE_NS + 8_000_000_000);
    assert!(!t(u64::MAX, u64::MAX, u64::MAX).slow(), "no overflow");
    // The flush before the lock counts with its bytes.
    let pre = CommitTime { ns: 10 * MS, pre_ns: 2 * SLOW_BASE_NS, pre_bytes: 4 << 20, ..CommitTime::default() };
    assert!(!pre.slow() && CommitTime { pre_bytes: 0, ..pre }.slow());
}

#[test]
fn much_written_is_flushed_before_the_pool_lock() {
    let d = slow_on(dev(512, 256, 4096, 8));
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 128 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-pre");
    let vd = h.open_volume(v).unwrap();
    let big = alloc::vec![7u8; 4096];
    for b in 0..(PREFLUSH_BYTES / 4096 + 8) {
        vd.write_blocks(b * 8, &big, false, IoPriority::Normal).unwrap();
    }
    d.flush_ms.store(30, Ordering::Relaxed);
    let flushes = d.mem.flushes();
    vd.flush().unwrap();
    let t = h.last_commit();
    assert!(t.pre_bytes >= PREFLUSH_BYTES && t.pre_ns >= 30 * MS, "{t:?}");
    assert!(t.io.covered < PREFLUSH_BYTES, "the commit's barrier no longer carries the data: {t:?}");
    assert!(d.mem.flushes() - flushes >= 2, "the flush before the lock and the commit's own barrier");
    // Little written: no flush before the lock.
    vd.write_blocks(0, &big, false, IoPriority::Normal).unwrap();
    vd.flush().unwrap();
    assert_eq!((h.last_commit().pre_bytes, h.last_commit().pre_ns), (0, 0));
}

#[test]
fn commit_writes_go_at_least_normal() {
    let d = slow();
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    let run = |p: &mut Pool, b: u64, prio| {
        p.write(v, b, &block(b + 1, 512), false, IoPriority::Background).unwrap();
        d.prios.lock().unwrap().clear();
        p.commit_at(prio).unwrap();
        core::mem::take(&mut *d.prios.lock().unwrap())
    };
    let all = |w: &[IoPriority], want| !w.is_empty() && w.iter().all(|&x| x == want);
    let w = run(&mut p, 0, IoPriority::Background);
    assert!(all(&w, IoPriority::Normal), "a Background commit holds the pool lock: Normal ({w:?})");
    let w = run(&mut p, 8, IoPriority::Interactive);
    assert!(all(&w, IoPriority::Interactive), "{w:?}");
    let w = run(&mut p, 16, IoPriority::Normal);
    assert!(all(&w, IoPriority::Normal), "{w:?}");
}

#[test]
fn a_slow_commit_is_logged_with_its_fields() {
    let d = slow();
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-slowlog");
    let vd = h.open_volume(v).unwrap();
    vd.write_blocks(0, &block(1, 512), false, IoPriority::Normal).unwrap();
    let seq = klog::next_seq();
    d.flush_ms.store(SLOW_BASE_NS / MS + 20, Ordering::Relaxed);
    vd.flush().unwrap();
    assert!(h.last_commit().slow(), "{:?}", h.last_commit());
    let recs = klog::read_since(seq, 10_000);
    let r = recs.iter().find(|r| r.name == "commit.slow" && r.text.contains("vs7-slowlog")).expect("commit.slow");
    for k in ["why=flush", "lock_wait_ns=", "io_ns=", "flush_ns=", "flushes=", "bytes=", "covered=", "pre_ns=", "pre_bytes=", "budget_ns="] {
        assert!(r.text.contains(k), "{k} missing in {}", r.text);
    }
}

#[test]
fn a_fast_commit_is_no_warning() {
    let d = slow();
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-neg-fast");
    let vd = h.open_volume(v).unwrap();
    let seq = klog::next_seq();
    vd.write_blocks(0, &block(1, 512), false, IoPriority::Normal).unwrap();
    vd.flush().unwrap();
    assert!(!h.last_commit().slow(), "{:?}", h.last_commit());
    assert!(events(seq, "vs7-neg-fast", "commit.slow").is_empty());
}

#[test]
fn a_long_commit_within_its_budget_is_no_warning() {
    let d = slow_on(dev(512, 512, 4096, 8));
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 400 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-neg-long");
    let vd = h.open_volume(v).unwrap();
    // 1 MiB in big writes: few metadata pages, so no commit inside a write takes the bytes with it.
    let big = alloc::vec![5u8; 64 << 10];
    for i in 0..16u64 {
        vd.write_blocks(i * 128, &big, false, IoPriority::Normal).unwrap();
    }
    let seq = klog::next_seq();
    // Two flushes (before the lock, the commit's own) of 0.6 s: over the base, but the stick may take 2 s
    // for 1 MiB.
    d.flush_ms.store(SLOW_BASE_NS / MS * 6 / 10, Ordering::Relaxed);
    vd.flush().unwrap();
    let t = h.last_commit();
    assert!(t.ns + t.pre_ns > SLOW_BASE_NS, "longer than the base alone allows: {t:?}");
    assert!(t.pre_bytes + t.io.covered >= SLOW_MIN_BPS, "{t:?}");
    assert!(!t.slow(), "{t:?}");
    assert!(events(seq, "vs7-neg-long", "commit.slow").is_empty(), "no Warn within the budget");
}

#[test]
fn a_commit_inside_a_write_clears_what_the_flush_before_the_lock_carries() {
    let d = slow_on(dev(512, 512, 4096, 8));
    let mut c = cfg(4096);
    c.commit_dirty_pages = 1;
    let mut p = Pool::format(d.clone(), c).unwrap();
    let v = p.create_volume("a", 400 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-inner");
    let vd = h.open_volume(v).unwrap();
    // Every 4 KiB write dirties a metadata page, so the pool commits (and flushes) inside each write.
    let page = alloc::vec![3u8; 4096];
    for i in 0..(PREFLUSH_BYTES / 4096 + 8) {
        vd.write_blocks(i * 8, &page, false, IoPriority::Normal).unwrap();
    }
    let flushes = d.mem.flushes();
    vd.flush().unwrap();
    let t = h.last_commit();
    assert_eq!((t.pre_bytes, t.pre_ns), (0, 0), "the inner commits made the data durable: {t:?}");
    assert!(t.io.covered < PREFLUSH_BYTES, "{t:?}");
    assert!(d.mem.flushes() - flushes <= 1, "no flush before the lock");
}

#[test]
fn failing_flushes_are_reported_and_retried() {
    let d = slow_on(dev(512, 256, 4096, 8));
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 128 * 4096, 1).unwrap();
    p.commit().unwrap();
    let h = PoolHandle::new(p, "vs7-fail");
    let vd = h.open_volume(v).unwrap();
    let big = alloc::vec![9u8; 64 << 10];
    for i in 0..(PREFLUSH_BYTES / (64 << 10) + 1) {
        vd.write_blocks(i * 128, &big, false, IoPriority::Normal).unwrap();
    }
    let seq = klog::next_seq();
    // The flush before the lock fails, the commit's own barrier carries the data instead.
    d.fail_flushes.store(1, Ordering::Relaxed);
    vd.flush().unwrap();
    let t = h.last_commit();
    assert_eq!(events(seq, "vs7-fail", "commit.preflush_failed").len(), 1);
    assert_eq!(t.pre_bytes, 0, "a failed flush made nothing durable: {t:?}");
    assert!(t.io.covered >= PREFLUSH_BYTES, "{t:?}");
    // Every flush fails: the commit fails, nothing counts as durable, and the next try carries it all again.
    vd.write_blocks(0, &big, false, IoPriority::Normal).unwrap();
    d.fail_flushes.store(u64::MAX, Ordering::Relaxed);
    assert!(vd.flush().is_err());
    let t = h.last_commit();
    assert_eq!((t.why, t.pre_bytes, t.io.covered), ("flush", 0, 0), "{t:?}");
    assert!(t.io.flushes >= 1, "{t:?}");
    d.fail_flushes.store(0, Ordering::Relaxed);
    vd.flush().unwrap();
    let t = h.last_commit();
    assert!(t.pre_bytes + t.io.covered >= 64 << 10, "the failed commit's data is still owed: {t:?}");
}

#[test]
fn only_a_quarter_longer_commit_is_the_longest() {
    let pool: alloc::string::String = "vs7-longest".into();
    let longest = AtomicU64::new(0);
    let seq = klog::next_seq();
    let mut infos = Vec::new();
    for ms in [50, 100, 125, 126, 126, 157, 158, 90, 400] {
        let t = CommitTime { why: "flush", ns: ms * MS, ..CommitTime::default() };
        let before = events(seq, "vs7-longest", "commit.longest").len();
        crate::timing::record(&pool, &longest, &t);
        if events(seq, "vs7-longest", "commit.longest").len() > before {
            infos.push(ms);
        }
    }
    // 50: under the floor; 125 = 100 * 5/4 and 157 = 126 * 5/4 (rounded down) are not more; 90 is shorter.
    assert_eq!(infos, [100, 126, 158, 400]);
    // Over its budget: a Warn, never the Info line.
    let t = CommitTime { why: "flush", ns: SLOW_BASE_NS * 2, ..CommitTime::default() };
    crate::timing::record(&pool, &longest, &t);
    assert_eq!(events(seq, "vs7-longest", "commit.longest").len(), 4);
    assert_eq!(events(seq, "vs7-longest", "commit.slow").len(), 1);
}
