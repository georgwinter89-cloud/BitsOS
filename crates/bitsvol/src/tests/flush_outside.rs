//! VS-H: a volume's flush with no metadata owed runs without the pool lock, and an fsync still only returns once a
//! device flush that covers its writes has ended. The device's flush waits until the test lets it go, so these are
//! orders of events, no clock is judged (the 30 s are how long a test waits for what must happen before it fails).

use super::{block, cfg, dev};
use crate::memdev::MemDev;
use crate::{Pool, PoolHandle, VolumeDevice};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority::{self, Normal}};
use std::time::{Duration, Instant};

/// A device whose flushes wait while `hold` is on (`started` counts those asked for, `ended` those through) and whose
/// plain writes wait while `hold_write` is on, before they land (`writing` counts those at the gate).
struct GateDev {
    mem: Arc<MemDev>,
    hold: AtomicBool,
    fail: AtomicBool,
    started: AtomicU64,
    ended: AtomicU64,
    hold_write: AtomicBool,
    writing: AtomicU64,
}

impl BlockDevice for GateDev {
    fn name(&self) -> &str {
        "gate"
    }
    fn info(&self) -> BlockInfo {
        self.mem.info()
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.mem.read_blocks(lba, buf, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        if !fua && self.hold_write.load(Ordering::SeqCst) {
            self.writing.fetch_add(1, Ordering::SeqCst);
            while self.hold_write.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
        self.mem.write_blocks(lba, buf, fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        while self.hold.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        self.ended.fetch_add(1, Ordering::SeqCst);
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(DevError::Io);
        }
        self.mem.flush()
    }
}

/// A committed volume with its first chunks mapped, so rewrites owe no metadata.
fn setup(name: &str) -> (Arc<GateDev>, Arc<PoolHandle>, Arc<VolumeDevice>) {
    let mem = dev(512, 64, 4096, 8);
    let d = Arc::new(GateDev { mem, hold: AtomicBool::new(false), fail: AtomicBool::new(false), started: AtomicU64::new(0), ended: AtomicU64::new(0), hold_write: AtomicBool::new(false), writing: AtomicU64::new(0) });
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 16 * 4096, 1).unwrap();
    for b in 0..8 {
        p.write(v, b * 8, &block(b + 1, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    d.started.store(0, Ordering::SeqCst);
    d.ended.store(0, Ordering::SeqCst);
    let h = PoolHandle::new(p, name);
    let vd = Arc::new(h.open_volume(v).unwrap());
    (d, h, vd)
}

fn soon(what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(30), "{what}");
        std::thread::yield_now();
    }
}

fn flush_thread(vd: &Arc<VolumeDevice>) -> (std::thread::JoinHandle<Result<(), DevError>>, Arc<AtomicBool>) {
    let (v2, done) = (vd.clone(), Arc::new(AtomicBool::new(false)));
    let d2 = done.clone();
    let t = std::thread::spawn(move || {
        let r = v2.flush();
        d2.store(true, Ordering::SeqCst);
        r
    });
    (t, done)
}

#[test]
fn a_data_flush_leaves_reads_and_writes_going() {
    let (d, _h, vd) = setup("vsh-out1");
    vd.write_blocks(0, &block(100, 512), false, Normal).unwrap();
    d.hold.store(true, Ordering::SeqCst);
    let (f, done) = flush_thread(&vd);
    soon("the flush reached the device", || d.started.load(Ordering::SeqCst) == 1);
    // In the old way these wait for the pool lock the flush holds, and the flush is not let go until they are through.
    let (v2, through) = (vd.clone(), Arc::new(AtomicBool::new(false)));
    let t2 = through.clone();
    let io = std::thread::spawn(move || {
        let mut b = [0u8; 512];
        v2.read_blocks(8, &mut b, Normal).unwrap();
        v2.write_blocks(16, &block(101, 512), false, Normal).unwrap();
        t2.store(true, Ordering::SeqCst);
    });
    soon("a read and a write got through during the device flush", || through.load(Ordering::SeqCst));
    assert!(!done.load(Ordering::SeqCst));
    d.hold.store(false, Ordering::SeqCst);
    io.join().unwrap();
    f.join().unwrap().unwrap();
}

#[test]
fn a_second_fsync_waits_for_a_flush_that_covers_it() {
    let (d, _h, vd) = setup("vsh-out2");
    vd.write_blocks(0, &block(100, 512), false, Normal).unwrap();
    d.hold.store(true, Ordering::SeqCst);
    let (f1, done1) = flush_thread(&vd);
    soon("first flush at the device", || d.started.load(Ordering::SeqCst) == 1);
    // Written after the flush under way began: only a new device flush covers it.
    vd.write_blocks(8, &block(101, 512), false, Normal).unwrap();
    let (f2, done2) = flush_thread(&vd);
    std::thread::sleep(Duration::from_millis(20));
    assert!(!done2.load(Ordering::SeqCst), "returned before any device flush ended");
    assert_eq!(d.started.load(Ordering::SeqCst), 1, "flushes run one at a time");
    d.hold.store(false, Ordering::SeqCst);
    f1.join().unwrap().unwrap();
    f2.join().unwrap().unwrap();
    assert!(done1.load(Ordering::SeqCst));
    assert_eq!(d.ended.load(Ordering::SeqCst), 2, "the second write needs its own flush");
}

#[test]
fn an_fsync_with_nothing_new_returns_only_after_the_flush_under_way() {
    let (d, _h, vd) = setup("vsh-out3");
    vd.write_blocks(0, &block(100, 512), false, Normal).unwrap();
    d.hold.store(true, Ordering::SeqCst);
    let (f1, _) = flush_thread(&vd);
    soon("first flush at the device", || d.started.load(Ordering::SeqCst) == 1);
    // Nothing written since it began, yet its write may be what the flush under way covers.
    let (f2, done2) = flush_thread(&vd);
    std::thread::sleep(Duration::from_millis(20));
    assert!(!done2.load(Ordering::SeqCst), "returned while the flush covering it was still running");
    d.hold.store(false, Ordering::SeqCst);
    f1.join().unwrap().unwrap();
    f2.join().unwrap().unwrap();
    assert_eq!(d.ended.load(Ordering::SeqCst), 1, "fsyncs waiting together share one flush");
}

#[test]
fn a_flush_owing_metadata_still_commits() {
    let (d, h, vd) = setup("vsh-out4");
    // A chunk not mapped yet: its mapping must be durable before the flush returns.
    vd.write_blocks(9 * 8, &block(100, 512), false, Normal).unwrap();
    let gen0 = h.lock().unwrap().generation();
    vd.flush().unwrap();
    assert_eq!(h.lock().unwrap().generation(), gen0 + 1);
    let image = d.mem.durable();
    let mut again = Pool::open(Arc::new(MemDev::from_image("after", 512, 8, image).unwrap()), cfg(4096)).unwrap();
    let v = vd.id();
    assert_eq!(super::read_stamp(&mut again, v, 9 * 8), 100);
}

#[test]
fn a_failed_flush_is_owed_to_the_next_one() {
    let (d, h, vd) = setup("vsh-out5");
    vd.write_blocks(0, &block(100, 512), false, Normal).unwrap();
    d.fail.store(true, Ordering::SeqCst);
    assert_eq!(vd.flush(), Err(DevError::Io));
    let tried = d.ended.load(Ordering::SeqCst);
    vd.flush().unwrap();
    assert_eq!(d.ended.load(Ordering::SeqCst), tried + 1, "the failed flush is done again");
    let flushes = h.lock().unwrap().barriers().0;
    vd.flush().unwrap();
    assert_eq!(h.lock().unwrap().barriers().0, flushes, "an idle flush costs none");
}

/// The checker's order of events on the volume's write path: the flush past the pool lock clears the mark while a write
/// is on its way to the device, and the write lands after that flush. Marked before the device took it, the write's own
/// fsync finds nothing to flush and reports data as durable that a power cut loses.
#[test]
fn a_write_landing_behind_a_flush_gets_its_own() {
    let (d, h, vd) = setup("vsh-out6");
    // Something finished, so the flush below really reaches the device, as under fsync load.
    vd.write_blocks(8, &block(99, 512), false, Normal).unwrap();
    let io = h.lock().unwrap().barrier_state();
    d.hold_write.store(true, Ordering::SeqCst);
    let v2 = vd.clone();
    let w = std::thread::spawn(move || v2.write_blocks(0, &block(100, 512), false, Normal));
    soon("the write at the device", || d.writing.load(Ordering::SeqCst) == 1);
    // As `flush_volume` past the pool lock, which the write holds.
    let dyn_dev: Arc<dyn BlockDevice> = d.clone();
    io.flush(&dyn_dev).unwrap();
    let after_first = d.ended.load(Ordering::SeqCst);
    assert_eq!(after_first, 1, "the first flush reached the device");
    d.hold_write.store(false, Ordering::SeqCst);
    w.join().unwrap().unwrap();
    vd.flush().unwrap();
    assert_eq!(d.ended.load(Ordering::SeqCst), after_first + 1, "the fsync of a write that landed after the flush must flush");
    let mut again = Pool::open(Arc::new(MemDev::from_image("after", 512, 8, d.mem.durable()).unwrap()), cfg(4096)).unwrap();
    assert_eq!(super::read_stamp(&mut again, vd.id(), 0), 100, "fsync returned Ok, the write must survive a power cut");
}

/// The same on a commit: a flush past the pool lock during the commit's metadata runs must not stand in for the commit's
/// barrier, or the superblock goes out naming pages still in the device's cache.
#[test]
fn a_commit_flushes_its_pages_after_a_flush_on_the_way() {
    let (d, h, vd) = setup("vsh-out7");
    // A chunk not mapped yet: the commit owes pages.
    vd.write_blocks(9 * 8, &block(100, 512), false, Normal).unwrap();
    let (io, gen0) = {
        let p = h.lock().unwrap();
        (p.barrier_state(), p.generation())
    };
    d.hold_write.store(true, Ordering::SeqCst);
    let h2 = h.clone();
    let c = std::thread::spawn(move || h2.commit());
    soon("the commit's run at the device", || d.writing.load(Ordering::SeqCst) == 1);
    let dyn_dev: Arc<dyn BlockDevice> = d.clone();
    io.flush(&dyn_dev).unwrap();
    let after_first = d.ended.load(Ordering::SeqCst);
    assert_eq!(after_first, 1, "the first flush reached the device");
    d.hold_write.store(false, Ordering::SeqCst);
    c.join().unwrap().unwrap();
    assert_eq!(d.ended.load(Ordering::SeqCst), after_first + 1, "the commit's own barrier must run");
    let mut again = Pool::open(Arc::new(MemDev::from_image("after", 512, 8, d.mem.durable()).unwrap()), cfg(4096)).unwrap();
    assert_eq!(again.generation(), gen0 + 1);
    assert_eq!(super::read_stamp(&mut again, vd.id(), 9 * 8), 100);
}

const WRITERS: u64 = 4;
const ROUNDS: u64 = 30;
const TAKES: u64 = 6;

/// `snap.fsyncs`' shape on a device whose flush takes real time: its counters and the longest the test is allowed to go
/// on. `inside` counts device reads and writes that began and ended inside one device flush; `most` is the most flushes
/// at the device at once. Counts only: load on the host changes the numbers, not what they must be.
struct Paced {
    mem: Arc<MemDev>,
    flush_gen: AtomicU64,
    at_once: AtomicU64,
    most: AtomicU64,
    inside: AtomicU64,
    flushes: AtomicU64,
}

impl Paced {
    fn io<T>(&self, f: impl FnOnce() -> T) -> T {
        let before = self.flush_gen.load(Ordering::Acquire);
        let r = f();
        if before % 2 == 1 && self.flush_gen.load(Ordering::Acquire) == before {
            self.inside.fetch_add(1, Ordering::Relaxed);
        }
        r
    }
}

impl BlockDevice for Paced {
    fn name(&self) -> &str {
        "paced"
    }
    fn info(&self) -> BlockInfo {
        self.mem.info()
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.io(|| self.mem.read_blocks(lba, buf, prio))
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.io(|| self.mem.write_blocks(lba, buf, fua, prio))
    }
    fn flush(&self) -> Result<(), DevError> {
        let n = self.at_once.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(n, Ordering::SeqCst);
        self.flush_gen.fetch_add(1, Ordering::AcqRel);
        std::thread::sleep(Duration::from_millis(2));
        let r = self.mem.flush();
        self.flush_gen.fetch_add(1, Ordering::AcqRel);
        self.at_once.fetch_sub(1, Ordering::SeqCst);
        self.flushes.fetch_add(1, Ordering::Relaxed);
        r
    }
}

/// How an fsync reaches the device in `fsync_shape`.
type Fsync = fn(&PoolHandle, &VolumeDevice) -> Result<(), DevError>;

/// Four writers rewrite their block and fsync, half of them settling as fsync does in a take's window, a reader runs
/// through it all (a take's write-back stands in the same queue) and takes run alongside. Returns (fsyncs done, I/O inside
/// device flushes, flushes at once, flushes). Every last fsync must be in the image a power cut leaves.
fn fsync_shape(name: &str, fsync: Fsync) -> (u64, u64, u64, u64) {
    let mem = dev(512, 256, 4096, 8);
    let d = Arc::new(Paced { mem, flush_gen: AtomicU64::new(0), at_once: AtomicU64::new(0), most: AtomicU64::new(0), inside: AtomicU64::new(0), flushes: AtomicU64::new(0) });
    let mut p = Pool::format(d.clone(), cfg(4096)).unwrap();
    let v = p.create_volume("a", 64 * 4096, 1).unwrap();
    for w in 0..WRITERS {
        p.write(v, w * 8, &block(1, 512), false, Normal).unwrap();
    }
    p.commit().unwrap();
    let h = PoolHandle::new(p, name);
    let vd = Arc::new(h.open_volume(v).unwrap());
    let (done, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let (h2, v2, d2) = (h.clone(), vd.clone(), done.clone());
        writers.push(std::thread::spawn(move || {
            for r in 0..ROUNDS {
                v2.write_blocks(w * 8, &block(w * 1000 + r, 512), false, Normal).unwrap();
                fsync(&h2, &v2).unwrap();
                if w < WRITERS / 2 {
                    h2.settle().unwrap();
                }
                d2.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    let (v3, s3) = (vd.clone(), stop.clone());
    let reader = std::thread::spawn(move || {
        let mut b = [0u8; 512];
        let mut i = 0;
        while !s3.load(Ordering::SeqCst) {
            v3.read_blocks(i % 64, &mut b, Normal).unwrap();
            i += 1;
            std::thread::yield_now();
        }
    });
    let (h4, s4) = (h.clone(), stop.clone());
    let taker = std::thread::spawn(move || {
        let mut ids = Vec::new();
        for t in 0..TAKES {
            if s4.load(Ordering::SeqCst) {
                break;
            }
            let frozen = Arc::new(AtomicBool::new(true));
            let f2 = frozen.clone();
            let w = h4.window(v, Arc::new(move || f2.load(Ordering::SeqCst))).unwrap();
            ids.push(h4.lock().unwrap().snapshot_uncommitted(v, &alloc::format!("t{t}"), t, true).unwrap());
            frozen.store(false, Ordering::SeqCst);
            h4.commit().unwrap();
            drop(w);
            std::thread::sleep(Duration::from_millis(2));
        }
        for id in ids {
            h4.delete(id).unwrap();
        }
    });
    soon_long("every fsync returned", || done.load(Ordering::SeqCst) == WRITERS * ROUNDS);
    for t in writers {
        t.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    reader.join().unwrap();
    taker.join().unwrap();
    h.settle().unwrap();
    let image = d.mem.durable();
    let mut again = Pool::open(Arc::new(MemDev::from_image("after", 512, 8, image).unwrap()), cfg(4096)).unwrap();
    for w in 0..WRITERS {
        assert_eq!(super::read_stamp(&mut again, v, w * 8), w * 1000 + ROUNDS - 1, "writer {w}'s last fsync is not in what a power cut leaves");
    }
    (done.load(Ordering::SeqCst), d.inside.load(Ordering::Relaxed), d.most.load(Ordering::SeqCst), d.flushes.load(Ordering::Relaxed))
}

/// As `soon`, for a whole run on a loaded host: only a real stop fails it.
fn soon_long(what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(300), "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn four_fsync_writers_and_takes_leave_the_pool_free_during_a_flush() {
    let (fsyncs, inside, most, flushes) = fsync_shape("vsh-shape", |_, vd| vd.flush());
    assert_eq!(fsyncs, WRITERS * ROUNDS);
    assert!(inside > 0, "none of the pool's I/O ran inside any of {flushes} device flushes");
    assert_eq!(most, 1, "device flushes run one at a time");
}

/// The negative control: the old fsync, the same flush under the pool lock. The measure above must see the difference,
/// or its green says nothing.
#[test]
fn the_same_shape_with_the_flush_under_the_pool_lock_lets_no_io_in() {
    let (fsyncs, inside, most, flushes) = fsync_shape("vsh-shape-old", |h, _| h.lock().map_err(|_| DevError::Timeout)?.commit().map_err(crate::VolError::to_dev));
    assert_eq!(fsyncs, WRITERS * ROUNDS);
    assert!(flushes > 0);
    assert_eq!(inside, 0, "with every flush under the pool lock no I/O can run inside one");
    assert_eq!(most, 1);
}
