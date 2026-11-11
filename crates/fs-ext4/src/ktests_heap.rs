//! FS-STALL, the Mini-PC's panic (`memory allocation of 75600 bytes failed`, 20 GB of 24 GB clean page cache):
//! kernel heap allocations must take clean page cache back instead of failing.
//!
//! - `fs.ext4.heap_pressure` (QemuOnly: it fills the machine's memory for a moment; run it with a small RAM, e.g.
//!   `cargo xtask ktest fs.ext4.heap_pressure -- -m 768M`, or a large one, `-m 4G`): the page cache is filled with
//!   clean pages of 1500 small ext4 files and one large one (holes of sparse files, read: no disk I/O needed), a thread writes back another file meanwhile, then the
//!   heap is asked for half of the available memory in 75600-byte blocks, faster than kswapd frees. Before
//!   FS-STALL the heap never took cache back (an infallible allocation of the kernel panicked there); now at most
//!   1 % may fail (memory in pieces between the cache's single frames: V2 cannot move pages, it evicts the ones in
//!   a block's way, `Buddy::reclaim_for_heap`). The blocks are asked for fallibly here, so a failure is a red test,
//!   not a panic.

use crate::mkfs::{self, MkfsOptions};
use crate::{mount, MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

const BLOCK: usize = 75_600;
const SMALL_FILES: usize = 1500;
const SMALL: u64 = 128 << 10;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn heap_pressure() -> TestResult {
    let Some(frames) = mm_core::frames() else { return Err("no frame allocator".into()) };
    // Review R1/R2: never reclaim for the heap with a spin lock held (reclaim frees into the grace queue under
    // `mm.grace` and takes cache index locks; the holder would spin on itself) or with interrupts off.
    kassert!(mm_core::reclaim::context_allows(), "a plain kernel thread may reclaim");
    {
        let l = ksync::SpinLock::named("fs-ktest.heap.spin", ());
        let _g = l.lock();
        kassert!(!mm_core::reclaim::context_allows(), "reclaim allowed under a spin lock");
    }
    let total = frames.stats().total_bytes;
    let dev = Arc::new(MemDev::new(vec![0u8; 32 << 20]));
    let mut o = MkfsOptions::new(b"heap", [7; 16], [9; 16], vfs::inode::now());
    o.zeroed = true;
    mkfs::format(&*dev, &o).map_err(e)?;
    let ns = Arc::new(vfs::MountNs::new_empty());
    let (_v, _m) = mount::mount_dev(dev, None, &ns, "/x", Options::default()).map_err(e)?;
    let c = Ctx::kernel(&ns);
    let big = c.open(None, b"/x/sparse", O_RDWR | O_CREAT, 0o600).map_err(e)?;
    c.truncate(None, b"/x/sparse", total * 2).map_err(e)?;
    // Write-back beside it: the flusher's path of the crash.
    // Started before the cache fills (a kernel stack does not reclaim cache yet: a spawn then fails).
    let stop = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let writer = {
        let (ns, stop, go) = (ns.clone(), stop.clone(), go.clone());
        sched::spawn("fs-ktest-heapwb", sched::Class::Normal, move || {
            let chunk = vec![0x5Au8; 256 << 10];
            while !go.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                sched::sleep_ns(1_000_000);
            }
            let c = Ctx::kernel(&ns);
            let Ok(f) = c.open(None, b"/x/dirty", O_RDWR | O_CREAT, 0o600) else { return };
            let mut i = 0u64;
            while !stop.load(Ordering::Acquire) {
                let _ = f.pwrite((i % 32) * chunk.len() as u64, &chunk);
                if let Ok(l) = ns.lookup("/x/dirty") {
                    let _ = l.inode.sync(SyncKind::Background);
                }
                i += 1;
            }
        })
        .map_err(e)?
    };
    // Many small files too (review item 4: a cache far larger than any window, spread over many caches): sparse,
    // so reading them fills the cache without the volume's space.
    let mut small = vec![0u8; SMALL as usize];
    for i in 0..SMALL_FILES {
        let p = format!("/x/s{i}");
        let f = c.open(None, p.as_bytes(), O_RDWR | O_CREAT, 0o600).map_err(|x| format!("open {p}: {x}"))?;
        c.truncate(None, p.as_bytes(), SMALL).map_err(|x| format!("truncate {p}: {x}"))?;
        f.pread(0, &mut small).map_err(|x| format!("read {p}: {x} (free {} MiB)", frames.stats().free_bytes >> 20))?;
    }
    drop(small);
    // Clean cache over most of memory: every page read is a hole of the file.
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0u64;
    let mut fill_nomem = 0u64;
    while at < total + total / 2 {
        // A read that finds no frame is counted and asked again (not this test's question; reported).
        let mut tries = 0;
        while let Err(x) = big.pread(at, &mut buf) {
            fill_nomem += 1;
            tries += 1;
            if tries > 100 {
                return Err(format!("read the large file at {} MiB: {x} 100 times (cache {} MiB, free {} MiB)", at >> 20, vfs::reclaim::file_pages() >> 8, frames.stats().free_bytes >> 20));
            }
            sched::sleep_ns(5_000_000);
        }
        at += buf.len() as u64;
    }
    let cached = vfs::reclaim::file_pages() * 4096;
    let free = frames.stats().free_bytes;
    go.store(true, Ordering::Release);
    if cached <= total / 2 {
        stop.store(true, Ordering::Release);
        writer.join();
    }
    kassert!(cached > total / 2, "the page cache holds only {} MiB of {} MiB", cached >> 20, total >> 20);
    let (runs0, rescued0) = (mm_core::reclaim::HEAP_RUNS.get(), mm_core::reclaim::HEAP_RESCUED.get());
    // Half of what is available (free plus the clean cache): the rest of the machine (tmpfs, the kernel) is not.
    let available = vfs::reclaim::mem_available_bytes();
    let want = (available / 2) as usize / BLOCK;
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut failed = 0usize;
    let mut failed_beside_cache = 0usize;
    // The list is a heap block too (85 KB): counted like the others, asked again after kswapd had a turn.
    let mut tries = 0;
    while held.try_reserve_exact(want).is_err() {
        failed += 1;
        tries += 1;
        if tries > 50 {
            stop.store(true, Ordering::Release);
            writer.join();
            return Err("no room for the list of blocks".into());
        }
        sched::sleep_ns(10_000_000);
    }
    for k in 0..want {
        let mut b: Vec<u8> = Vec::new();
        if b.try_reserve_exact(BLOCK).is_err() {
            // Review item 4's question: did it fail while clean cache was there to give (a tenth of memory)?
            if vfs::reclaim::file_pages() * 4096 > total / 10 {
                failed_beside_cache += 1;
            }
            if failed == 0 {
                let r = vfs::reclaim::stats();
                klog::event!(Info, "ext4", "ktest.heap_first_fail", block = k, cached_mb = vfs::reclaim::file_pages() >> 8, free_kb = frames.stats().free_bytes >> 10, reclaimed = r.pages, scanned = r.scanned, busy = r.busy_caches, cleared = mm_core::reclaim::HEAP_CLEARED.get(), heap_runs = mm_core::reclaim::HEAP_RUNS.get(), frame_pages = vfs::reclaim::FRAME_PAGES.get(), grace_waiting = mm_core::grace::waiting());
            }
            failed += 1;
            continue;
        }
        b.resize(BLOCK, k as u8);
        held.push(b);
    }
    stop.store(true, Ordering::Release);
    writer.join();
    let got = held.len();
    let (cached_end, available_end) = (vfs::reclaim::file_pages() * 4096, vfs::reclaim::mem_available_bytes());
    drop(held);
    let (runs, rescued) = (mm_core::reclaim::HEAP_RUNS.get() - runs0, mm_core::reclaim::HEAP_RESCUED.get() - rescued0);
    klog::event!(Info, "ext4", "ktest.heap_pressure", total_mb = total >> 20, cached_mb = cached >> 20, free_mb_before = free >> 20, available_mb = available >> 20, cached_mb_end = cached_end >> 20, available_mb_end = available_end >> 20, blocks = got, wanted = want, failed = failed, failed_beside_cache = failed_beside_cache, fill_nomem = fill_nomem, frame_pages = vfs::reclaim::FRAME_PAGES.get(), cleared = mm_core::reclaim::HEAP_CLEARED.get(), heap_reclaims = runs, heap_rescued = rescued);
    // Judged on the failures while a tenth of memory or more was still clean cache: there the heap must take it
    // back (the Mini-PC's case: 20 GB of cache, a 75600-byte block failed). At 1 % of the blocks: a block now and
    // then meets memory in pieces it cannot clear in its 20 ms. Failures after the cache is used up are memory in
    // pieces between kernel objects (no page migration in V2), reported, not judged.
    kassert!(failed_beside_cache * 100 <= want, "{failed_beside_cache} heap blocks of {BLOCK} bytes failed beside clean cache ({failed} in all, {got} of {want} got)");
    Ok(())
}

ktest!("fs.ext4.heap_pressure", QemuOnly, heap_pressure);
