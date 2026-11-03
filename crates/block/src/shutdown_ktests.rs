//! RB-14 in-kernel tests of the storage stop on the test's own slow RAM disk (class Safe, rerunnable: the disk
//! is private, never in the registry, so the stick and /data are not touched):
//! - `rb.stop.slow_disk`: a writer keeps writing (an install's last files, the 5 s flusher) to a disk whose
//!   every call takes 20 ms; the stop closes it, waits for the write at the device, flushes, and only then calls
//!   the layer below. The device's call log ends with the flush, no write follows it, every write the writer
//!   was told succeeded is in the log, the writer's next write is held back (it waits, nothing reaches the
//!   device); reopened, the held write lands and the writer goes on.
//! - `rb.stop.held`: a write the device holds past the budget: the stop gives up on the disk (`Busy`, no flush
//!   racing the dispatcher, `quiet` false) and still runs the layers; released, the write completes.
//! - `rb.stop.flush_hangs`: the stop's own flush never answers (a stick that hangs on SYNCHRONIZE CACHE): the stop
//!   returns at its budget (`TimedOut`, `quiet` false) instead of waiting for the device; released, the helper
//!   thread's flush ends.

use crate::disk::{Disk, Dispatch};
use crate::error::BlockError;
use crate::policy::DiskKind;
use crate::ramdisk::{CallOp, RamDisk};
use crate::shutdown::{stop_these, Flushed, LayerNote, Stage, Stop};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::block::IoPriority::Normal;
use ksync::SpinLock;
use ktest::{kassert, ktest, TestResult};

const MS: u64 = 1_000_000;
const CHUNK: usize = 8 * 512;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

fn until(ms: u64, f: impl Fn() -> bool) -> bool {
    let end = now() + ms * MS;
    while now() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(MS);
    }
    f()
}

/// The layer below the disks: what it saw when the stop called it.
struct Probe {
    disk: Arc<Disk>,
    ram: Arc<RamDisk>,
    seen: SpinLock<Option<((usize, usize), usize)>>,
}

impl Stop for Probe {
    fn name(&self) -> &str {
        "rb14-probe"
    }
    fn stop(&self, _deadline_ns: u64) -> LayerNote {
        *self.seen.lock() = Some((self.disk.pending(), self.ram.calls().len()));
        LayerNote { idle: true, detail: String::new() }
    }
    fn resume(&self) {}
}

struct Writer {
    stop: AtomicBool,
    done: AtomicBool,
    ok: SpinLock<Vec<u64>>,
    first_err: SpinLock<Option<BlockError>>,
    next: AtomicU64,
}

fn start_writer(d: &Arc<Disk>) -> Result<Arc<Writer>, String> {
    let w = Arc::new(Writer { stop: AtomicBool::new(false), done: AtomicBool::new(false), ok: SpinLock::new(Vec::new()),
        first_err: SpinLock::new(None), next: AtomicU64::new(0) });
    let (w2, d2) = (w.clone(), d.clone());
    sched::spawn("rb14-writer", sched::Class::Normal, move || {
        while !w2.stop.load(Ordering::Acquire) {
            let lba = w2.next.fetch_add(8, Ordering::AcqRel) % 4000;
            match d2.write(lba, &[0x5A; CHUNK], false, Normal) {
                Ok(()) => w2.ok.lock().push(lba),
                Err(x) => {
                    *w2.first_err.lock() = Some(x);
                    break;
                }
            }
        }
        w2.done.store(true, Ordering::Release);
    })
    .map_err(e)?;
    Ok(w)
}

fn slow(name: &str) -> Result<(Arc<RamDisk>, Arc<Disk>), String> {
    let r = Arc::new(RamDisk::try_new(name, 512, 4096).map_err(e)?.with_max_blocks(64));
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Thread).map_err(e)?;
    Ok((r, d))
}

fn end(r: &RamDisk, d: &Arc<Disk>) {
    r.resume();
    r.set_delay_ns(0);
    d.reopen();
    d.stop();
}

fn slow_disk() -> TestResult {
    let (r, d) = slow("rb14-slow")?;
    let res = (|| -> TestResult {
        r.set_delay_ns(20 * MS);
        r.record();
        let w = start_writer(&d)?;
        kassert!(until(2000, || w.ok.lock().len() >= 3), "the writer did not get going");
        let probe = Arc::new(Probe { disk: d.clone(), ram: r.clone(), seen: SpinLock::new(None) });
        let sum = stop_these("ktest", core::slice::from_ref(&d), alloc::vec![(Stage::Device, probe.clone() as Arc<dyn Stop>)], 2000 * MS);
        let s = &sum.disks[0];
        kassert!(s.drained && s.flush == Flushed::Done && s.failed_queued == 0 && sum.quiet, "{s:?}");
        kassert!(until(2000, || d.held() == 1), "the writer's next write was not held back (held {})", d.held());
        sched::sleep_ns(100 * MS);
        kassert!(!w.done.load(Ordering::Acquire) && w.first_err.lock().is_none(), "the writer's next write: {:?}", *w.first_err.lock());
        let ok_before: Vec<u64> = w.ok.lock().clone();
        let calls = r.calls();
        let last_write = calls.iter().rposition(|c| matches!(c.op, CallOp::Write { .. }));
        let last_flush = calls.iter().rposition(|c| c.op == CallOp::Flush);
        kassert!(matches!((last_write, last_flush), (Some(a), Some(b)) if b > a && b == calls.len() - 1),
            "the flush is the device's last call, after every write ({} calls)", calls.len());
        let seen = *probe.seen.lock();
        kassert!(seen == Some(((0, 0), calls.len())), "the layer ran with the disk quiet and nothing after it: {seen:?}");
        let wrote: Vec<u64> = calls.iter().filter(|c| matches!(c.op, CallOp::Write { .. })).map(|c| c.lba).collect();
        for lba in ok_before.iter() {
            kassert!(wrote.contains(lba), "write at {lba} was answered ok but never reached the device");
        }
        klog::event!(Info, "block", "ktest.stop_slow", writes = wrote.len(), at_device = s.at_device, ms = s.ms, dirty = s.dirty);
        d.reopen();
        kassert!(until(2000, || w.ok.lock().len() > ok_before.len()), "the held write did not land after the reopen");
        w.stop.store(true, Ordering::Release);
        kassert!(until(2000, || w.done.load(Ordering::Acquire)), "the writer did not stop");
        kassert!(w.first_err.lock().is_none(), "the writer failed: {:?}", *w.first_err.lock());
        d.write(4000, &[1; 512], false, Normal).map_err(e)?;
        Ok(())
    })();
    end(&r, &d);
    res
}
ktest!("rb.stop.slow_disk", Safe, slow_disk);

fn held() -> TestResult {
    let (r, d) = slow("rb14-held")?;
    let res = (|| -> TestResult {
        d.write(0, &[1; 512], false, Normal).map_err(e)?;
        r.pause();
        let t = d.submit(crate::queue::Kind::Write { fua: false, preflush: false }, 8, 0, crate::disk::Data::Bytes(alloc::vec![2; 512]), Normal)
            .map_err(e)?;
        kassert!(until(2000, || r.holding()), "the device never got the write");
        let probe = Arc::new(Probe { disk: d.clone(), ram: r.clone(), seen: SpinLock::new(None) });
        let t0 = now();
        let sum = stop_these("ktest", core::slice::from_ref(&d), alloc::vec![(Stage::Device, probe.clone() as Arc<dyn Stop>)], 50 * MS);
        let took = now() - t0;
        let s = &sum.disks[0];
        kassert!(!s.drained && s.flush == Flushed::Busy && !sum.quiet && s.at_device == 1, "{s:?}");
        kassert!(probe.seen.lock().is_some(), "the layers still run when a disk is not quiet");
        kassert!(took < 1000 * MS, "the stop kept to its budget ({} ms)", took / MS);
        r.resume();
        kassert!(t.wait().is_ok(), "the held write completes once the device answers");
        Ok(())
    })();
    end(&r, &d);
    res
}
ktest!("rb.stop.held", Safe, held);

fn flush_hangs() -> TestResult {
    let (r, d) = slow("rb14-flush")?;
    let res = (|| -> TestResult {
        d.write(0, &[3; 512], false, Normal).map_err(e)?;
        let flushes = r.flushes();
        r.pause();
        let t0 = now();
        let sum = stop_these("ktest", core::slice::from_ref(&d), Vec::new(), 100 * MS);
        let took = now() - t0;
        let s = &sum.disks[0];
        kassert!(s.drained && s.flush == Flushed::TimedOut && !sum.quiet, "{s:?}");
        kassert!(took < 1000 * MS, "the stop kept to its budget ({} ms)", took / MS);
        kassert!(until(2000, || r.holding()), "the flush never reached the device");
        r.resume();
        kassert!(until(2000, || r.flushes() > flushes), "the helper's flush did not end once the device answered");
        Ok(())
    })();
    end(&r, &d);
    res
}
ktest!("rb.stop.flush_hangs", Safe, flush_hangs);
