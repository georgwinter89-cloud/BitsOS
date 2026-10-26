//! FS-13 (preload) in-kernel tests. Class Safe and rerunnable: each has its own RAM disk (a `DiskFile` page
//! cache over it stands in for a file), joins its threads, and never touches the record or the switches.
//! - `fs.preload.yield`: a preload pass reads 64 separate 4-page ranges from a disk that takes 20 ms per call;
//!   four Interactive readers submitted meanwhile (three reads each, far apart: they queue behind the pass's
//!   read and each other) are answered within the FS-12 limit (2 s, functional; the times are in the event
//!   `vfs`/`preload.yield_test`), the pass paused for them at least once (`yields` > 0), went on and every page
//!   ends cached and clean;
//! - `fs.preload.stop`: a pass stops at its page budget; with a free-memory bound above free memory, switched
//!   off, or with a clear pending it reads nothing (Memory, Off, Cleared);
//! - `fs.preload.sys`: `/sys/bitsos/preload_status` reads one line and is 0444, an open of it for writing is
//!   EACCES (Linux kernfs for a read-only attribute); bad values for `preload`, `preload_max_mb`,
//!   `preload_clear` are EINVAL; `all` is taken for `preload_max_mb` and read back as `all` (FS-13b); the
//!   current values written back are taken.

use crate::preload::{self, Budget, Stop};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use block::bdev::DiskFile;
use block::{Disk, DiskKind, Dispatch, RamDisk};
use ksync::SpinLock;
use ktest::{TestResult, kassert, ktest};
use sched::Class;
use uapi::errno::*;
use vfs::preload::Ranges;

const MS: u64 = 1_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn until(ms: u64, f: impl Fn() -> bool) -> bool {
    let end = time::now_ns() + ms * MS;
    while time::now_ns() < end {
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
    d.stop();
    kassert!(until(5000, || d.worker_done()), "the dispatcher thread did not end");
    Ok(())
}

fn all(pages: u64) -> Ranges {
    let mut r = Ranges::new();
    r.add(0, pages);
    r
}

type PassOut = Option<(u64, u64, Option<Stop>, u64)>;

/// 64 ranges of 4 pages, 16 pages apart (farther than `GAP`: each is its own device read).
fn spread() -> Ranges {
    let mut r = Ranges::new();
    for k in 0..64u64 {
        r.add(k * 16, k * 16 + 4);
    }
    r
}

fn preload_yield() -> TestResult {
    let (r, d) = disk("fs13-preload", 8192)?;
    let res = (|| -> TestResult {
        r.set_delay_ns(20 * MS);
        let cache = Arc::new(DiskFile::page_cache(d.clone()));
        let want = spread();
        let pages = want.pages();
        let out: Arc<SpinLock<PassOut>> = Arc::new(SpinLock::new(None));
        let (o, c) = (out.clone(), cache.clone());
        let h = sched::spawn("fs13-preload", Class::Background, move || {
            let mut b = Budget::new(u64::MAX, 0, false);
            let res = preload::prefetch_ranges(&c, &spread(), &mut b);
            *o.lock() = Some((b.pages, b.yields, if res.is_ok() { b.stopped } else { Some(Stop::Off) }, time::now_ns()));
        })
        .map_err(e)?;
        kassert!(until(10_000, || cache.pages() >= 16), "the preload did not start reading");
        // Interactive readers on blocks the preload never reads (64 mod 128: between its ranges), far apart (no
        // merging): while one is served
        // the others are queued, so the pass sees foreground requests waiting before its next read.
        let lat: Arc<SpinLock<Vec<Result<u64, String>>>> = Arc::new(SpinLock::new(Vec::new()));
        let mut readers = Vec::new();
        for k in 0..4u64 {
            let (l, dd) = (lat.clone(), d.clone());
            readers.push(
                sched::spawn("fs13-reader", Class::Interactive, move || {
                    let mut buf = vec![0u8; 512];
                    for n in 0..3u64 {
                        let t0 = time::now_ns();
                        let r = dd.read(64 + 128 * (50 + k * 3 + n), &mut buf, block::prio::current()).map(|_| time::now_ns() - t0).map_err(e);
                        l.lock().push(r);
                    }
                })
                .map_err(e)?,
            );
        }
        for h in readers {
            kassert!(h.join_timeout(10_000 * MS), "an Interactive reader did not end in 10 s");
        }
        let read_done = time::now_ns();
        let running = out.lock().is_none();
        let lats = core::mem::take(&mut *lat.lock());
        kassert!(lats.len() == 12, "{} read results", lats.len());
        let mut latency = 0;
        for r in lats {
            latency = latency.max(r?);
        }
        kassert!(h.join_timeout(30_000 * MS), "the preload pass did not end in 30 s");
        let (got, yields, stopped, end) = out.lock().take().ok_or("no pass result")?;
        klog::event!(Info, "vfs", "preload.yield_test", latency_us = latency / 1000, yields = yields, pages = got, during = running, after_read_ms = end.saturating_sub(read_done) / MS);
        kassert!(running, "the pass was over before the Interactive reads ended (nothing tested)");
        kassert!(latency <= 2000 * MS, "an Interactive read took {} ms during the preload (limit 2000)", latency / MS);
        kassert!(yields > 0, "the pass never paused for the queued Interactive reads");
        kassert!(stopped.is_none(), "the pass ended with {stopped:?}");
        kassert!(got == pages && cache.pages() == pages, "inserted {got}, cached {} of {pages}", cache.pages());
        for &(a, b) in want.as_slice() {
            kassert!((a..b).all(|p| cache.cached(p).is_some()), "pages {a}..{b} not cached");
        }
        kassert!(cache.dirty_pages() == 0, "preloaded pages are dirty");
        Ok(())
    })();
    close(&r, &d)?;
    res
}
ktest!("fs.preload.yield", Safe, preload_yield);

fn preload_stop() -> TestResult {
    let (r, d) = disk("fs13-stop", 8192)?;
    let res = (|| -> TestResult {
        let cache = DiskFile::page_cache(d.clone());
        // One read may bring a whole window (at most 256 pages) past the budget; the next one is not made.
        let mut b = Budget::new(10, 0, false);
        preload::prefetch_ranges(&cache, &all(1024), &mut b).map_err(e)?;
        kassert!(b.stopped == Some(Stop::Budget), "stopped {:?}", b.stopped);
        kassert!(b.pages >= 10 && b.pages < 10 + 256 && cache.pages() == b.pages, "budget 10: inserted {}, cached {}", b.pages, cache.pages());
        let mut m = Budget::new(u64::MAX, u64::MAX, false);
        let before = cache.pages();
        preload::prefetch_ranges(&cache, &all(1024), &mut m).map_err(e)?;
        kassert!(m.stopped == Some(Stop::Memory) && m.pages == 0 && cache.pages() == before, "memory bound: {:?}, {} pages", m.stopped, m.pages);
        for (why, off, cleared) in [(Stop::Off, (|| true) as fn() -> bool, (|| false) as fn() -> bool), (Stop::Cleared, || false, || true)] {
            let mut x = Budget::new(u64::MAX, 0, false);
            x.off = off;
            x.cleared = cleared;
            preload::prefetch_ranges(&cache, &all(1024), &mut x).map_err(e)?;
            kassert!(x.stopped == Some(why) && x.pages == 0 && x.reads == 0 && cache.pages() == before, "{why:?}: stopped {:?}, {} pages", x.stopped, x.pages);
        }
        Ok(())
    })();
    close(&r, &d)?;
    res
}
ktest!("fs.preload.stop", Safe, preload_stop);

fn preload_sys() -> TestResult {
    let ns = vfs::root_ns();
    let line = ns.read_file("/sys/bitsos/preload_status", 4096).map_err(e)?;
    let line = String::from_utf8_lossy(&line);
    kassert!(line.starts_with("state=") && line.contains(" enabled=") && line.ends_with('\n') && line.lines().count() == 1, "status: {line:?}");
    let sw = ns.read_file("/sys/bitsos/preload", 64).map_err(e)?;
    kassert!(sw == b"on\n" || sw == b"off\n", "preload: {sw:?}");
    let mb = ns.read_file("/sys/bitsos/preload_max_mb", 64).map_err(e)?;
    let mb = String::from_utf8_lossy(&mb);
    kassert!(mb.trim() == "all" || mb.trim().parse::<u64>().is_ok(), "preload_max_mb: {mb:?}");
    kassert!(line.contains(" floor_mb="), "status without floor_mb: {line:?}");
    // The pass's floor: a quarter of RAM, at least the high watermark; the status shows it once a pass ran (0
    // before).
    let shown = line.split_whitespace().find_map(|w| w.strip_prefix("floor_mb=")).and_then(|v| v.parse::<u64>().ok());
    if let Some(total) = mm_core::frames().map(|f| f.stats().total_bytes) {
        let floor = crate::preload::memory_floor();
        let high = mm::reclaim::watermarks().high.saturating_mul(4096);
        kassert!(floor >= total / 4 && floor >= high && floor == crate::preload::memory_floor_for(total, high), "floor {floor} for RAM {total}, high {high}");
        kassert!(shown == Some(0) || shown == Some(floor >> 20), "status floor_mb {shown:?}, floor {} MiB", floor >> 20);
    }
    let c = vfs::ops::Ctx::kernel(&ns);
    let write = |path: &[u8], v: &[u8]| -> Result<usize, i64> {
        let f = c.open(None, path, vfs::abi::O_WRONLY, 0)?;
        f.write(v)
    };
    let st = c.open(None, b"/sys/bitsos/preload_status", vfs::abi::O_WRONLY, 0).err();
    kassert!(st == Some(EACCES), "preload_status opened for writing: {st:?}");
    let mode = c.stat(None, b"/sys/bitsos/preload_status", true).map(|a| a.mode & 0o777).map_err(e)?;
    kassert!(mode == 0o444, "preload_status mode {mode:o}");
    for (p, v) in [(&b"/sys/bitsos/preload"[..], &b"maybe"[..]), (b"/sys/bitsos/preload_max_mb", b"lots"), (b"/sys/bitsos/preload_max_mb", b"99999999999"), (b"/sys/bitsos/preload_clear", b"2")] {
        let r = write(p, v);
        kassert!(r == Err(EINVAL), "{} <- {}: {r:?}", String::from_utf8_lossy(p), String::from_utf8_lossy(v));
    }
    // FS-13b: `all` (any case) is taken and read back as `all`; then the value from before goes back.
    kassert!(write(b"/sys/bitsos/preload_max_mb", b"ALL\n").is_ok(), "writing all");
    let now = ns.read_file("/sys/bitsos/preload_max_mb", 64).map_err(e)?;
    kassert!(now == b"all\n", "preload_max_mb after all: {:?}", String::from_utf8_lossy(&now));
    kassert!(write(b"/sys/bitsos/preload_max_mb", mb.trim().as_bytes()).is_ok(), "writing preload_max_mb back");
    kassert!(write(b"/sys/bitsos/preload", &sw[..sw.len() - 1]).is_ok(), "writing preload back");
    Ok(())
}
ktest!("fs.preload.sys", Safe, preload_sys);
