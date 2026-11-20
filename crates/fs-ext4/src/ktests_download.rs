//! FS-STALL round 4: generation 100 reset itself on the Mini-PC during Steam's X4 download (page cache 22 GB of
//! 24 GB, MemFree ~60 MB, thousands of heap failures). This puts a download-like load on ext4 through the block
//! layer beside a page cache that fills memory, and judges that the machine keeps working.
//!
//! - `fs.ext4.download_pressure` (QemuOnly, e.g. `-- -m 4G -smp 8`): clean cache over most of memory (holes of a
//!   sparse file, read in scattered order), then for `RUN_S` seconds at once: a streaming writer (4 MiB writes, fsync
//!   every GiB), a small-
//!   file writer (create, write, fsync, unlink), heap churn in thread context (75600-byte blocks), heap blocks
//!   asked for under a spin lock (a network driver's receive buffers: they cannot reclaim), and a thread spawner
//!   (kernel stacks). Every worker must keep making progress, no file call may fail, dirty pages stay bounded;
//!   failures are counted and logged (`ktest.download_pressure`), and those beside clean cache are judged.
//! - `fs.ext4.dirty_balance` (QemuOnly): round 5, small writes beside a big writer that is over the dirty limit
//!   (`dirty_balance`).

use crate::mkfs::{self, MkfsOptions};
use crate::{mount, MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

const RUN_S: u64 = 40;
const DISK: usize = 512 << 20;
const DIRTY_LIMIT_PAGES: u64 = 64 << 8;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

#[derive(Default)]
struct Counts {
    stream_mb: AtomicU64,
    stream_err: AtomicU64,
    small_files: AtomicU64,
    small_err: AtomicU64,
    heap_ok: AtomicU64,
    heap_fail: AtomicU64,
    heap_fail_beside_cache: AtomicU64,
    spin_ok: AtomicU64,
    spin_fail: AtomicU64,
    spawned: AtomicU64,
    spawn_fail: AtomicU64,
}

fn beside_cache(total: u64) -> bool {
    vfs::reclaim::file_pages() * 4096 > total / 10
}

fn download_pressure() -> TestResult {
    let Some(frames) = mm_core::frames() else { return Err("no frame allocator".into()) };
    let total = frames.stats().total_bytes;
    let name = "ext4dl";
    block::unregister(name);
    let img = {
        // One block of memory for the disk: asked for fallibly (a machine without it is a red test, not a panic).
        let mut raw: Vec<u8> = Vec::new();
        raw.try_reserve_exact(DISK).map_err(|_| format!("no {} MiB in one piece for the disk", DISK >> 20))?;
        raw.resize(DISK, 0);
        let dev = MemDev::new(raw);
        let mut o = MkfsOptions::new(b"dl", [3; 16], [4; 16], vfs::inode::now());
        o.zeroed = true;
        mkfs::format(&dev, &o).map_err(e)?;
        dev.into_image()
    };
    let rd = Arc::new(block::RamDisk::from_image(name, 512, img, false));
    let disk = block::register(rd.clone(), block::DiskKind::Ram).map_err(e)?;
    let r = run(&disk, total, frames);
    block::unregister(name);
    r
}

fn run(disk: &Arc<block::Disk>, total: u64, frames: &dyn mm_core::FrameAlloc) -> TestResult {
    let ns = Arc::new(vfs::MountNs::new_empty());
    let (vol, _m) = mount::mount_disk(disk.clone(), 0, (DISK / 512) as u64, &ns, "/x", Options::default()).map_err(e)?;
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/x/small", 0o755).map_err(e)?;
    let sparse = c.open(None, b"/x/sparse", O_RDWR | O_CREAT, 0o600).map_err(e)?;
    c.truncate(None, b"/x/sparse", total * 2).map_err(e)?;
    let stop = Arc::new(AtomicBool::new(false));
    let k = Arc::new(Counts::default());
    // Workers first: a spawn after the fill is part of what is judged, not the setup.
    let mut workers = Vec::new();
    let go = Arc::new(AtomicBool::new(false));
    let spawn = |what: &'static str, f: alloc::boxed::Box<dyn FnOnce() + Send>| sched::spawn(what, sched::Class::Normal, f).map_err(e);
    {
        let (ns, stop, k, go) = (ns.clone(), stop.clone(), k.clone(), go.clone());
        workers.push(spawn("fs-ktest-dlstream", alloc::boxed::Box::new(move || {
            let chunk = vec![0xD1u8; 4 << 20];
            wait_go(&go, &stop);
            let c = Ctx::kernel(&ns);
            let Ok(f) = c.open(None, b"/x/stream", O_RDWR | O_CREAT, 0o600) else { return };
            let mut at = 0u64;
            while !stop.load(Ordering::Acquire) {
                match f.pwrite(at % (384 << 20), &chunk) {
                    Ok(_) => {
                        k.stream_mb.fetch_add(4, Ordering::Relaxed);
                    }
                    Err(_) => {
                        k.stream_err.fetch_add(1, Ordering::Relaxed);
                    }
                }
                at += chunk.len() as u64;
                if at % (1 << 30) == 0 && ns.lookup("/x/stream").and_then(|l| l.inode.sync(SyncKind::Fsync)).is_err() {
                    k.stream_err.fetch_add(1, Ordering::Relaxed);
                }
            }
        }))?);
    }
    {
        let (ns, stop, k, go) = (ns.clone(), stop.clone(), k.clone(), go.clone());
        workers.push(spawn("fs-ktest-dlsmall", alloc::boxed::Box::new(move || {
            let data = vec![0x5Cu8; 4096];
            wait_go(&go, &stop);
            let c = Ctx::kernel(&ns);
            let mut i = 0u64;
            while !stop.load(Ordering::Acquire) {
                let p = format!("/x/small/f{}", i % 2000);
                let ok = (|| -> Result<(), i64> {
                    let f = c.open(None, p.as_bytes(), O_RDWR | O_CREAT, 0o600)?;
                    f.pwrite(0, &data)?;
                    ns.lookup(&p)?.inode.sync(SyncKind::Fsync)?;
                    if i % 4 == 3 {
                        c.unlink(None, p.as_bytes())?;
                    }
                    Ok(())
                })();
                match ok {
                    Ok(()) => k.small_files.fetch_add(1, Ordering::Relaxed),
                    Err(_) => k.small_err.fetch_add(1, Ordering::Relaxed),
                };
                i += 1;
            }
        }))?);
    }
    {
        let (stop, k, go) = (stop.clone(), k.clone(), go.clone());
        workers.push(spawn("fs-ktest-dlheap", alloc::boxed::Box::new(move || {
            let mut held: Vec<Vec<u8>> = Vec::new();
            let _ = held.try_reserve_exact(256);
            wait_go(&go, &stop);
            let mut i = 0usize;
            while !stop.load(Ordering::Acquire) {
                let mut b: Vec<u8> = Vec::new();
                if b.try_reserve_exact(75_600).is_ok() {
                    b.resize(75_600, i as u8);
                    k.heap_ok.fetch_add(1, Ordering::Relaxed);
                    if held.len() < 256 {
                        held.push(b);
                    } else {
                        held[i % 256] = b;
                    }
                } else {
                    k.heap_fail.fetch_add(1, Ordering::Relaxed);
                    if beside_cache(total) {
                        k.heap_fail_beside_cache.fetch_add(1, Ordering::Relaxed);
                    }
                }
                i += 1;
                if i % 64 == 0 {
                    sched::sleep_ns(1_000_000);
                }
            }
        }))?);
    }
    {
        let (stop, k, go) = (stop.clone(), k.clone(), go.clone());
        workers.push(spawn("fs-ktest-dlspin", alloc::boxed::Box::new(move || {
            let l = ksync::SpinLock::named("fs-ktest.dl.net", ());
            wait_go(&go, &stop);
            let mut i = 0usize;
            while !stop.load(Ordering::Acquire) {
                let size = if i % 4 == 0 { 65_536 } else { 9_216 };
                let got = {
                    let _g = l.lock();
                    let mut b: Vec<u8> = Vec::new();
                    b.try_reserve_exact(size).is_ok().then_some(b)
                };
                match got {
                    Some(b) => {
                        drop(b);
                        k.spin_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    None => {
                        k.spin_fail.fetch_add(1, Ordering::Relaxed);
                    }
                }
                i += 1;
                if i % 16 == 0 {
                    sched::sleep_ns(500_000);
                }
            }
        }))?);
    }
    {
        let (stop, k, go) = (stop.clone(), k.clone(), go.clone());
        workers.push(spawn("fs-ktest-dlspawn", alloc::boxed::Box::new(move || {
            wait_go(&go, &stop);
            while !stop.load(Ordering::Acquire) {
                match sched::spawn("fs-ktest-dlchild", sched::Class::Normal, || {}) {
                    Ok(h) => {
                        h.join();
                        k.spawned.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        k.spawn_fail.fetch_add(1, Ordering::Relaxed);
                    }
                }
                sched::sleep_ns(10_000_000);
            }
        }))?);
    }
    // Clean cache over most of memory, its pages read in a scattered order (a real cache: many files, read as
    // programs need them), so frames that reclaim frees lie apart, as on the Mini-PC (free memory in pieces).
    let pages = total * 3 / 2 / 4096;
    let mut buf = vec![0u8; 4096];
    let mut fill_nomem = 0u64;
    let mut i = 0u64;
    while i < pages {
        let at = (i.wrapping_mul(7919) % pages) * 4096;
        if sparse.pread(at, &mut buf).is_err() {
            fill_nomem += 1;
            if fill_nomem > 1000 {
                break;
            }
            sched::sleep_ns(5_000_000);
            continue;
        }
        i += 1;
    }
    let cached = vfs::reclaim::file_pages() * 4096;
    let free_before = frames.stats().free_bytes;
    let vol0 = ksync::stats::class("ext4.vol").map_or((0, 0, 0), |v| (v.acquired, v.contended, v.wait_ticks));
    let (runs0, cleared0, fails0) = (mm_core::reclaim::HEAP_RUNS.get(), mm_core::reclaim::HEAP_CLEARED.get(), mm_core::report::GLOBAL.heap_alloc_fails.load(Ordering::Relaxed));
    // The disk here cannot hold a fifth of memory: the limit is 64 MiB for this run.
    #[cfg(not(bitsos_no_ktests))]
    let _limit = vfs::pagecache::dirty_limit_for_test(DIRTY_LIMIT_PAGES);
    go.store(true, Ordering::Release);
    // The run, with a look every second: every worker must move.
    let t0 = now();
    let mut stuck = Vec::new();
    let mut last = snapshot(&k);
    let mut still = 0;
    let mut dirty_peak = 0u64;
    while now().saturating_sub(t0) < RUN_S * 1_000_000_000 {
        for _ in 0..10 {
            sched::sleep_ns(100_000_000);
            dirty_peak = dirty_peak.max(vfs::pagecache::dirty_pages_total());
        }
        let s = snapshot(&k);
        let moved = s.iter().zip(last.iter()).filter(|(a, b)| a != b).count();
        still = if moved == 0 { still + 1 } else { 0 };
        if still >= 5 {
            stuck.push(format!("nothing moved for 5 s at {} s: {s:?}", now().saturating_sub(t0) / 1_000_000_000));
            break;
        }
        last = s;
    }
    stop.store(true, Ordering::Release);
    let mut unjoined = 0;
    for w in &workers {
        if !w.join_timeout(30_000_000_000) {
            unjoined += 1;
        }
    }
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    // FS-W: what the volume lock cost the workers (maxima since boot: compare runs of this test alone).
    let lock = |name: &str| ksync::stats::class(name);
    let us = |t: u64| ksync::stats::ticks_to_ns(t).unwrap_or(0) / 1000;
    if let (Some(v), Some(c)) = (lock("ext4.vol"), lock("ext4.commit")) {
        klog::event!(Info, "ext4", "ktest.download_pressure.locks", vol_acquired = v.acquired - vol0.0, vol_contended = v.contended - vol0.1, vol_wait_ms = us(v.wait_ticks - vol0.2) / 1000,
            vol_wait_max_us = us(v.wait_max_ticks), vol_hold_max_us = us(v.hold_max_ticks), commit_hold_max_us = us(c.hold_max_ticks), commit_wait_max_us = us(c.wait_max_ticks));
    }
    let (runs, cleared, heap_fails) = (mm_core::reclaim::HEAP_RUNS.get() - runs0, mm_core::reclaim::HEAP_CLEARED.get() - cleared0, mm_core::report::GLOBAL.heap_alloc_fails.load(Ordering::Relaxed) - fails0);
    klog::event!(Info, "ext4", "ktest.download_pressure", total_mb = total >> 20, cached_mb = cached >> 20, free_mb_before = free_before >> 20, fill_nomem = fill_nomem,
        stream_mb = r(&k.stream_mb), stream_err = r(&k.stream_err), small_files = r(&k.small_files), small_err = r(&k.small_err),
        heap_ok = r(&k.heap_ok), heap_fail = r(&k.heap_fail), heap_fail_beside_cache = r(&k.heap_fail_beside_cache), spin_ok = r(&k.spin_ok), spin_fail = r(&k.spin_fail),
        spawned = r(&k.spawned), spawn_fail = r(&k.spawn_fail), heap_null_all = heap_fails, heap_runs = runs, heap_cleared = cleared,
        cached_mb_end = vfs::reclaim::file_pages() >> 8, free_mb_end = frames.stats().free_bytes >> 20, unjoined = unjoined,
        dirty_peak_mb = dirty_peak >> 8, throttled = crate::file::DIRTY_THROTTLED.get(), bdev_cache_copy_mb = crate::dev::COPY_BDEV_CACHE.sum() >> 20, bdev_cache_write_mb = crate::dev::COPY_BDEV_WRITE.sum() >> 20, dev_written_mb = disk.stats().write_bytes >> 20, map_lookups = crate::runs::LOOKUPS.get());
    // Dirty pages are bounded (the limit, with room for what is in flight: a 4 MiB write of each writer, a write-back
    // batch): reclaim cannot take them.
    kassert!(dirty_peak <= DIRTY_LIMIT_PAGES * 2, "dirty pages reached {} MiB, the limit is {} MiB", dirty_peak >> 8, DIRTY_LIMIT_PAGES >> 8);
    kassert!(crate::file::DIRTY_THROTTLED.get() > 0, "the limit never held a writer back");
    kassert!(stuck.is_empty(), "{stuck:?}");
    kassert!(unjoined == 0, "{unjoined} workers did not end");
    kassert!(r(&k.stream_mb) > 0 && r(&k.small_files) > 0 && r(&k.heap_ok) > 0 && r(&k.spin_ok) > 0 && r(&k.spawned) > 0, "a worker never got anywhere");
    kassert!(r(&k.stream_err) == 0 && r(&k.small_err) == 0, "file errors: stream {} small {}", r(&k.stream_err), r(&k.small_err));
    kassert!(r(&k.spawn_fail) == 0, "{} thread spawns failed beside clean cache", r(&k.spawn_fail));
    kassert!(r(&k.heap_fail_beside_cache) * 100 <= r(&k.heap_ok) + 1, "{} heap blocks failed beside clean cache", r(&k.heap_fail_beside_cache));
    // Under a spin lock the heap cannot reclaim; the reserve and kswapd must still serve it nearly always.
    kassert!(r(&k.spin_fail) * 100 <= r(&k.spin_ok) + 1, "{} of {} receive-buffer-like blocks failed under a spin lock", r(&k.spin_fail), r(&k.spin_ok) + r(&k.spin_fail));
    vol.sync_fs().map_err(e)?;
    Ok(())
}

fn wait_go(go: &AtomicBool, stop: &AtomicBool) {
    while !go.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
        sched::sleep_ns(5_000_000);
    }
}

fn snapshot(k: &Counts) -> [u64; 5] {
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    [r(&k.stream_mb), r(&k.small_files), r(&k.heap_ok) + r(&k.heap_fail), r(&k.spin_ok) + r(&k.spin_fail), r(&k.spawned) + r(&k.spawn_fail)]
}

ktest!("fs.ext4.download_pressure", QemuOnly, download_pressure);

/// Round 5: a small writer beside a big one that has the dirty pages over the limit. Its 100-byte writes wait at
/// most in proportion to what they dirty and never cost a journal commit and a device flush each (before: every one
/// of them did, a Steam config write crawled exactly during a download).
///
/// Since FS-W2 a big writer alone is over the limit only for moments: its own write-back hands its pages to the
/// device and the count falls at once. So the test no longer counts commits against the few small writes that
/// happened to meet such a moment; it has two parts:
/// - held: the device is paused and the big writer stops inside its own write-back with the count far over the
///   limit. Every small write finds the count over the limit and must return while the device is busy. A write
///   that writes back, commits or flushes (round 4 did all three) waits for the device and does not return.
/// - live: the device goes on, the big writer beside the small one. The commits follow the flusher's clock (a
///   kicked pass at most every `PASS_GAP_MS`, at most `PASS_COMMITS` commits each), however many small writes
///   there were.
fn dirty_balance() -> TestResult {
    let name = "ext4bal";
    block::unregister(name);
    let img = {
        let mut raw: Vec<u8> = Vec::new();
        raw.try_reserve_exact(BAL_DISK).map_err(|_| String::from("no room for the disk"))?;
        raw.resize(BAL_DISK, 0);
        let dev = MemDev::new(raw);
        let mut o = MkfsOptions::new(b"bal", [5; 16], [6; 16], vfs::inode::now());
        o.zeroed = true;
        mkfs::format(&dev, &o).map_err(e)?;
        dev.into_image()
    };
    let rd = Arc::new(block::RamDisk::from_image(name, 512, img, false));
    let disk = block::register(rd.clone(), block::DiskKind::Ram).map_err(e)?;
    let b = Arc::new(Bal::default());
    let mut big = None;
    let r = balance(&rd, disk, &b, &mut big);
    // After an early error: the device and the big writer go on and end.
    rd.resume();
    let ended = end_big(&b, &mut big);
    block::unregister(name);
    r?;
    kassert!(ended, "the big writer did not end");
    Ok(())
}

fn end_big(b: &Bal, big: &mut Option<sched::JoinHandle>) -> bool {
    b.stop.store(true, Ordering::Release);
    big.take().is_none_or(|h| h.join_timeout(30_000_000_000))
}

#[derive(Default)]
struct Bal {
    warm: AtomicBool,
    go: AtomicBool,
    stop: AtomicBool,
}

/// What a run of small writes saw.
#[derive(Default)]
struct Small {
    done: AtomicU64,
    /// Writes that found the dirty count over the limit before and after.
    over: AtomicU64,
    failed: AtomicU64,
}

/// `n` writes of 100 bytes into /x/small, `gap_ns` apart.
fn small_writes(ns: &vfs::MountNs, n: u64, gap_ns: u64, k: &Small) {
    let c = Ctx::kernel(ns);
    let Ok(f) = c.open(None, b"/x/small", O_RDWR, 0) else {
        k.failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    for i in 0..n {
        let before = vfs::pagecache::dirty_over_limit();
        if f.pwrite((i % 64) * 100, &[i as u8; 100]).is_err() {
            k.failed.fetch_add(1, Ordering::Relaxed);
        }
        k.over.fetch_add(u64::from(before && vfs::pagecache::dirty_over_limit()), Ordering::Relaxed);
        k.done.fetch_add(1, Ordering::Relaxed);
        if gap_ns > 0 {
            sched::sleep_ns(gap_ns);
        }
    }
}

fn until(ms: u64, f: impl Fn() -> bool) -> bool {
    let end = now() + ms * 1_000_000;
    while now() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(1_000_000);
    }
    f()
}

fn balance(rd: &block::RamDisk, disk: Arc<block::Disk>, b: &Arc<Bal>, big: &mut Option<sched::JoinHandle>) -> TestResult {
    let ns = Arc::new(vfs::MountNs::new_empty());
    let (_v, _m) = mount::mount_disk(disk.clone(), 0, (BAL_DISK / 512) as u64, &ns, "/x", Options::default()).map_err(e)?;
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let fsync = |p: &str| ns.lookup(p).and_then(|l| l.inode.sync(SyncKind::Fsync)).map_err(e);
    // Both files on the disk and in its cache first: what the writes below look up (bitmaps, the inodes' block)
    // must be in memory while the device is paused.
    ns.create_file_bytes(b"/x/small", 0o600, &[0u8; 6400]).map_err(e)?;
    fsync("/x/small")?;
    *big = Some({
        let (ns, b) = (ns.clone(), b.clone());
        sched::spawn("fs-ktest-balbig", sched::Class::Normal, move || {
            let chunk = vec![0xBBu8; 4 << 20];
            let c = Ctx::kernel(&ns);
            let Ok(f) = c.open(None, b"/x/big", O_RDWR | O_CREAT, 0o600) else { return };
            if f.pwrite(0, &chunk).is_err() || ns.lookup("/x/big").and_then(|l| l.inode.sync(SyncKind::Fsync)).is_err() {
                return;
            }
            b.warm.store(true, Ordering::Release);
            wait_go(&b.go, &b.stop);
            let mut at = chunk.len() as u64;
            while !b.stop.load(Ordering::Acquire) {
                let _ = f.pwrite(at % (128 << 20), &chunk);
                at += chunk.len() as u64;
            }
        })
        .map_err(e)?
    });
    kassert!(until(10_000, || b.warm.load(Ordering::Acquire)), "the big writer's first write and fsync did not end");

    // Held. No limit yet: the big writer dirties on (128 MiB at most, far under a fifth of memory) and nothing
    // reaches the paused device. With twice the limit dirty the limit is set: its next write finds the count over
    // it and writes its file back. The first batch (4 MiB) leaves the count and waits in the device; the rest stays
    // counted, far over the limit, until the device goes on.
    rd.pause();
    b.go.store(true, Ordering::Release);
    kassert!(until(10_000, || vfs::pagecache::dirty_pages_total() >= 2 * BAL_LIMIT_PAGES), "the big writer dirtied only {} pages", vfs::pagecache::dirty_pages_total());
    let throttled0 = crate::file::DIRTY_THROTTLED.get();
    #[cfg(not(bitsos_no_ktests))]
    let _limit = vfs::pagecache::dirty_limit_for_test(BAL_LIMIT_PAGES);
    let stopped = || crate::file::DIRTY_THROTTLED.get() > throttled0 && rd.holding() && vfs::pagecache::dirty_over_limit();
    kassert!(until(10_000, stopped), "the big writer did not stop in the paused device over the limit ({} pages dirty)", vfs::pagecache::dirty_pages_total());
    let held = Arc::new(Small::default());
    let (commits0, flushes0, t0) = (crate::journal::commits(), rd.flushes(), now());
    let w = {
        let (ns, held) = (ns.clone(), held.clone());
        sched::spawn("fs-ktest-balsmall", sched::Class::Normal, move || {
            // The first half asks for the flusher; after the wait its pass is at the paused device too.
            small_writes(&ns, HELD_WRITES / 2, 0, &held);
            sched::sleep_ns(HELD_WAIT_NS);
            small_writes(&ns, HELD_WRITES / 2, 0, &held);
        })
        .map_err(e)?
    };
    let returned = w.join_timeout(HELD_LIMIT_MS * 1_000_000);
    let (held_done, held_over, held_ms, still) = (r(&held.done), r(&held.over), now().saturating_sub(t0) / 1_000_000, stopped());
    let (held_commits, held_flushes) = (crate::journal::commits() - commits0, rd.flushes() - flushes0);
    rd.resume();
    let small_ended = returned || w.join_timeout(30_000_000_000);
    klog::event!(Info, "ext4", "ktest.dirty_balance.held", writes = HELD_WRITES, returned = held_done, over_limit = held_over, commits = held_commits, dev_flushes = held_flushes, took_ms = held_ms, held_to_the_end = still);

    // Live.
    let live = Small::default();
    let (commits0, dev0, t0) = (crate::journal::commits(), disk.stats().write_bytes, now());
    small_writes(&ns, SMALL_WRITES, 1_000_000, &live);
    let (commits, took_ms, dev_bytes) = (crate::journal::commits() - commits0, now().saturating_sub(t0) / 1_000_000, disk.stats().write_bytes - dev0);
    let big_ended = end_big(b, big);
    klog::event!(Info, "ext4", "ktest.dirty_balance", writes = SMALL_WRITES, over_limit = r(&live.over), commits = commits, took_ms = took_ms, dev_mb = dev_bytes >> 20);

    kassert!(returned, "{held_done} of {HELD_WRITES} small writes over the limit returned in {HELD_LIMIT_MS} ms while the device was busy with the big writer's data: such a write waits for the device, or for somebody who does");
    kassert!(still, "the device or the limit let the big writer go on before the small writes ended ({held_ms} ms)");
    kassert!(held_over == HELD_WRITES, "{held_over} of {HELD_WRITES} small writes found the dirty pages over the limit");
    kassert!(held_commits == 0 && held_flushes == 0, "{held_commits} commits and {held_flushes} device flushes while the device was paused");
    kassert!(dev_bytes > 0, "none of the big writer's data reached the device beside the small writes");
    // Nobody but the flusher commits here; before round 5 every small write over the limit did.
    let most = PASS_COMMITS * (took_ms / PASS_GAP_MS + 2);
    kassert!(commits <= most, "{commits} commits in {took_ms} ms beside {SMALL_WRITES} small writes: more than the flusher's passes make ({most})");
    kassert!(took_ms < 20_000, "{SMALL_WRITES} small writes took {took_ms} ms");
    kassert!(r(&held.failed) + r(&live.failed) == 0, "small writes failed: held {} live {}", r(&held.failed), r(&live.failed));
    kassert!(small_ended && big_ended, "a writer did not end (small {small_ended}, big {big_ended})");
    Ok(())
}

const BAL_DISK: usize = 256 << 20;
const BAL_LIMIT_PAGES: u64 = 16 << 8;
const SMALL_WRITES: u64 = 200;
const HELD_WRITES: u64 = 100;
/// Between the two halves of the held writes: longer than `PASS_GAP_MS`.
const HELD_WAIT_NS: u64 = 300_000_000;
/// Well under the 10 s after which a paused RAM disk goes on by itself.
const HELD_LIMIT_MS: u64 = 6_000;
/// `vfs-sys`: the shortest gap between two passes of the flusher that writers asked for (`KICK_GAP_NS`).
const PASS_GAP_MS: u64 = 200;
/// A pass commits a volume's journal at most this often (`Volume::commit_out` for the background pass).
const PASS_COMMITS: u64 = 3;

ktest!("fs.ext4.dirty_balance", QemuOnly, dirty_balance);
