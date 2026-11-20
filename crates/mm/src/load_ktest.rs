//! ktest `mm.migrate.load` (its own boot, host/tests/mm/80-load.sh: 16 CPUs, 6 GiB; on the Mini-PC over MCP in any
//! ktest round): the load that hung the Mini-PC
//! on 12.11.2026 (generation 137, 30 minutes of Steam and a game beside a page cache of 20 GB), small. A program
//! with a large anonymous heap and many threads that unmap, map and fault, CPUs that keep its address space
//! loaded, a page cache that fills memory and is read on (reclaim keeps running), and kernel threads that take
//! and drop buffers of 68 KiB, 12 KiB and 28 KiB (what recvmsg, getdents64 and write did there). Every second the
//! counters the Mini-PC's journal showed are written out (`mm ktest.load_sec`).
//!
//! What is judged are counts (QEMU's clock is not judged; the longest calls are only written out). Before the
//! fixes of that day (aa8385fc, this test, 16 CPUs, 6 GiB, 45 s, two runs) and with them:
//! - interrupts to other CPUs for freed kernel ranges (`mm.tlb.ipis.vmap` + `.kernel`): 143000 and 147000 a
//!   second, now 130 to 145 (a freed buffer is kept for the next one of its size, `scatter`);
//! - drains of every CPU's frame lists (`mm.frames.pcp_drain`): 4600 a second, now 930 (a block that single
//!   frames serve takes a run only if one is free);
//! - frames the two scanners read per direct compaction run: 84000 and 83000, now 23500 and 24300 (cached
//!   positions and a budget for the free scanner too, a run ends with its block, targets as the chunk needs them).
//!
//! Beside them a program's huge-page faults with MADV_HUGEPAGE (direct compaction when no 2 MiB block is free) keep
//! compaction's scanners, migration and its shootdowns at work, and the run must have moved pages (anonymous ones
//! too): without them a run moved 18 pages in 45 s and the scan bounds judged nothing (MM2-NIGHT checkers).
//!
//! `mm.migrate.load_long` (its own boot, host/tests/mm/81-load-long.sh, never on hardware) is the same for ten
//! minutes with a second, larger program beside it (a sixth of memory, ten threads, regions up to 8 MiB, six more
//! CPUs keeping it loaded), and judges every minute against the desk's stop numbers for the Mini-PC
//! (`STOP_*`) and the longest wait and hold of `mm.as` (250 ms); its buffers come at about the rate of generation
//! 137's busiest minute. The short run's come four times as fast, so its minutes are only written out.

use crate::linux::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use hal::paging;
use ktest::{kassert, ktest, TestResult};
use mm_core::PAGE as P;

const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = 3;
const SEC: u64 = 1_000_000_000;
const BASE: u64 = 0x36_0000_0000;
const RUN_S: u64 = 45;
const LONG_S: u64 = 600;
// The desk's stop numbers per minute (plan/guides/memory-under-load.md).
const STOP_VMAP_IPIS: u64 = 200_000;
const STOP_NEW_SCATTER: u64 = 20_000;
const STOP_PCP_DRAIN: u64 = 30_000;
const STOP_FREE_SCANNED: u64 = 60_000_000;
const STOP_DAEMON_SCANNED: u64 = 60_000_000;
const STOP_STALL: u64 = 6_000;
/// Longest wait for and hold of `mm.as` (the long run judges it; QEMU's clock, so only the long run).
const STOP_AS_US: u64 = 250_000;
/// Interrupts for freed kernel ranges, a second on average.
const IPIS_PER_S: u64 = 4_000;
/// Drains of all per-CPU frame lists, a second on average.
const DRAINS_PER_S: u64 = 3_000;
/// Frames both scanners read per direct compaction run for a small block, on average (one run may read
/// `DIRECT_SCAN` with each). The huge-page faults' runs are judged apart (`HUGE_SCAN_PER_RUN`).
const SCAN_PER_RUN: u64 = 40_000;
/// Pages compaction moves a second at least, all kinds and anonymous ones (the huge-page faults ask for it; the
/// 45 s run on 13.11.2026: 700 and 21 a second).
const MOVED_PER_S: u64 = 100;
const ANON_PER_S: u64 = 2;
/// Huge-page faults a second that found no free 2 MiB block and compacted (QEMU: 20 of 20).
const COMPACTS_PER_S: u64 = 5;
/// Frames a huge-page fault's direct run reads on average at most: its budgets (`DIRECT_SCAN` for the migrate
/// scanner, a `FREE_SCAN` slice for the free one) twice, with the last try after scanners that met. A run for 2 MiB
/// beside a full cache reads its budget (13.11.2026: 108000 a run on average in ten minutes); what this catches is
/// a scanner without a bound, generation 137's fault.
const HUGE_SCAN_PER_RUN: u64 = 2 * ((1 << 18) + (1 << 16));
/// Pages a program thread unmaps, maps and touches per round.
const REGION: u64 = 16;
const REGIONS: u64 = 16;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    rounds: AtomicU64,
    /// The longest call of a program thread since the sampler's last look, and in the whole run.
    worst_ns: AtomicU64,
    worst_all_ns: AtomicU64,
    errors: AtomicU64,
    cache_reads: AtomicU64,
    blocks: AtomicU64,
    block_fails: AtomicU64,
    /// The longest allocation of a buffer of 12 or 28 KiB (a run of frames: no single frames at first) since the
    /// sampler's last look, and in the whole run.
    block_ns: AtomicU64,
    block_all_ns: AtomicU64,
    /// The huge-page faults (`huge_faults`): asked, and served with a 2 MiB block.
    huge_asked: AtomicU64,
    huge_got: AtomicU64,
    /// Those that found no free block and compacted, and the frames the scanners read meanwhile (direct runs' only;
    /// another thread's direct run in that window counts here too).
    huge_compacts: AtomicU64,
    huge_scanned: AtomicU64,
}

impl Shared {
    fn took(&self, t0: u64) {
        let d = time::now_ns().saturating_sub(t0);
        self.worst_ns.fetch_max(d, Relaxed);
        self.worst_all_ns.fetch_max(d, Relaxed);
    }
}

fn rng(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// A thread of the program: a region of its own unmapped, mapped again and every page written.
fn program(a: &AddressSpace, arena: u64, s: &Shared, mut seed: u64, region: u64, pause_ns: u64) {
    while !s.stop.load(Relaxed) {
        let va = arena + (rng(&mut seed) % REGIONS) * region * P;
        let t = time::now_ns();
        let unmapped = a.sys_munmap(va, region * P);
        s.took(t);
        let t = time::now_ns();
        let mapped = a.sys_mmap(va, region * P, RW, AP | MAP_FIXED, 0);
        s.took(t);
        if unmapped.is_err() || mapped.is_err() {
            s.errors.fetch_add(1, Relaxed);
            continue;
        }
        for pg in 0..region {
            let t = time::now_ns();
            if a.write_u64(va + pg * P, pg).is_err() {
                s.errors.fetch_add(1, Relaxed);
            }
            s.took(t);
        }
        s.rounds.fetch_add(1, Relaxed);
        sched::sleep_ns(pause_ns);
    }
}

/// A CPU that keeps the program's space loaded (a thread of it in user mode): every shootdown of the space goes
/// there too.
fn loaded(a: &AddressSpace, s: &Shared) {
    while !s.stop.load(Relaxed) {
        {
            let _irq = hal::cpu::IrqGuard::new();
            paging::activate(a.root());
            let t = time::now_ns();
            while time::now_ns() - t < 20_000 {
                core::hint::spin_loop();
            }
        }
        sched::sleep_ns(200_000);
    }
    if let Some(k) = paging::kernel_root() {
        let _irq = hal::cpu::IrqGuard::new();
        paging::activate(k);
    }
}

/// A reader of files: pages of a file larger than memory, so the cache stays full and reclaim keeps running.
fn reader(c: &vfs::pagecache::PageCache, pages: u64, s: &Shared, mut seed: u64) {
    let mut b = [0u8; 8];
    let mut n = 0u64;
    while !s.stop.load(Relaxed) {
        let pg = rng(&mut seed) % pages;
        if c.read_at(pg * P, &mut b).is_err() || u64::from_le_bytes(b) != pg {
            s.errors.fetch_add(1, Relaxed);
        }
        s.cache_reads.fetch_add(1, Relaxed);
        n += 1;
        if n.is_multiple_of(2) {
            sched::sleep_ns(1_000_000);
        }
    }
}

/// A kernel thread with buffers: six of 68 KiB (a socket's receive buffer) for one of 12 KiB and one of 28 KiB, a
/// few held, the rest dropped at once; eight of them every `pause_ns`.
fn buffers(s: &Shared, mut seed: u64, pause_ns: u64) {
    let mut held: Vec<Vec<u8>> = Vec::new();
    if held.try_reserve_exact(32).is_err() {
        return;
    }
    while !s.stop.load(Relaxed) {
        for size in [68_000usize, 68_000, 68_000, 12_000, 68_000, 68_000, 68_000, 28_000] {
            let mut b: Vec<u8> = Vec::new();
            let t = time::now_ns();
            if b.try_reserve_exact(size).is_err() {
                s.block_fails.fetch_add(1, Relaxed);
                continue;
            }
            if size < 32_768 {
                let d = time::now_ns().saturating_sub(t);
                s.block_ns.fetch_max(d, Relaxed);
                s.block_all_ns.fetch_max(d, Relaxed);
            }
            b.resize(size, 7);
            s.blocks.fetch_add(1, Relaxed);
            if rng(&mut seed).is_multiple_of(8) {
                if held.len() < 32 {
                    held.push(b);
                } else {
                    let i = (seed >> 8) as usize % 32;
                    held[i] = b;
                }
            }
        }
        sched::sleep_ns(pause_ns);
    }
}

/// A program's huge-page faults with MADV_HUGEPAGE (Linux's `defrag=madvise`: direct compaction when no 2 MiB
/// block is free; `thp::fault_block`): one every `pause_ns`, the last `HUGE_HELD` blocks kept (a heap that grows
/// and gives back), so compaction's scanners, migration and its shootdowns work under the load (the checkers of
/// MM2-NIGHT: without it a run moved 18 pages in 45 s and the scan bounds said nothing).
fn huge_faults(s: &Shared, pause_ns: u64) {
    let (Some(b), Some(f)) = (mm_core::boot_buddy(), mm_core::frames()) else { return };
    let mut held = [0u64; HUGE_HELD];
    let mut at = 0;
    while !s.stop.load(Relaxed) {
        s.huge_asked.fetch_add(1, Relaxed);
        let got = mm_core::alloc_zeroed_huge_light(mm_core::Kind::User).or_else(|| {
            s.huge_compacts.fetch_add(1, Relaxed);
            let s0 = direct_scanned();
            let made = b.compact_direct(mm_core::HUGE_ORDER);
            s.huge_scanned.fetch_add(direct_scanned().saturating_sub(s0), Relaxed);
            if made { mm_core::alloc_zeroed_huge_light(mm_core::Kind::User) } else { None }
        });
        if let Some(g) = got {
            s.huge_got.fetch_add(1, Relaxed);
            if held[at] != 0 {
                f.free(held[at], mm_core::HUGE_ORDER, mm_core::Kind::User);
            }
            held[at] = g;
            at = (at + 1) % HUGE_HELD;
        }
        sched::sleep_ns(pause_ns);
    }
    for h in held.into_iter().filter(|&h| h != 0) {
        f.free(h, mm_core::HUGE_ORDER, mm_core::Kind::User);
    }
}
const HUGE_HELD: usize = 16;

/// Frames both scanners of direct runs read so far (the totals less kcompactd's).
fn direct_scanned() -> u64 {
    let (mut all, mut daemon) = (0u64, 0u64);
    for (n, v) in klog::metrics::snapshot() {
        match n {
            "mm.compact.free_scanned" | "mm.compact.migrate_scanned" => all += v,
            "mm.compact.daemon_free_scanned" | "mm.compact.daemon_migrate_scanned" => daemon += v,
            _ => {}
        }
    }
    all.saturating_sub(daemon)
}
const HUGE_PAUSE_NS: u64 = 50_000_000;

const NW: usize = 22;
const WATCHED: [&str; NW] = [
    "mm.compact.stall", "mm.compact.success", "mm.compact.free_scanned", "mm.compact.migrate_scanned", "mm.compact.last_tries",
    "mm.tlb.ipis.vmap", "mm.tlb.ipis.kernel", "mm.tlb.ipis.migrate", "mm.tlb.ipis.munmap", "mm.heap.scatter_allocs", "mm.frames.fallback_steal",
    "mm.frames.fallback_sweep", "mm.frames.pcp_drain", "mm.migrate.cache_moved", "mm.migrate.anon_moved", "mm.migrate.anon_busy", "vfs.reclaim.pages", "mm.heap.scatter_reused",
    "mm.compact.daemon_free_scanned", "mm.compact.daemon_migrate_scanned", "mm.heap.patient_allocs", "mm.heap.patience_cut",
];

/// One minute's counts (or the whole short run scaled to a minute) against the stop numbers; Err names the first.
fn under_stop(d: &[u64], secs: u64, at: u64) -> TestResult {
    let m = |x: u64| x * 60 / secs.max(1);
    let checks = [
        ("mm.tlb.ipis.vmap", m(d[5]), STOP_VMAP_IPIS),
        ("mm.heap.scatter_allocs - scatter_reused", m(d[9].saturating_sub(d[17])), STOP_NEW_SCATTER),
        ("mm.frames.pcp_drain", m(d[12]), STOP_PCP_DRAIN),
        ("mm.compact.free_scanned", m(d[2]), STOP_FREE_SCANNED),
        ("mm.compact.daemon scans", m(d[18] + d[19]), STOP_DAEMON_SCANNED),
        ("mm.compact.stall", m(d[0]), STOP_STALL),
    ];
    for (what, v, stop) in checks {
        kassert!(v <= stop, "{what}: {v} in the minute up to second {at} (stop number {stop})");
    }
    Ok(())
}

fn watched() -> [u64; NW] {
    let mut v = [0u64; NW];
    for (n, x) in klog::metrics::snapshot() {
        if let Some(i) = WATCHED.iter().position(|w| *w == n) {
            v[i] = x;
        }
    }
    v
}

fn load() -> TestResult {
    run(RUN_S, false)
}

fn load_long() -> TestResult {
    run(LONG_S, true)
}

/// The load for `run_s` seconds; `big`: with the second program, and `mm.as` judged.
fn run(run_s: u64, big: bool) -> TestResult {
    let cpus = 1 + crate::tlb_ktest::aps().len();
    kassert!(cpus >= 8, "{cpus} CPUs: this is the load of a machine with many (host/tests/mm/80-load.sh boots 16)");
    let Some(f) = mm_core::frames() else { return ktest::fail("no frame allocator") };
    let total = f.stats().total_bytes / P;
    let a = Arc::new(AddressSpace::with_fill(Fill::OnFault).map_err(e)?);
    a.set_thp_disabled(true);
    // The program's heap: an eighth of memory, every page written.
    let heap = total / 8;
    let m = a.sys_mmap(BASE, heap * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for i in 0..heap {
        a.write_u64(m + i * P, i).map_err(e)?;
    }
    // The second program (long run): a sixth of memory, written, its regions mapped and unmapped by ten threads.
    let b2 = if big { Some(Arc::new(AddressSpace::with_fill(Fill::OnFault).map_err(e)?)) } else { None };
    let heap2 = total / 6;
    if let Some(b2) = &b2 {
        b2.set_thp_disabled(true);
        b2.sys_mmap(BASE, heap2 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
        for i in 0..heap2 {
            b2.write_u64(BASE + i * P, !i).map_err(e)?;
        }
    }
    // The page cache: a file twice the size of memory, read until memory is full.
    let file = total * 2;
    let (cache, _pat) = crate::reclaim_ktest::pattern_file(file);
    let cache = Arc::new(cache);
    let mut b = [0u8; 8];
    let fill = (f.stats().usable_free_bytes() / P).saturating_sub(crate::reclaim::watermarks().high);
    // In a scattered order (many files read as programs need them): what reclaim frees later lies apart.
    for i in 0..fill {
        let pg = i.wrapping_mul(7919) % file;
        cache.read_at(pg * P, &mut b).map_err(|x| format!("fill page {pg}: {x}"))?;
    }
    let s = Arc::new(Shared::default());
    let mut threads = Vec::new();
    let spawn = |name: &str, cpu: Option<usize>, f: alloc::boxed::Box<dyn FnOnce() + Send>| {
        let o = sched::SpawnOpts { cpu, pin: cpu.is_some(), ..sched::SpawnOpts::default() };
        sched::spawn_with(name, sched::Class::Normal, o, f).map_err(e)
    };
    for t in 0..6u64 {
        let (a, s) = (a.clone(), s.clone());
        let arena = BASE + ((2 + t) << 32);
        let (region, pause) = if t < 2 { (1024, 100_000_000) } else { (REGION, 10_000_000) };
        threads.push(spawn("mm-load-program", None, alloc::boxed::Box::new(move || program(&a, arena, &s, 0x9E37_79B9 + t, region, pause)))?);
    }
    for cpu in [1usize, 2, 3, 4, 5, 6] {
        let (a, s) = (a.clone(), s.clone());
        threads.push(spawn("mm-load-loaded", Some(cpu), alloc::boxed::Box::new(move || loaded(&a, &s)))?);
    }
    if let Some(b2) = &b2 {
        for t in 0..10u64 {
            let (b2, s) = (b2.clone(), s.clone());
            let arena = BASE + ((16 + t) << 32);
            let (region, pause) = if t < 3 { (2048, 50_000_000) } else { (256, 5_000_000) };
            threads.push(spawn("mm-load-big", None, alloc::boxed::Box::new(move || program(&b2, arena, &s, 0x51ED_2701 + t, region, pause)))?);
        }
        for cpu in 7..=12usize {
            let (b2, s) = (b2.clone(), s.clone());
            threads.push(spawn("mm-load-loaded", Some(cpu % cpus), alloc::boxed::Box::new(move || loaded(&b2, &s)))?);
        }
    }
    for t in 0..2u64 {
        let (c, s) = (cache.clone(), s.clone());
        threads.push(spawn("mm-load-reader", None, alloc::boxed::Box::new(move || reader(&c, file, &s, 0xABCD_EF01 + t)))?);
    }
    // The short run's buffers come four times as fast as generation 137's busiest minute (it shows the signature
    // in 45 s); the long run's at about that minute's rate (1400 scattered blocks a second against 2000 there), so
    // its minutes compare with the Mini-PC's stop numbers.
    let pause = if big { 32_000_000 } else { 8_000_000 };
    for t in 0..8u64 {
        let s = s.clone();
        threads.push(spawn("mm-load-buffers", None, alloc::boxed::Box::new(move || buffers(&s, 0x1234_5678 + t, pause)))?);
    }
    {
        let s = s.clone();
        threads.push(spawn("mm-load-huge", None, alloc::boxed::Box::new(move || huge_faults(&s, HUGE_PAUSE_NS)))?);
    }
    let t0 = time::now_ns();
    let first = watched();
    let (mut last, mut rounds0) = (first, 0u64);
    let mut still = 0u64;
    let mut worst_sec_ms = 0u64;
    let (mut minute0, mut minute_err) = (first, None);
    for sec in 1..=run_s {
        sched::sleep_ns(t0 + sec * SEC - time::now_ns().min(t0 + sec * SEC));
        let now = watched();
        let d: Vec<u64> = now.iter().zip(last.iter()).map(|(n, l)| n - l).collect();
        last = now;
        let rounds = s.rounds.load(Relaxed);
        let worst_ms = s.worst_ns.swap(0, Relaxed) / 1_000_000;
        worst_sec_ms = worst_sec_ms.max(worst_ms);
        still += u64::from(rounds == rounds0);
        let st = f.stats();
        klog::event!(Info, "mm", "ktest.load_sec", t = sec, rounds = rounds - rounds0, worst_ms = worst_ms, block_ms = s.block_ns.swap(0, Relaxed) / 1_000_000, free_mb = st.usable_free_bytes() >> 20,
            cache_mb = vfs::reclaim::file_pages() >> 8, stall = d[0], success = d[1], free_scanned = d[2], migrate_scanned = d[3], last_tries = d[4],
            ipis_vmap = d[5], ipis_kernel = d[6], ipis_migrate = d[7], ipis_munmap = d[8], scatter = d[9], steals = d[10], sweeps = d[11], pcp_drain = d[12],
            cache_moved = d[13], anon_moved = d[14], anon_busy = d[15], reclaimed = d[16], reused = d[17], reads = s.cache_reads.load(Relaxed), blocks = s.blocks.load(Relaxed),
            daemon_free = d[18], daemon_migrate = d[19], patient = d[20], cut = d[21], huge_got = s.huge_got.load(Relaxed));
        rounds0 = rounds;
        if sec.is_multiple_of(60) {
            let dm: Vec<u64> = now.iter().zip(minute0.iter()).map(|(n, l)| n - l).collect();
            klog::event!(Info, "mm", "ktest.load_min", t = sec, ipis_vmap = dm[5], new_scatter = dm[9].saturating_sub(dm[17]), pcp_drain = dm[12], free_scanned = dm[2],
                daemon_scanned = dm[18] + dm[19], stall = dm[0], patient = dm[20], cut = dm[21]);
            if minute_err.is_none() {
                minute_err = under_stop(&dm, 60, sec).err();
            }
            minute0 = now;
        }
    }
    s.stop.store(true, Relaxed);
    let mut unjoined = 0;
    for h in &threads {
        unjoined += u64::from(!h.join_timeout(60 * SEC));
    }
    let d: Vec<u64> = last.iter().zip(first.iter()).map(|(n, f)| n - f).collect();
    // Direct runs' frames only: the totals hold kcompactd's too, which once direct runs became rare (13.11.2026: 92
    // in ten minutes) were nearly all of them.
    let (ipis, drains, all_runs, all_scanned) = ((d[5] + d[6]) / run_s, d[12] / run_s, d[0], (d[2] + d[3]).saturating_sub(d[18] + d[19]));
    let (huge_runs, huge_scanned) = (s.huge_compacts.load(Relaxed), s.huge_scanned.load(Relaxed));
    // The small blocks' direct runs (all direct runs less the huge-page faults').
    let (runs, scanned) = (all_runs.saturating_sub(huge_runs), all_scanned.saturating_sub(huge_scanned));
    let as_lock = ksync::stats::class("mm.as");
    let us = |t: u64| ksync::stats::ticks_to_ns(t).unwrap_or(0) / 1000;
    let kept_pages = mm_core::HEAP.stats().scatter.kept_bytes / P;
    klog::event!(Info, "mm", "ktest.load", cpus = cpus, total_mb = total >> 8, heap_mb = heap >> 8, rounds = s.rounds.load(Relaxed), errors = s.errors.load(Relaxed),
        blocks = s.blocks.load(Relaxed), block_fails = s.block_fails.load(Relaxed), cache_reads = s.cache_reads.load(Relaxed), still_seconds = still,
        ipis_per_s = ipis, ipis_bound = IPIS_PER_S, drains_per_s = drains, drains_bound = DRAINS_PER_S, direct_runs = runs, all_direct_runs = all_runs, scan_per_run = scanned / runs.max(1), scan_bound = SCAN_PER_RUN,
        free_scanned_per_s = d[2] / run_s, scatter_per_s = d[9] / run_s, reused_per_s = d[17] / run_s, kept_pages = kept_pages, big = big, run_s = run_s,
        patient_allocs = d[20], patience_cut = d[21], regain_max_us = metric("mm.heap.regain_max_us"),
        huge_asked = s.huge_asked.load(Relaxed), huge_got = s.huge_got.load(Relaxed), huge_compacts = huge_runs, huge_scan_per_run = huge_scanned / huge_runs.max(1), huge_scan_bound = HUGE_SCAN_PER_RUN, moved = d[13] + d[14], moved_bound = MOVED_PER_S * run_s, anon_moved = d[14],
        worst_ms = s.worst_all_ns.load(Relaxed) / 1_000_000, block_ms = s.block_all_ns.load(Relaxed) / 1_000_000,
        as_wait_max_us = as_lock.as_ref().map_or(0, |c| us(c.wait_max_ticks)), as_hold_max_us = as_lock.as_ref().map_or(0, |c| us(c.hold_max_ticks)), unjoined = unjoined);
    kassert!(unjoined == 0, "{unjoined} threads did not end");
    kassert!(s.errors.load(Relaxed) == 0, "{} calls failed or read wrong", s.errors.load(Relaxed));
    for i in (0..heap).step_by(97) {
        let v = a.read_u64(m + i * P).map_err(e)?;
        kassert!(v == i, "heap page {i} reads {v:#x}");
    }
    if let Some(b2) = &b2 {
        for i in (0..heap2).step_by(97) {
            let v = b2.read_u64(BASE + i * P).map_err(e)?;
            kassert!(v == !i, "second program's heap page {i} reads {v:#x}");
        }
    }
    kassert!(still == 0, "the program made no round in {still} of {run_s} seconds (longest call {worst_sec_ms} ms)");
    kassert!(s.block_fails.load(Relaxed) == 0, "{} buffers were refused", s.block_fails.load(Relaxed));
    kassert!(ipis <= IPIS_PER_S, "{ipis} interrupts a second for freed kernel ranges (at most {IPIS_PER_S}): every freed buffer goes round all CPUs");
    kassert!(drains <= DRAINS_PER_S, "{drains} drains of all per-CPU frame lists a second (at most {DRAINS_PER_S})");
    // QEMU's memory is full of cache, so the faults compact (13.11.2026: 20 a second). A machine with free 2 MiB
    // blocks may serve them without compacting: the floors then judge only when compaction ran.
    kassert!(huge_scanned / huge_runs.max(1) <= HUGE_SCAN_PER_RUN, "{} frames read per direct run of a huge-page fault ({huge_runs} runs; at most {HUGE_SCAN_PER_RUN})", huge_scanned / huge_runs.max(1));
    let compacts = huge_runs;
    let compacted = compacts >= COMPACTS_PER_S * run_s;
    kassert!(compacted || !hal::cpu::detect().hypervisor, "the huge-page faults compacted {compacts} times in {run_s} s (at least {} wanted)", COMPACTS_PER_S * run_s);
    if compacted {
        kassert!(d[13] + d[14] >= MOVED_PER_S * run_s, "compaction moved {} pages in {run_s} s (at least {} wanted): its scanners did not work under the load", d[13] + d[14], MOVED_PER_S * run_s);
        kassert!(d[14] >= ANON_PER_S * run_s, "compaction moved {} anonymous pages in {run_s} s (at least {} wanted)", d[14], ANON_PER_S * run_s);
    }
    kassert!(runs == 0 || scanned / runs <= SCAN_PER_RUN, "{} frames read per direct compaction run ({runs} runs; at most {SCAN_PER_RUN})", scanned / runs.max(1));
    if let Some(x) = minute_err {
        return Err(x);
    }
    if big {
        let (w, h) = as_lock.as_ref().map_or((0, 0), |c| (us(c.wait_max_ticks), us(c.hold_max_ticks)));
        kassert!(w <= STOP_AS_US && h <= STOP_AS_US, "mm.as: longest wait {w} us, longest hold {h} us (stop number {STOP_AS_US} us)");
    }
    Ok(())
}

fn metric(name: &str) -> u64 {
    klog::metrics::snapshot().find(|m| m.0 == name).map_or(0, |m| m.1)
}
// RunHw: under QEMU only in its own boot; on the Mini-PC it is the check for this load without a game.
ktest!("mm.migrate.load", RunHw("mm/80-load"), load);
// Run: ten minutes of 16 CPUs, the QEMU reproduction (host/tests/mm/81-load-long.sh); never on hardware.
ktest!("mm.migrate.load_long", Run("mm/81-load-long"), load_long);
