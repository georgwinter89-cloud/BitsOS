//! FS-12 in-kernel tests (class Safe, rerunnable: every disk is the test's own, every thread is joined, every
//! ioprio set is taken back):
//! - `block.prio.class`: a page-cache read goes at the reading thread's priority (KernelRt/Interactive ->
//!   Interactive, Normal, Background; ioprio IDLE lowers, RT raises only an Interactive thread, BE is Normal),
//!   and the read-ahead after a sequential miss reaches the disk as Background.
//! - `block.prio.sync`: a write-back run goes as Background only for the background flusher; an fsync goes
//!   at least as Normal (FS-6's rule), as Interactive from an Interactive thread.
//! - `block.ram.hog`: a Background thread queues 512 writes on a disk that takes 1 ms per call; a read of an
//!   Interactive thread goes next (at most two device calls later), long before the hog is done (the
//!   generous limit: 2 s), and every hog write completes. Event `block`/`hog.read` carries the latency.
//! - `block.ram.starve`: under a steady stream of Interactive reads a Background write is still served (by
//!   aging, or in a gap of the stream when the CPU is busy).
//! - `block.ram.starve_aged` (FS-15): with a Background write and eight Interactive reads queued behind a held device
//!   for longer than two aging steps, the write is the next call and counted as aged.

use crate::bdev::DiskFile;
use vfs::pagecache::SyncKind;
use crate::disk::{Data, Disk, Dispatch};
use crate::policy::DiskKind;
use crate::prio::{self, IOPRIO_CLASS_BE, IOPRIO_CLASS_IDLE, IOPRIO_CLASS_RT, IOPRIO_DEFAULT};
use crate::queue::Kind;
use crate::ramdisk::{CallOp, RamDisk};
use crate::Ticket;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::block::IoPriority::{self, *};
use ksync::SpinLock;
use ktest::{kassert, ktest, TestResult};
use sched::Class;

const MS: u64 = 1_000_000;

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

fn disk(name: &str, blocks: u64) -> Result<(Arc<RamDisk>, Arc<Disk>), String> {
    let r = Arc::new(RamDisk::try_new(name, 512, blocks).map_err(e)?.with_max_blocks(8));
    let d = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Thread).map_err(e)?;
    Ok((r, d))
}

fn close(r: &RamDisk, d: &Arc<Disk>) -> TestResult {
    r.set_delay_ns(0);
    r.resume();
    d.stop();
    kassert!(until(5000, || d.worker_done()), "{}: the dispatcher thread did not end", d.name());
    Ok(())
}

/// Runs `f` in a new thread of `class` and waits (`ms`) for its result.
fn in_class<T: Send + 'static>(class: Class, ms: u64, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let out: Arc<SpinLock<Option<T>>> = Arc::new(SpinLock::new(None));
    let o = out.clone();
    let h = sched::spawn("fs12-ktest", class, move || {
        let v = f();
        *o.lock() = Some(v);
    })
    .map_err(e)?;
    if !h.join_timeout(ms * MS) {
        return Err(format!("the {class:?} thread did not finish in {ms} ms"));
    }
    out.lock().take().ok_or_else(|| String::from("the thread gave no result"))
}

fn me() -> u64 {
    sched::current_thread().map_or(0, |t| t.id)
}

// ---- block.prio.class

fn prio_class() -> TestResult {
    let (r, d) = disk("fs12-cls", 1024)?;
    let res = (|| -> TestResult {
        let pc = Arc::new(DiskFile::page_cache(d.clone()));
        r.record();
        let none = IOPRIO_DEFAULT;
        let (rt, be, idle) = (prio::value(IOPRIO_CLASS_RT, 0), prio::value(IOPRIO_CLASS_BE, 4), prio::value(IOPRIO_CLASS_IDLE, 0));
        // Pages 8 apart: none of these reads is sequential to another (no read-ahead between them).
        let cases = [
            (8u64, Class::KernelRt, none, Interactive),
            (16, Class::Interactive, none, Interactive),
            (24, Class::Normal, none, Normal),
            (32, Class::Background, none, Background),
            (40, Class::Interactive, idle, Background),
            (48, Class::Normal, rt, Normal),
            (56, Class::Interactive, rt, Interactive),
            (64, Class::Background, be, Normal),
        ];
        for (pg, class, ioprio, want) in cases {
            let pc = pc.clone();
            let (seen, read) = in_class(class, 5000, move || {
                let id = me();
                let set = if ioprio != IOPRIO_DEFAULT { prio::set(id, ioprio) } else { Ok(()) };
                let seen = prio::current();
                let read = pc.read_at(pg * 4096, &mut [0u8; 1]);
                let _ = prio::set(id, IOPRIO_DEFAULT);
                (seen, set.and(read.map(|_| ())))
            })?;
            read.map_err(|x| format!("page {pg}: errno {x}"))?;
            kassert!(seen == want, "{class:?} ioprio {ioprio:#x}: current() {seen:?}, want {want:?}");
            let call = r.calls().into_iter().find(|c| c.op == CallOp::Read && c.lba == pg * 8);
            kassert!(call.map(|c| c.prio) == Some(want), "{class:?} ioprio {ioprio:#x}: device call {call:?}, want {want:?}");
        }
        // Read-ahead: a Normal thread misses page 0 (the file start); the pages after it come from the
        // read-ahead thread as Background.
        if !vfs::readahead::active() {
            return ktest::skip("no read-ahead thread in this kernel");
        }
        let pc0 = pc.clone();
        in_class(Class::Normal, 5000, move || pc0.read_at(0, &mut [0u8; 1]).map(|_| ()))?.map_err(|x| format!("page 0: errno {x}"))?;
        kassert!(until(5000, || r.calls().iter().any(|c| c.lba == 8 && c.op == CallOp::Read && c.prio == Background)), "no read-ahead of page 1: {:?}", r.calls());
        let p0 = r.calls().into_iter().find(|c| c.lba == 0);
        kassert!(p0.map(|c| c.prio) == Some(Normal), "the missed page goes at the reader's priority: {p0:?}");
        kassert!(until(2000, || pc.cached(1).is_some()), "the read-ahead page is cached");
        Ok(())
    })();
    close(&r, &d)?;
    res
}
ktest!("block.prio.class", Safe, prio_class);

// ---- block.prio.sync

fn prio_sync() -> TestResult {
    let (r, d) = disk("fs12-sync", 1024)?;
    let res = (|| -> TestResult {
        let pc = Arc::new(DiskFile::page_cache(d.clone()));
        r.record();
        // (page, thread class, sync kind, the write's priority at the device)
        let cases = [
            (100u64, Class::Background, SyncKind::Background, Background),
            (104, Class::Background, SyncKind::Fsync, Normal),
            (108, Class::Normal, SyncKind::Fsync, Normal),
            (112, Class::Interactive, SyncKind::Fsync, Interactive),
            (116, Class::Interactive, SyncKind::Background, Background),
        ];
        for (pg, class, why, want) in cases {
            let pc = pc.clone();
            let res = in_class(class, 5000, move || {
                pc.write_at(pg * 4096, &[pg as u8; 512]).map(|_| ())?;
                pc.sync_all(why)
            })?;
            res.map_err(|x| format!("page {pg}: {x:?}"))?;
            let call = r.calls().into_iter().find(|c| matches!(c.op, CallOp::Write { .. }) && c.lba == pg * 8);
            kassert!(call.map(|c| c.prio) == Some(want), "{class:?} {why:?}: device call {call:?}, want {want:?}");
        }
        Ok(())
    })();
    close(&r, &d)?;
    res
}
ktest!("block.prio.sync", Safe, prio_sync);

// ---- block.ram.hog

const HOG: u64 = 512;

fn ram_hog() -> TestResult {
    let (r, d) = disk("fs12-hog", 4096)?;
    // This run's own result slot and thread (a hog left from a failed run never writes into the next one).
    let hog_ok = Arc::new(AtomicU64::new(u64::MAX));
    let mut hog_thread = None;
    let res = (|| -> TestResult {
        r.set_delay_ns(MS);
        r.record();
        // The hog: a Background thread queues every write first (a write-back burst), then waits for them.
        let d2 = d.clone();
        let slot = hog_ok.clone();
        let hog = hog_thread.insert(sched::spawn("fs12-hog", Class::Background, move || {
            let p = prio::current();
            let tickets: Vec<Result<Ticket, _>> = (0..HOG).map(|i| d2.submit(Kind::Write { fua: false, preflush: false }, i * 2, 0, Data::Bytes(vec![i as u8; 512]), p)).collect();
            let ok = tickets.into_iter().filter(|t| matches!(t, Ok(_))).map(|t| t.map(|t| t.wait_timeout(120_000 * MS).is_ok()).unwrap_or(false)).filter(|&x| x).count();
            slot.store(ok as u64, Ordering::Release);
        })
        .map_err(e)?);
        kassert!(until(5000, || d.stats().queued >= HOG / 2), "the hog did not queue: {:?}", d.stats().queued);
        let d3 = d.clone();
        let r3 = r.clone();
        let (seen, before, ns, left, read) = in_class(Class::Interactive, 10_000, move || {
            let seen = prio::current();
            let before = r3.calls().len();
            let t0 = now();
            let read = d3.read_vec(4000, 512, seen).map(|_| ());
            (seen, before, now().saturating_sub(t0), d3.stats().queued, read)
        })?;
        read.map_err(e)?;
        klog::event!(Info, "block", "hog.read", latency_us = ns / 1000, hog = HOG, left = left, device_ms_per_call = 1u64);
        kassert!(seen == Interactive, "the Interactive thread's requests go as {seen:?}");
        let calls = r.calls();
        let pos = calls.iter().position(|c| c.op == CallOp::Read && c.lba == 4000);
        kassert!(pos.is_some_and(|p| p <= before + 2), "the read went to the device as call {pos:?}, {before} calls before it");
        kassert!(calls.get(pos.unwrap_or(0)).map(|c| c.prio) == Some(Interactive));
        kassert!(calls.iter().filter(|c| matches!(c.op, CallOp::Write { .. })).all(|c| c.prio == Background), "hog writes go as Background");
        kassert!(left > 0, "the read waited until the hog was done");
        kassert!(ns < 2000 * MS, "the read took {} ms behind the hog (limit 2000)", ns / MS);
        kassert!(hog.join_timeout(120_000 * MS), "the hog did not finish");
        let ok = hog_ok.load(Ordering::Acquire);
        kassert!(ok == HOG, "hog writes done: {ok} of {HOG}");
        Ok(())
    })();
    close(&r, &d)?;
    // After a failure the stopped disk fails the hog's tickets, so it ends quickly; never leave it running.
    if let Some(h) = hog_thread {
        kassert!(h.join_timeout(10_000 * MS), "the hog thread did not end after the disk stopped");
    }
    res
}
ktest!("block.ram.hog", Safe, ram_hog);

// ---- block.ram.starve

fn ram_starve() -> TestResult {
    let (r, d) = disk("fs12-starve", 4096)?;
    let res = (|| -> TestResult {
        d.set_age_ns(5 * MS);
        r.set_delay_ns(MS);
        r.record();
        let bg = d.submit(Kind::Write { fua: false, preflush: false }, 3001, 0, Data::Bytes(vec![7; 512]), Background).map_err(e)?;
        let read = |i: u64| d.submit(Kind::Read, (i % 1000) * 2, 0, Data::Bytes(vec![0; 512]), IoPriority::Interactive).map_err(e);
        let mut inflight: VecDeque<Ticket> = (0..8).map(read).collect::<Result<_, _>>()?;
        let mut served = None;
        for i in 8..400u64 {
            if let Some(t) = inflight.pop_front() {
                t.wait().map_err(e)?;
            }
            if bg.is_done() {
                served = Some(i);
                break;
            }
            inflight.push_back(read(i)?);
        }
        for t in inflight {
            t.wait().map_err(e)?;
        }
        kassert!(served.is_some(), "the Background write starved behind 400 Interactive reads");
        bg.wait().map_err(e)?;
        // FS-15: not "served by aging" here: a busy CPU 0 (full Mini-PC run, desk-minipc-w6) can let all reads
        // finish before the next one is queued, and the write then goes in that gap as a plain pick.
        // block.ram.starve_aged proves aging with a queue that is never empty.
        klog::event!(Debug, "block", "starve.served", reads = served.unwrap_or(0), aged = d.stats().aged);
        Ok(())
    })();
    close(&r, &d)?;
    res
}
ktest!("block.ram.starve", Safe, ram_starve);

// ---- block.ram.starve_aged

/// FS-15: aging, deterministically. The device holds a first read (paused), a Background write and then eight
/// Interactive reads queue behind it, and the test waits past two aging steps before letting the device go.
/// The write now competes as Interactive and is older than every read, so it is the very next call, picked as
/// aged, while eight Interactive reads still wait. The sleep is only a lower bound; a longer one changes
/// nothing (no timing is judged).
fn ram_starve_aged() -> TestResult {
    let (r, d) = disk("fs15-aging", 4096)?;
    let res = (|| -> TestResult {
        let age = 5 * MS;
        d.set_age_ns(age);
        let read = |lba: u64| d.submit(Kind::Read, lba, 0, Data::Bytes(vec![0; 512]), IoPriority::Interactive).map_err(e);
        r.pause();
        let first = read(0)?;
        if !until(2000, || r.holding()) {
            r.resume();
            return Err("the device never got the first read".into());
        }
        r.record();
        let aged_before = d.stats().aged;
        let bg = d.submit(Kind::Write { fua: false, preflush: false }, 3001, 0, Data::Bytes(vec![7; 512]), Background).map_err(e)?;
        let reads: Vec<Ticket> = (1..=8u64).map(|i| read(i * 2)).collect::<Result<_, _>>()?;
        sched::sleep_ns(2 * age + MS);
        r.resume();
        first.wait().map_err(e)?;
        bg.wait().map_err(e)?;
        for t in reads {
            t.wait().map_err(e)?;
        }
        let calls = r.calls();
        let at = calls.iter().position(|c| matches!(c.op, CallOp::Write { .. }) && c.lba == 3001);
        kassert!(calls.len() == 10, "calls: {calls:?}");
        kassert!(at == Some(1), "the aged write must go right after the held read, before 8 queued reads: {calls:?}");
        kassert!(d.stats().aged > aged_before, "picked as aged (stats {} before {aged_before})", d.stats().aged);
        Ok(())
    })();
    close(&r, &d)?;
    res
}
ktest!("block.ram.starve_aged", Safe, ram_starve_aged);
