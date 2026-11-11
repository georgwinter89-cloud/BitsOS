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
    let (runs, cleared, heap_fails) = (mm_core::reclaim::HEAP_RUNS.get() - runs0, mm_core::reclaim::HEAP_CLEARED.get() - cleared0, mm_core::report::GLOBAL.heap_alloc_fails.load(Ordering::Relaxed) - fails0);
    klog::event!(Info, "ext4", "ktest.download_pressure", total_mb = total >> 20, cached_mb = cached >> 20, free_mb_before = free_before >> 20, fill_nomem = fill_nomem,
        stream_mb = r(&k.stream_mb), stream_err = r(&k.stream_err), small_files = r(&k.small_files), small_err = r(&k.small_err),
        heap_ok = r(&k.heap_ok), heap_fail = r(&k.heap_fail), heap_fail_beside_cache = r(&k.heap_fail_beside_cache), spin_ok = r(&k.spin_ok), spin_fail = r(&k.spin_fail),
        spawned = r(&k.spawned), spawn_fail = r(&k.spawn_fail), heap_null_all = heap_fails, heap_runs = runs, heap_cleared = cleared,
        cached_mb_end = vfs::reclaim::file_pages() >> 8, free_mb_end = frames.stats().free_bytes >> 20, unjoined = unjoined,
        dirty_peak_mb = dirty_peak >> 8, throttled = crate::file::DIRTY_THROTTLED.get());
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

/// Round 5: a small writer beside a big one that keeps the dirty pages over the limit. Its 100-byte writes wait at
/// most in proportion to what they dirty and never cost a journal commit and a device flush each (before: every one
/// of them did, a Steam config write crawled exactly during a download).
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
    let disk = block::register(rd, block::DiskKind::Ram).map_err(e)?;
    let r = (|| -> TestResult {
        let ns = Arc::new(vfs::MountNs::new_empty());
        let (_v, _m) = mount::mount_disk(disk.clone(), 0, (BAL_DISK / 512) as u64, &ns, "/x", Options::default()).map_err(e)?;
        #[cfg(not(bitsos_no_ktests))]
        let _limit = vfs::pagecache::dirty_limit_for_test(16 << 8);
        let stop = Arc::new(AtomicBool::new(false));
        let big = {
            let (ns, stop) = (ns.clone(), stop.clone());
            sched::spawn("fs-ktest-balbig", sched::Class::Normal, move || {
                let chunk = vec![0xBBu8; 4 << 20];
                let c = Ctx::kernel(&ns);
                let Ok(f) = c.open(None, b"/x/big", O_RDWR | O_CREAT, 0o600) else { return };
                let mut at = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let _ = f.pwrite(at % (128 << 20), &chunk);
                    at += chunk.len() as u64;
                }
            })
            .map_err(e)?
        };
        let mut waited = 0;
        while !vfs::pagecache::dirty_over_limit() && waited < 500 {
            sched::sleep_ns(10_000_000);
            waited += 1;
        }
        let c = Ctx::kernel(&ns);
        let small = c.open(None, b"/x/small", O_RDWR | O_CREAT, 0o600).map_err(e)?;
        let (commits0, t0) = (crate::journal::commits(), now());
        let mut over = 0;
        for i in 0..SMALL_WRITES {
            over += u32::from(vfs::pagecache::dirty_over_limit());
            small.pwrite((i % 64) * 100, &[i as u8; 100]).map_err(e)?;
            sched::sleep_ns(1_000_000);
        }
        let (commits, took_ms) = (crate::journal::commits() - commits0, now().saturating_sub(t0) / 1_000_000);
        stop.store(true, Ordering::Release);
        big.join();
        klog::event!(Info, "ext4", "ktest.dirty_balance", writes = SMALL_WRITES, over_limit = over, commits = commits, took_ms = took_ms);
        kassert!(over >= 8, "the big writer did not keep the dirty pages over the limit ({over} of {SMALL_WRITES})");
        // The flusher's passes commit (a few per second at most here); before round 5 every small write over the
        // limit cost one.
        kassert!(commits * 4 < u64::from(over), "{commits} commits during {over} small writes over the limit");
        kassert!(took_ms < 20_000, "{SMALL_WRITES} small writes took {took_ms} ms");
        Ok(())
    })();
    block::unregister(name);
    r
}

const BAL_DISK: usize = 256 << 20;
const SMALL_WRITES: u64 = 200;

ktest!("fs.ext4.dirty_balance", QemuOnly, dirty_balance);
