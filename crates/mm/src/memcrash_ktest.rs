//! ktest `mm.memcrash` (its own boot, host/tests/mm/82-memcrash.sh: 2 GiB, 12 CPUs, three minutes; never on hardware):
//! the Mini-PC's crash of 13.11.2026 (generation 150, Steam downloading 60 GB) in small. A page cache that file
//! writes and reads keep full for minutes (dirty pages written back, clean ones reclaimed; one big file and 3000
//! small ones, each a cache of its own in reclaim's ring), a program with a few
//! hundred VMAs whose threads look up futex keys (`vma_at`, what `ProcSpace::key` asks) while others unmap, map and
//! fault in the same space (`mm.as` is waited for), and kernel threads that take and drop 12 and 28 KiB buffers: kernel
//! blocks of order 2 and 3 meet a memory that is full and in pieces.
//!
//! There the futex key copied the VMA list (22 KiB under `mm.as`), the copy's heap allocation gave up when its
//! patience was cut (others waited for `mm.as`) and the kernel panicked; beside it an unmap grew its frame list
//! through the heap and stood in reclaim behind `mm.grace`. Judged: no allocation failed (the buffers, the heap's
//! own count), every call of the program read right, nobody waited for `mm.grace` inside reclaim, and the numbers
//! of the crash's last minute are written out every 10 s (`mm ktest.memcrash_10s`) and at the end.

use crate::linux::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use ktest::{kassert, ktest, TestResult};
use mm_core::PAGE as P;

const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = 3;
const RO: u64 = 1;
const SEC: u64 = 1_000_000_000;
const RUN_S: u64 = 180;
/// The VMAs of the program: one page each with a page between, every other one read-only (they never merge).
const VMA_BASE: u64 = 0x40_0000_0000;
const ARENA: u64 = 0x48_0000_0000;
const HEAP_BASE: u64 = 0x50_0000_0000;
/// The VMA list's size the crash copied (22680 bytes): the program gets at least that many bytes of VMAs.
const LIST_BYTES: usize = 22_680;
/// Pages a churning thread unmaps, maps and writes per round.
const REGION: u64 = 64;
const REGIONS: u64 = 8;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// Wrong answers by part: futex keys, unmap/map/write of the arena, file writes, file reads.
    errors: [AtomicU64; 4],
    keys: AtomicU64,
    rounds: AtomicU64,
    writes: AtomicU64,
    reads: AtomicU64,
    blocks: AtomicU64,
    block_fails: AtomicU64,
    /// The longest futex-key lookup and buffer allocation since the last look (ns).
    key_ns: AtomicU64,
    block_ns: AtomicU64,
}

fn rng(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn since(t0: u64) -> u64 {
    time::now_ns().saturating_sub(t0)
}

/// A futex waiter's key: the VMA of one of the program's words (shared futexes ask for it on every wait).
fn keys(a: &AddressSpace, vmas: u64, s: &Shared, mut seed: u64) {
    while !s.stop.load(Relaxed) {
        let i = rng(&mut seed) % vmas;
        let addr = VMA_BASE + i * 2 * P + 8;
        let t = time::now_ns();
        let ok = a.vma_at(addr).is_some_and(|v| v.start <= addr && addr < v.end);
        s.key_ns.fetch_max(since(t), Relaxed);
        if !ok {
            s.errors[0].fetch_add(1, Relaxed);
        }
        s.keys.fetch_add(1, Relaxed);
        sched::sleep_ns(100_000);
    }
}

/// A thread of the program: one of its own two regions of the arena (another thread's unmap would make its writes
/// fail) unmapped, mapped again and every page written.
fn churn(a: &AddressSpace, s: &Shared, t: u64, mut seed: u64) {
    while !s.stop.load(Relaxed) {
        let va = ARENA + (t * 2 + rng(&mut seed) % 2) * REGION * P;
        if a.sys_munmap(va, REGION * P).is_err() || a.sys_mmap(va, REGION * P, RW, AP | MAP_FIXED, 0).is_err() {
            s.errors[1].fetch_add(1, Relaxed);
            continue;
        }
        for pg in 0..REGION {
            if a.write_u64(va + pg * P, pg).is_err() {
                s.errors[1].fetch_add(1, Relaxed);
            }
        }
        s.rounds.fetch_add(1, Relaxed);
        sched::sleep_ns(1_000_000);
    }
}

/// A download: the file written page after page from `first` on, written back every 256 pages.
fn writer(c: &vfs::pagecache::PageCache, first: u64, pages: u64, s: &Shared) {
    let mut pg = first;
    while !s.stop.load(Relaxed) {
        let v = pg.to_le_bytes();
        if c.write_at(pg * P, &v).is_err() || c.write_at(pg * P + P - 8, &v).is_err() {
            s.errors[2].fetch_add(1, Relaxed);
        }
        s.writes.fetch_add(1, Relaxed);
        pg = if pg + 1 >= pages { 0 } else { pg + 1 };
        if pg.is_multiple_of(256) && c.sync_all(vfs::pagecache::SyncKind::Fsync).is_err() {
            s.errors[2].fetch_add(1, Relaxed);
        }
    }
}

/// Small files beside the download (the Mini-PC's Steam wrote and read thousands): each is its own cache in
/// reclaim's ring, and every visit of the walk to one that nobody else holds went to the grace queue under `mm.grace`.
const SMALL_FILES: usize = 3000;
const SMALL_PAGES: u64 = 16;

type Cache = vfs::pagecache::PageCache;

/// A reader in a scattered order (what reclaim frees later lies apart): half of the reads in the download, half
/// in the small files.
fn reader(c: &Cache, pages: u64, small: &[Cache], s: &Shared, mut seed: u64) {
    let mut b = [0u8; 8];
    while !s.stop.load(Relaxed) {
        let r = rng(&mut seed);
        let (f, pg) = if r % 2 == 0 { (c, (r >> 1) % pages) } else { (&small[(r >> 1) as usize % small.len()], (r >> 20) % SMALL_PAGES) };
        if f.read_at(pg * P, &mut b).is_err() || u64::from_le_bytes(b) != pg {
            s.errors[3].fetch_add(1, Relaxed);
        }
        s.reads.fetch_add(1, Relaxed);
    }
}

/// Kernel buffers of 12 and 28 KiB (order 2 and 3 runs), an eighth of them held for a while.
fn buffers(s: &Shared, mut seed: u64) {
    let mut held: Vec<Vec<u8>> = Vec::new();
    if held.try_reserve_exact(64).is_err() {
        s.block_fails.fetch_add(1, Relaxed);
        return;
    }
    while !s.stop.load(Relaxed) {
        for size in [28_000usize, 12_000, 28_000, 22_680] {
            let mut b: Vec<u8> = Vec::new();
            let t = time::now_ns();
            if b.try_reserve_exact(size).is_err() {
                s.block_fails.fetch_add(1, Relaxed);
                continue;
            }
            s.block_ns.fetch_max(since(t), Relaxed);
            b.resize(size, 7);
            s.blocks.fetch_add(1, Relaxed);
            if rng(&mut seed).is_multiple_of(8) {
                if held.len() < 64 {
                    held.push(b);
                } else {
                    held[(seed >> 8) as usize % 64] = b;
                }
            }
        }
        sched::sleep_ns(2_000_000);
    }
}

const NW: usize = 17;
const WATCHED: [&str; NW] = [
    "mm.heap.patient_allocs", "mm.heap.patience_cut", "mm.reclaim.heap_runs", "vfs.reclaim.busy_caches", "vfs.reclaim.scanned",
    "mm.frames.boost_raised", "mm.frames.boost_cleared", "mm.grace.reclaim_handover", "mm.grace.reclaim_waits", "mm.alloc.nofail_rounds",
    "mm.oom.kills", "mm.tlb.gather_nobatch", "mm.heap.alloc_fail", "vfs.reclaim.pages", "mm.alloc.nofail_allocs", "mm.alloc.nofail_failed",
    "mm.grace.reclaim_overflow",
];

fn watched() -> [u64; NW] {
    let mut v = [0u64; NW];
    for (n, x) in klog::metrics::snapshot() {
        if let Some(i) = WATCHED.iter().position(|w| *w == n) {
            v[i] = x;
        }
    }
    v
}

fn metric(name: &str) -> u64 {
    klog::metrics::snapshot().find(|m| m.0 == name).map_or(0, |m| m.1)
}

fn memcrash() -> TestResult {
    let cpus = 1 + crate::tlb_ktest::aps().len();
    kassert!(cpus >= 8, "{cpus} CPUs: the crash's load needs many (host/tests/mm/82-memcrash.sh boots 12)");
    let Some(f) = mm_core::frames() else { return ktest::fail("no frame allocator") };
    let total = f.stats().total_bytes / P;
    kassert!(total <= (3u64 << 30) / P, "{} MiB: a pressure test for a small machine (82-memcrash.sh boots 2 GiB)", total * P >> 20);
    let fails0 = mm_core::report::GLOBAL.alloc_fails.load(Relaxed);
    let a = Arc::new(AddressSpace::with_fill(Fill::OnFault).map_err(e)?);
    a.set_thp_disabled(true);
    // The VMAs: as many as make the crash's list, every one written.
    let vmas = (LIST_BYTES / core::mem::size_of::<crate::Vma>()).max(256) as u64;
    for i in 0..vmas {
        let va = VMA_BASE + i * 2 * P;
        a.sys_mmap(va, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
        a.write_u64(va, i).map_err(e)?;
        if i % 2 == 1 {
            a.sys_mprotect(va, P, RO).map_err(e)?;
        }
    }
    let listed = a.vmas().len() as u64;
    kassert!(listed >= vmas, "{listed} VMAs listed, {vmas} mapped: they merged");
    // The program's heap: a tenth of memory, written.
    let heap = total / 10;
    a.sys_mmap(HEAP_BASE, heap * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for i in 0..heap {
        a.write_u64(HEAP_BASE + i * P, !i).map_err(e)?;
    }
    for r in 0..REGIONS {
        a.sys_mmap(ARENA + r * REGION * P, REGION * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    }
    // The file: twice memory, filled until memory is full, in a scattered order.
    let file = total * 2;
    let (cache, _pat) = crate::reclaim_ktest::pattern_file(file);
    let cache = Arc::new(cache);
    let mut small = Vec::new();
    small.try_reserve_exact(SMALL_FILES).map_err(e)?;
    for _ in 0..SMALL_FILES {
        small.push(crate::reclaim_ktest::pattern_file(SMALL_PAGES).0);
    }
    let small = Arc::new(small);
    let mut b = [0u8; 8];
    let fill = (f.stats().usable_free_bytes() / P).saturating_sub(crate::reclaim::watermarks().high);
    for i in 0..fill {
        let pg = i.wrapping_mul(7919) % file;
        cache.read_at(pg * P, &mut b).map_err(|x| format!("fill page {pg}: {x}"))?;
    }
    let s = Arc::new(Shared::default());
    let mut threads = Vec::new();
    let spawn = |name: &str, f: alloc::boxed::Box<dyn FnOnce() + Send>| sched::spawn_with(name, sched::Class::Normal, sched::SpawnOpts::default(), f).map_err(e);
    for t in 0..4u64 {
        let (a, s) = (a.clone(), s.clone());
        threads.push(spawn("mm-crash-keys", alloc::boxed::Box::new(move || keys(&a, vmas, &s, 0x9E37_79B9 + t)))?);
    }
    for t in 0..4u64 {
        let (a, s) = (a.clone(), s.clone());
        threads.push(spawn("mm-crash-churn", alloc::boxed::Box::new(move || churn(&a, &s, t, 0x51ED_2701 + t)))?);
    }
    for t in 0..2u64 {
        let (c, s) = (cache.clone(), s.clone());
        threads.push(spawn("mm-crash-writer", alloc::boxed::Box::new(move || writer(&c, t * file / 2, file, &s)))?);
    }
    for t in 0..4u64 {
        let (c, s) = (cache.clone(), s.clone());
        let small = small.clone();
        threads.push(spawn("mm-crash-reader", alloc::boxed::Box::new(move || reader(&c, file, &small, &s, 0xABCD_EF01 + t)))?);
    }
    for t in 0..4u64 {
        let s = s.clone();
        threads.push(spawn("mm-crash-buffers", alloc::boxed::Box::new(move || buffers(&s, 0x1234_5678 + t)))?);
    }
    let grace_lock = || ksync::stats::class("mm.grace").map_or((0, 0), |c| (c.wait_max_ticks, c.hold_max_ticks));
    let us = |t: u64| ksync::stats::ticks_to_ns(t).unwrap_or(0) / 1000;
    let t0 = time::now_ns();
    let first = watched();
    let mut last = first;
    for step in 1..=RUN_S / 10 {
        let at = t0 + step * 10 * SEC;
        sched::sleep_ns(at.saturating_sub(time::now_ns()));
        let now = watched();
        let d: Vec<u64> = now.iter().zip(last.iter()).map(|(n, l)| n - l).collect();
        last = now;
        let st = f.stats();
        let (gw, gh) = grace_lock();
        klog::event!(Info, "mm", "ktest.memcrash_10s", t = step * 10, free_mb = st.usable_free_bytes() >> 20, cache_mb = vfs::reclaim::file_pages() >> 8,
            dirty_mb = vfs::pagecache::dirty_pages_total() >> 8, keys = s.keys.load(Relaxed), rounds = s.rounds.load(Relaxed), writes = s.writes.load(Relaxed),
            blocks = s.blocks.load(Relaxed), block_fails = s.block_fails.load(Relaxed), key_ms = s.key_ns.swap(0, Relaxed) / 1_000_000, block_ms = s.block_ns.swap(0, Relaxed) / 1_000_000,
            patient = d[0], cut = d[1], heap_reclaim = d[2], busy_caches = d[3], scanned = d[4], boost_raised = d[5], boost_cleared = d[6],
            grace_handover = d[7], grace_waits = d[8], nofail = d[9], oom_kills = d[10], gather_nobatch = d[11], alloc_fail = d[12], reclaimed = d[13],
            nofail_allocs = d[14], nofail_failed = d[15], grace_overflow = d[16],
            regain_max_us = metric("mm.heap.regain_max_us"), regain_held_max_us = metric("mm.heap.regain_held_max_us"), grace_wait_max_us = us(gw), grace_hold_max_us = us(gh));
    }
    s.stop.store(true, Relaxed);
    let mut unjoined = 0;
    for h in &threads {
        unjoined += u64::from(!h.join_timeout(60 * SEC));
    }
    let d: Vec<u64> = last.iter().zip(first.iter()).map(|(n, f)| n - f).collect();
    let (gw, gh) = grace_lock();
    let fails = mm_core::report::GLOBAL.alloc_fails.load(Relaxed) - fails0;
    klog::event!(Info, "mm", "ktest.memcrash", cpus = cpus, total_mb = total >> 8, vmas = listed, keys = s.keys.load(Relaxed), rounds = s.rounds.load(Relaxed),
        writes = s.writes.load(Relaxed), reads = s.reads.load(Relaxed), blocks = s.blocks.load(Relaxed), block_fails = s.block_fails.load(Relaxed), errors_keys = s.errors[0].load(Relaxed), errors_churn = s.errors[1].load(Relaxed), errors_write = s.errors[2].load(Relaxed), errors_read = s.errors[3].load(Relaxed),
        frame_fails = fails, patient = d[0], cut = d[1], heap_reclaim = d[2], busy_caches = d[3], boost_raised = d[5], boost_cleared = d[6], grace_handover = d[7],
        grace_waits = d[8], nofail = d[9], oom_kills = d[10], gather_nobatch = d[11], heap_alloc_fail = d[12], nofail_allocs = d[14],
        nofail_failed = d[15], grace_overflow = d[16], nofail_max_us = metric("mm.alloc.nofail_max_us"), regain_max_us = metric("mm.heap.regain_max_us"),
        regain_held_max_us = metric("mm.heap.regain_held_max_us"), grace_wait_max_us = us(gw), grace_hold_max_us = us(gh), unjoined = unjoined);
    kassert!(unjoined == 0, "{unjoined} threads did not end");
    let errors: Vec<u64> = s.errors.iter().map(|e| e.load(Relaxed)).collect();
    kassert!(errors.iter().all(|e| *e == 0), "calls failed or read wrong (keys, arena, file writes, file reads): {errors:?}");
    kassert!(s.block_fails.load(Relaxed) == 0, "{} kernel buffers were refused", s.block_fails.load(Relaxed));
    kassert!(d[12] == 0, "{} kernel heap allocations failed", d[12]);
    kassert!(d[15] == 0, "{} allocations that must not fail found nothing to kill", d[15]);
    kassert!(d[8] == 0, "reclaim waited {} times for mm.grace inside an allocation", d[8]);
    kassert!(d[10] == 0, "the OOM killer killed {} times beside a page cache that could go", d[10]);
    kassert!(s.keys.load(Relaxed) > 0 && s.rounds.load(Relaxed) > 0 && s.writes.load(Relaxed) > 0, "a part of the load did not run");
    for i in (0..heap).step_by(97) {
        let v = a.read_u64(HEAP_BASE + i * P).map_err(e)?;
        kassert!(v == !i, "heap page {i} reads {v:#x}");
    }
    Ok(())
}
// Run: three minutes of 12 CPUs in 2 GiB (host/tests/mm/82-memcrash.sh); never on hardware.
ktest!("mm.memcrash", Run("mm/82-memcrash"), memcrash);
