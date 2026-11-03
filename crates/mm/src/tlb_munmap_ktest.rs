//! ktests of MM-11 (the TLB shootdown storm while Brave and Cobalt play video): `mm.tlb.munmap` runs Chromium's
//! compositor pattern (a fresh layer of up to 16 MiB per frame, 13.8 MB at 1080p) on every CPU of one space
//! and counts rounds and IPIs per munmap, by cause; `mm.tlb.batch` checks that one munmap, madvise or hole punch
//! over several VMAs is one round, that madvise and reclaim free no page tables (so CPUs holding the space lazily
//! are not asked), and that munmap frees only tables no neighbouring VMA shares (Linux's `free_pgtables` floor
//! and ceiling). `mm.tlb.ipis` counts one space's own rounds and IPIs: one round (at most one IPI per other CPU)
//! per munmap, one round for a munmap over a shared segment between two private ones, and no IPI for a hole that
//! frees no table while the other CPUs hold the space lazily.

use crate::linux::{madv, MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MAP_SHARED};
use crate::tlb_ktest::{ap_activate, aps, on};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use hal::paging::{self, tlb};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const KB: u64 = 1024;
const MB: u64 = 1 << 20;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const PF_U: u64 = 4;
const PF_W: u64 = 2;
const SEC: u64 = 1_000_000_000;
/// The buffer sizes, 64 KiB up to 16 MiB; 13.8 MB is Chromium's 1080p layer (1920 x 1080 x 4 rounded to its
/// tiles, plan/notes/sc-FD-1.md).
const SIZES: [u64; 6] = [64 * KB, 256 * KB, MB, 4 * MB, 3372 * P, 16 * MB];

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<Arc<AddressSpace>, String> {
    AddressSpace::with_fill(Fill::OnFault).map(Arc::new).map_err(|x| format!("address space: {x:?}"))
}

fn generation(a: &AddressSpace) -> u64 {
    tlb::generation(a.root()).unwrap_or(0)
}

fn user_frames() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

/// Reschedule IPIs sent so far (sched's counter, the other vector of the storm; not MM's to lower).
fn resched_sent() -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == "sched.resched_ipis_sent").map_or(0, |(_, v)| v)
}

fn touch(a: &AddressSpace, base: u64, len: u64) -> Result<(), String> {
    for va in (base..base + len).step_by(P as usize) {
        a.handle_user_fault(va, PF_U | PF_W).map_err(|x| format!("store fault at {va:#x}: {}", x.text()))?;
    }
    Ok(())
}

#[derive(Default)]
struct Work {
    munmaps: AtomicU64,
    pages: AtomicU64,
    /// Set when the test gave up waiting: the threads end at their next buffer.
    stop: AtomicBool,
}

/// One compositor-like thread: a fresh buffer per frame, every page written, unmapped, a short sleep (its CPU
/// then holds the space lazily, as a Chromium thread's CPU between frames).
fn churn(a: &AddressSpace, k: u64, until: u64, w: &Work) -> Result<(), String> {
    a.activate();
    let mut seed = 0x2545_F491_4F6C_DD1Du64 ^ (k << 32 | k);
    while time::now_ns() < until && !w.stop.load(SeqCst) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = SIZES[(seed % SIZES.len() as u64) as usize];
        let base = a.sys_mmap(0, len, RW, AP, 0).map_err(e)?;
        touch(a, base, len)?;
        a.sys_munmap(base, len).map_err(e)?;
        w.munmaps.fetch_add(1, SeqCst);
        w.pages.fetch_add(len / P, SeqCst);
        sched::sleep_ns(SEC / 1000);
    }
    Ok(())
}

/// Up to 16 threads of one space on their own CPUs mmap, write and munmap 64 KiB..16 MiB buffers for 1.5 s;
/// rounds, IPIs and shootdown interrupts per munmap go to `ktest.tlb_munmap`.
fn munmap() -> TestResult {
    const RUN_NS: u64 = 1_500_000_000;
    let cpus: Vec<usize> = (0..hal::percpu::cpu_count()).filter(|&c| hal::smp::is_online(c)).take(16).collect();
    if cpus.len() < 2 {
        return ktest::skip("one CPU");
    }
    let frames_before = user_frames();
    let a = lazy()?;
    let w = Arc::new(Work::default());
    let errs = Arc::new(ksync::SpinLock::new(Vec::<String>::new()));
    let splits0 = crate::thp::SPLIT_PMD.get();
    let (s0, g0, r0, t0) = (tlb::stats(), generation(&a), resched_sent(), time::now_ns());
    let until = t0 + RUN_NS;
    let mut hs = Vec::new();
    for (k, &c) in cpus.iter().enumerate() {
        let (a, w, errs) = (a.clone(), w.clone(), errs.clone());
        let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
        let h = sched::spawn_with("mm-tlb-munmap", sched::Class::Normal, o, move || {
            let r = churn(&a, k as u64, until, &w);
            paging::activate(paging::home_root());
            if let Err(x) = r {
                errs.lock().push(x);
            }
        })
        .map_err(|x| format!("spawn on CPU {c}: {x:?}"))?;
        hs.push(h);
    }
    if !hs.iter().all(|h| h.join_timeout(120 * SEC)) {
        // Fixer: a failed run must not leave threads of `a` running into the next run of this boot (Safe tests
        // rerun over MCP): stop them and wait for them, bounded, before failing.
        w.stop.store(true, SeqCst);
        let left = hs.iter().filter(|h| !h.join_timeout(10 * SEC)).count();
        return Err(format!("a munmap thread did not finish within 120 s ({left} still running after the stop)"));
    }
    let (s1, g1, r1, t1) = (tlb::stats(), generation(&a), resched_sent(), time::now_ns());
    // MM-13: neighbouring buffers merge into one VMA, so a huge page may span two threads' buffers; the munmap of
    // one splits it first, a round of its own (Linux's `pmdp_invalidate`; other spaces' splits count too: slack).
    let splits = crate::thp::SPLIT_PMD.get() - splits0;
    if let Some(x) = errs.lock().first().cloned() {
        return Err(x);
    }
    let (n, pages) = (w.munmaps.load(SeqCst).max(1), w.pages.load(SeqCst));
    let rounds = g1 - g0;
    let ipis = s1.ipis - s0.ipis;
    let (c0, c1) = (s0.by_cause[tlb::Cause::Munmap as usize], s1.by_cause[tlb::Cause::Munmap as usize]);
    let (m_flushes, m_rounds, m_ipis, m_tables) = (c1.flushes - c0.flushes, c1.rounds - c0.rounds, c1.ipis - c0.ipis, c1.tables - c0.tables);
    // Hundredths per munmap, so the event shows the fraction.
    let per = |x: u64| x * 100 / n;
    klog::event!(Info, "mm", "ktest.tlb_munmap", cpus = cpus.len(), ms = (t1 - t0) / 1_000_000, munmaps = n, pages = pages,
        rounds = rounds, ipis = ipis, irqs = s1.irqs - s0.irqs, rounds_per_munmap_x100 = per(rounds), ipis_per_munmap_x100 = per(ipis),
        full_flushes = s1.full_flushes - s0.full_flushes, page_flushes = s1.page_flushes - s0.page_flushes,
        lazy_skips = s1.lazy_skips - s0.lazy_skips, lazy_leaves = s1.lazy_leaves - s0.lazy_leaves, munmap_flushes = m_flushes,
        munmap_rounds = m_rounds, munmap_ipis = m_ipis, munmap_tables = m_tables, resched_ipis = r1 - r0, thp_splits = splits;
        "16 threads of one space: mmap, write, munmap 64 KiB..16 MiB with a 1 ms sleep");
    drop(a);
    kassert!(s1.give_ups == s0.give_ups, "a shootdown gave up");
    // Every buffer is below one gather (16 MiB = 4096 frames < GATHER_MAX): one round per munmap at most, plus the
    // early rounds of a gather that could not grow its list (no heap for it: memory pressure after a long uptime).
    let early = s1.gather_early - s0.gather_early;
    kassert!(rounds <= n + early + splits, "{rounds} rounds for {n} munmaps ({early} early gather rounds, {splits} huge page splits)");
    // Every munmap removed written pages: each is one flush of cause munmap; its IPIs are what the munmaps sent.
    kassert!(m_flushes >= n && m_rounds <= m_flushes && m_ipis <= ipis, "munmap cause: {m_flushes} flushes, {m_rounds} rounds, {m_ipis} of {ipis} IPIs for {n} munmaps");
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.munmap", Safe, munmap);

/// One madvise(DONTNEED) over several VMAs is one round and frees no page table; a munmap between two touched
/// neighbours frees no table either (they share its tables), one over a 2 MiB-aligned run frees the tables inside.
fn batch() -> TestResult {
    let aps = aps();
    let frames_before = user_frames();
    let r = (|| -> TestResult {
        let a = lazy()?;
        // Page tables of 4 KiB pages (MM-11); huge pages have their own tests (mm.thp.*).
        a.test_fail_huge(true);
        for &c in &aps {
            on(c, ap_activate, a.root())?;
        }
        let tables = || paging::count_user_tables(a.root());
        let madvise = |s: &tlb::TlbStats| s.by_cause[tlb::Cause::Madvise as usize];
        // Three VMAs: RW, R, RW (different rights split the mapping).
        let base = a.sys_mmap(0, 3 * MB, RW, AP, 0).map_err(e)?;
        touch(&a, base, 3 * MB)?;
        a.sys_mprotect(base + MB, MB, 1).map_err(e)?;
        let (g, t, m0) = (generation(&a), tables(), madvise(&tlb::stats()));
        a.sys_madvise(base, 3 * MB, madv::DONTNEED).map_err(e)?;
        let (rounds, t1, m1) = (generation(&a) - g, tables(), madvise(&tlb::stats()));
        klog::event!(Info, "mm", "ktest.tlb_batch", step = "madvise", rounds = rounds, tables_before = t, tables_after = t1, aps = aps.len());
        kassert!(rounds == 1, "madvise(DONTNEED) over three VMAs took {rounds} rounds");
        kassert!(t1 == t && m1.tables == m0.tables && m1.flushes == m0.flushes + 1, "madvise freed page tables: {t} -> {t1}");
        // The pages come back as zeros and the tables are reused (the read-only middle VMA is only read).
        touch(&a, base, MB)?;
        touch(&a, base + 2 * MB, MB)?;
        kassert!(a.read_u64(base + MB + 8 * P).map_err(e)? == 0, "a page after DONTNEED is not zero");
        kassert!(tables() == t, "touching again allocated tables: {t} -> {}", tables());
        // A hole between two touched neighbours: its tables stay (Linux `free_pgtables` floor and ceiling).
        let munmap = |s: &tlb::TlbStats| s.by_cause[tlb::Cause::Munmap as usize];
        let u0 = munmap(&tlb::stats());
        a.sys_munmap(base + MB - 64 * KB, 128 * KB).map_err(e)?;
        let u1 = munmap(&tlb::stats());
        kassert!(tables() == t && u1.tables == u0.tables && u1.flushes == u0.flushes + 1, "a 128 KiB hole freed tables: {t} -> {}", tables());
        a.sys_munmap(base, 3 * MB).map_err(e)?;
        // A 2 MiB-aligned 4 MiB run inside an 8 MiB mapping: its two page tables go, the neighbours' stay.
        let big = a.sys_mmap(0, 8 * MB, RW, AP, 0).map_err(e)?;
        touch(&a, big, 8 * MB)?;
        let run = (big + 2 * MB - 1) & !(2 * MB - 1);
        let t = tables();
        a.sys_munmap(run, 4 * MB).map_err(e)?;
        kassert!(tables() == t - 2, "a 2 MiB-aligned 4 MiB run: tables {t} -> {} (want two fewer)", tables());
        kassert!(munmap(&tlb::stats()).tables > u1.tables, "the run's munmap was not counted as freeing tables");
        a.sys_munmap(big, 8 * MB).map_err(e)?;
        for &c in &aps {
            on(c, ap_activate, paging::home_root())?;
        }
        Ok(())
    })();
    for &c in &aps {
        let _ = on(c, ap_activate, paging::home_root());
    }
    r?;
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.batch", Safe, batch);

fn holds_lazily(root: u64) -> bool {
    tlb::lazy_here() && paging::current_root() == root
}

/// On an AP: load `root`, then hold it lazily as a kernel thread does (1: done).
fn ap_lazy_on(root: u64) -> u64 {
    paging::activate(root);
    tlb::activate_lazy_kernel();
    holds_lazily(root) as u64
}

fn ap_holds_lazily(root: u64) -> u64 {
    holds_lazily(root) as u64
}

fn switches(cpus: &[usize]) -> Vec<u64> {
    cpus.iter().map(|&c| hal::switch::switch_count_of(c)).collect()
}

/// How many of `cpus` switched threads since `before`: such a CPU may hold the space as its own again (a thread
/// whose saved root it is), so it may rightly take an IPI.
fn moved(cpus: &[usize], before: &[u64]) -> u64 {
    cpus.iter().zip(before).filter(|&(&c, &b)| hal::switch::switch_count_of(c) > b).count() as u64
}

/// The space's own rounds and IPIs (`AsCore::shootdowns`; other programs' shootdowns do not count), so this
/// is exact on the Mini-PC with Brave running too.
fn ipis() -> TestResult {
    const K: u64 = 8;
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    let others = aps.len() as u64;
    let frames_before = user_frames();
    let r = (|| -> TestResult {
        let a = lazy()?;
        // 4 KiB pages: a huge page cut by the hole would add its split's table and round (mm.thp.*).
        a.test_fail_huge(true);
        // 1. Every AP has the space loaded: each munmap of a written 1 MiB buffer is one round, so at most one
        // IPI per other CPU (resends aside).
        for &c in &aps {
            on(c, ap_activate, a.root())?;
        }
        let (g0, (r0, i0), s0) = (generation(&a), a.shootdowns(), tlb::stats());
        for _ in 0..K {
            let b = a.sys_mmap(0, MB, RW, AP, 0).map_err(e)?;
            touch(&a, b, MB)?;
            a.sys_munmap(b, MB).map_err(e)?;
        }
        let (g1, (r1, i1), s1) = (generation(&a), a.shootdowns(), tlb::stats());
        let resends = s1.resends - s0.resends;
        klog::event!(Info, "mm", "ktest.tlb_ipis", step = "owners", munmaps = K, rounds = g1 - g0, space_rounds = r1 - r0, ipis = i1 - i0,
            others = others, resends = resends);
        kassert!(g1 - g0 == K && r1 - r0 == K, "{K} munmaps took {} rounds ({} counted by the space)", g1 - g0, r1 - r0);
        kassert!(i1 - i0 <= K * others * (1 + resends), "{K} munmaps sent {} IPIs to {others} other CPUs ({resends} resend rounds)", i1 - i0);
        // 2. One munmap over a shared segment between two private ones is one round (one per segment before).
        let m = a.sys_mmap(0, 3 * MB, RW, AP, 0).map_err(e)?;
        kassert!(a.sys_mmap(m + MB, MB, RW, MAP_SHARED | MAP_ANONYMOUS | MAP_FIXED, 0).map_err(e)? == m + MB, "fixed shared mapping refused");
        touch(&a, m, 3 * MB)?;
        let (g0, (r0, _)) = (generation(&a), a.shootdowns());
        a.sys_munmap(m, 3 * MB).map_err(e)?;
        let (g1, (r1, _)) = (generation(&a), a.shootdowns());
        klog::event!(Info, "mm", "ktest.tlb_ipis", step = "segments", rounds = g1 - g0, space_rounds = r1 - r0);
        kassert!(g1 - g0 == 1 && r1 - r0 == 1, "a munmap over private, shared, private took {} rounds ({} counted by the space)", g1 - g0, r1 - r0);
        // 3. Every AP holds the space lazily: a hole between two written neighbours frees no table, so its round
        // asks none of them (Linux `tlb_is_not_lazy`); they still hold the space lazily afterwards.
        let h = a.sys_mmap(0, 2 * MB, RW, AP, 0).map_err(e)?;
        touch(&a, h, 2 * MB)?;
        // Counted from before the CPUs go lazy: a switch right after `ap_lazy_on` may make one an owner again.
        let sw = switches(&aps);
        for &c in &aps {
            kassert!(on(c, ap_lazy_on, a.root())? == 1, "CPU {c} did not hold the space lazily");
        }
        let (t0, (_, i0)) = (paging::count_user_tables(a.root()), a.shootdowns());
        a.sys_munmap(h + MB - 64 * KB, 128 * KB).map_err(e)?;
        let (t1, (_, i1)) = (paging::count_user_tables(a.root()), a.shootdowns());
        let mut still = 0;
        for &c in &aps {
            still += on(c, ap_holds_lazily, a.root())?;
        }
        let moved = moved(&aps, &sw);
        klog::event!(Info, "mm", "ktest.tlb_ipis", step = "lazy_hole", ipis = i1 - i0, still_lazy = still, switched = moved, others = others);
        kassert!(t1 == t0, "a 128 KiB hole between written neighbours freed tables: {t0} -> {t1}");
        kassert!(i1 - i0 <= moved, "a munmap that freed no table sent {} IPIs to CPUs holding the space lazily ({moved} switched)", i1 - i0);
        kassert!(still + moved >= others, "only {still} of {others} CPUs hold the space lazily after the hole ({moved} switched)");
        a.sys_munmap(h, 2 * MB).map_err(e)?;
        Ok(())
    })();
    for &c in &aps {
        let _ = on(c, ap_activate, paging::home_root());
    }
    r?;
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.ipis", Safe, ipis);
