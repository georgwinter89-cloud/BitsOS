//! PF-5 ktests (real page-cache frames): the evict race of Linux 4c0f369 (readers on several CPUs check every
//! byte of a backed file while another thread evicts its pages and reuses the frames for pages of poison),
//! memory that waits for a grace period (counted as pending and as MemAvailable, the pool charge back at once,
//! the same MemAvailable after the drain), a hole-punch loop that never sees a false ENOSPC, the bound on what
//! waits, no wait for a grace period from inside a section, and (QemuOnly, small RAM: host/tests/fs/54-grace-
//! pressure.sh) the punch loop with almost no free memory, where the next round's pages can only come from the
//! frames that wait. The Safe ones are rerunnable: each run makes its own files and gives everything back.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use mm_core::grace;
use vfs::abi::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE};
use vfs::pagecache::{Backend, CachePage, PageCache};
use vfs::pool::Pool;
use vfs::reclaim::{Mode, Reclaimer};

const SEC: u64 = 1_000_000_000;
const P: u64 = 4096;
const POISON: u8 = 0xFF;

ktest!("fs.pf5.evict_race", Safe, evict_race);
ktest!("fs.pf5.pending", Safe, pending);
ktest!("fs.pf5.punch_loop", Safe, punch_loop);
ktest!("fs.pf5.bound", Safe, bound);
ktest!("fs.pf5.in_section", Safe, in_section);
ktest!("fs.pf5.pressure", QemuOnly, pressure);

/// The byte at offset `o` of the backed file (never 0, never `POISON`).
fn pat(o: u64) -> u8 {
    (((o / P).wrapping_mul(31) ^ (o % P).wrapping_mul(7)) % 250) as u8 + 1
}

struct Disk;

impl Backend for Disk {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        for (k, p) in pages.iter().enumerate() {
            let base = (first + k as u64) * P;
            let b: Vec<u8> = (0..P).map(|i| pat(base + i)).collect();
            p.write(0, &b);
        }
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
    fn read_window(&self) -> u64 {
        8
    }
}

fn evict_race() -> TestResult {
    const PAGES: u64 = 48;
    const READERS: u64 = 6;
    let c = Arc::new(PageCache::with_backend(Arc::new(Disk), PAGES * P));
    let len = (PAGES * P) as usize;
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for t in 0..READERS {
        let (c, stop, bad, reads) = (c.clone(), stop.clone(), bad.clone(), reads.clone());
        hs.push(
            sched::spawn("fs-ktest-pf5e", sched::Class::Normal, move || {
                let mut b = vec![0u8; 5 * P as usize];
                let mut one = vec![0u8; P as usize];
                let (mut mine, mut k) = (0u64, t + 1);
                while !stop.load(Ordering::Relaxed) || mine < 50 {
                    k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let pos = (k >> 33) % len as u64;
                    let want = ((k >> 11) as usize % b.len()).max(1);
                    if t % 2 == 0 {
                        match c.read_at(pos, &mut b[..want]) {
                            Ok(n) if n == want.min(len - pos as usize) && b[..n].iter().enumerate().all(|(i, &v)| v == pat(pos + i as u64)) => {}
                            _ => {
                                bad.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    } else {
                        // Each byte checked inside the section, then the page read again: a frame freed and
                        // reused while the section is open shows.
                        let _ = c.read_quick(pos, want, &mut |p, off, k, done| {
                            let Some(p) = p else { return k };
                            let ok = |one: &[u8]| one[..k].iter().enumerate().all(|(i, &v)| v == pat(pos + (done + i) as u64));
                            p.read(off, &mut one[..k]);
                            let first = ok(&one);
                            for _ in 0..k {
                                core::hint::spin_loop();
                            }
                            p.read(off, &mut one[..k]);
                            if !first || !ok(&one) {
                                bad.fetch_add(1, Ordering::SeqCst);
                            }
                            k
                        });
                    }
                    mine += 1;
                    if mine % 64 == 0 {
                        sched::yield_now();
                    }
                }
                reads.fetch_add(mine, Ordering::SeqCst);
            })
            .map_err(|e| format!("spawn: {e:?}"))?,
        );
    }
    let r = Reclaimer::of(&[&c]);
    let t0 = time::now_ns();
    let (mut evicted, mut rounds) = (0u64, 0u64);
    while time::now_ns() - t0 < SEC / 2 || rounds < 50 {
        rounds += 1;
        evicted += r.shrink(PAGES, Mode::Background, true).freed;
        // Free what passed (no wait: readers may be inside their sections), then reuse the frames.
        grace::drain();
        let junk = PageCache::tmpfs(Arc::new(Pool::new("pf5-junk")));
        if junk.write_at(0, &vec![POISON; 16 * P as usize]).is_err() {
            break;
        }
        drop(junk);
        if rounds % 8 == 0 {
            sched::yield_now();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(20 * SEC);
    }
    grace::expedite();
    kassert!(joined, "a reader did not finish in 20 s");
    kassert!(bad.load(Ordering::SeqCst) == 0, "{} reads saw a byte the file never held", bad.load(Ordering::SeqCst));
    kassert!(evicted > 0, "nothing evicted in {rounds} rounds");
    klog::event!(Info, "ktest", "fs.pf5.evict_race", reads = reads.load(Ordering::SeqCst), evicted = evicted, rounds = rounds,
        cpus = sched::online_cpus() as u64; "readers checked every byte while pages were evicted and their frames reused");
    Ok(())
}

/// MemAvailable now (bytes) and the frame allocator's pending bytes.
fn avail() -> (u64, u64) {
    let pending = mm_core::frames().map_or(0, |f| f.stats().pending_bytes);
    (vfs::reclaim::mem_available_bytes(), pending)
}

fn pending() -> TestResult {
    const N: u64 = 1024;
    const SLACK: u64 = 4 << 20;
    if mm_core::frames().is_none() {
        return ktest::fail("no frame allocator");
    }
    let pool = Arc::new(Pool::new("pf5-pending"));
    let c = PageCache::tmpfs(pool.clone());
    c.write_at(0, &vec![0x5A; (N * P) as usize]).map_err(|e| format!("write: {e}"))?;
    let (full, _) = avail();
    let held = vfs::pagecache::frames_held();
    let hold = grace::hold_for_test().ok_or("no grace slot")?;
    let punched = c.fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, N * P);
    let (waiting, pend) = avail();
    let used = pool.used();
    let held_after = vfs::pagecache::frames_held();
    // An allocation while they wait (here there is room anyway; the host test fills the allocator).
    let f = mm_core::frames().and_then(|a| a.alloc(0, mm_core::Zone::Normal, mm_core::Kind::User));
    drop(hold);
    grace::expedite();
    let (after, pend_after) = avail();
    if let (Some(p), Some(a)) = (f, mm_core::frames()) {
        a.free(p, 0, mm_core::Kind::User);
    }
    kassert!(punched.is_ok(), "punch: {:?}", punched);
    kassert!(pend >= N * P, "pending {pend} while {} bytes wait", N * P);
    kassert!(used == 0, "the pool charge went back at the punch ({used} pages left)");
    kassert!(held.saturating_sub(held_after) >= N, "frames held {held} -> {held_after}: the punched pages left the cache's numbers");
    kassert!(waiting + SLACK >= full + N * P, "MemAvailable {full} -> {waiting} while {} bytes only wait", N * P);
    kassert!(after.abs_diff(waiting) <= SLACK, "MemAvailable {waiting} while waiting, {after} after the drain: should not move");
    kassert!(pend_after + N * P <= pend + SLACK, "pending {pend} -> {pend_after} after the drain");
    kassert!(f.is_some(), "an allocation failed while frames waited");
    klog::event!(Info, "ktest", "fs.pf5.pending", full_kb = full / 1024, waiting_kb = waiting / 1024, after_kb = after / 1024,
        pending_kb = pend / 1024; "punched pages waited as pending memory, counted as available");
    Ok(())
}

fn punch_loop() -> TestResult {
    const LIMIT: u64 = 64;
    const ROUNDS: u64 = 300;
    let pool = Arc::new(Pool::new("pf5-punch"));
    pool.set_size(Some(LIMIT));
    let c = Arc::new(PageCache::tmpfs(pool.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for _ in 0..3 {
        let (c, stop) = (c.clone(), stop.clone());
        hs.push(
            sched::spawn("fs-ktest-pf5p", sched::Class::Normal, move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    // Lockless reads only: sections, no references; what they delay only waits.
                    let _ = c.read_quick(0, (16 * P) as usize, &mut |_, _, k, _| k);
                    n += 1;
                    if n % 64 == 0 {
                        sched::yield_now();
                    }
                }
            })
            .map_err(|e| format!("spawn: {e:?}"))?,
        );
    }
    let bound = grace::bound_bytes();
    let (mut max_pending, mut failed) = (0u64, None);
    for round in 0..ROUNDS {
        let v = (round % 200) as u8 + 1;
        match c.write_at(0, &vec![v; (LIMIT * P) as usize]) {
            Ok(n) if n == (LIMIT * P) as usize => {}
            other => {
                failed = Some((round, format!("write {other:?}")));
                break;
            }
        }
        if let Err(e) = c.fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, LIMIT * P) {
            failed = Some((round, format!("punch {e}")));
            break;
        }
        if pool.used() != 0 {
            failed = Some((round, format!("{} pages still charged after the punch", pool.used())));
            break;
        }
        max_pending = max_pending.max(grace::pending_bytes());
        if round % 16 == 0 {
            sched::yield_now();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(20 * SEC);
    }
    grace::expedite();
    // A contiguous allocation after the loop (1 MiB, as a DMA buffer would take).
    let block = mm_core::frames().and_then(|a| a.alloc(8, mm_core::Zone::Normal, mm_core::Kind::Dma));
    if let (Some(p), Some(a)) = (block, mm_core::frames()) {
        a.free(p, 8, mm_core::Kind::Dma);
    }
    kassert!(joined, "a reader did not finish in 20 s");
    kassert!(failed.is_none(), "round {:?}", failed);
    kassert!(max_pending <= bound + 2 * LIMIT * P, "pending reached {max_pending}, bound {bound}");
    kassert!(mm_core::frames().is_none() || block.is_some(), "no 1 MiB block after the punch loop");
    klog::event!(Info, "ktest", "fs.pf5.punch_loop", rounds = ROUNDS, max_pending_kb = max_pending / 1024, bound_kb = bound / 1024;
        "a full pool punched and refilled with lockless readers: no false ENOSPC");
    Ok(())
}

fn bound() -> TestResult {
    const PAGES: u64 = 256;
    let small = 32 * P;
    let before = grace::stats();
    let c = PageCache::tmpfs(Arc::new(Pool::new("pf5-bound")));
    let mut worst = 0u64;
    let mut err = None;
    {
        let _small = grace::bound_for_test(small);
        for _ in 0..20 {
            if let Err(e) = c.write_at(0, &vec![3u8; (PAGES * P) as usize]).and_then(|_| c.truncate(0)) {
                err = Some(e);
                break;
            }
            worst = worst.max(grace::pending_bytes());
        }
    }
    grace::expedite();
    let after = grace::stats();
    kassert!(err.is_none(), "write/truncate: {:?}", err);
    kassert!(after.throttled > before.throttled, "no freer waited above the bound ({before:?} -> {after:?})");
    kassert!(worst <= small + PAGES * P, "pending reached {worst} with a bound of {small}");
    kassert!(after.timeouts == before.timeouts, "a bounded wait ran out ({} -> {})", before.timeouts, after.timeouts);
    Ok(())
}

/// Inside a section this CPU never waits for a grace period (it would wait for its own section until the
/// bound): `synchronize` answers false without a timeout, `expedite` only drains; after the section both work.
fn in_section() -> TestResult {
    let before = grace::stats();
    let probe = Arc::new(AtomicU64::new(0));
    let (inside, sync_inside, waited_inside);
    {
        let s = grace::enter().ok_or("no grace slot")?;
        inside = grace::in_section();
        grace::defer_arc(Arc::new(Count(probe.clone())), 0);
        sync_inside = grace::synchronize();
        grace::expedite();
        waited_inside = probe.load(Ordering::SeqCst);
        drop(s);
    }
    let outside = grace::in_section();
    let sync_after = grace::synchronize();
    grace::expedite();
    let after = grace::stats();
    kassert!(inside, "in_section is false inside a section");
    kassert!(!outside, "in_section is true after the section");
    kassert!(!sync_inside, "synchronize inside a section answered true");
    kassert!(waited_inside == 0, "an item deferred inside the open section was dropped before it ended");
    kassert!(sync_after, "no grace period after the section");
    kassert!(probe.load(Ordering::SeqCst) == 1, "the item did not leave after the section");
    kassert!(after.timeouts == before.timeouts, "a wait ran out ({} -> {})", before.timeouts, after.timeouts);
    Ok(())
}

/// Counts its drop.
struct Count(Arc<AtomicU64>);
impl Drop for Count {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Frames taken from the allocator for the test (given back on drop, also when an assertion fails).
struct Taken(Vec<(u64, u32)>);
impl Drop for Taken {
    fn drop(&mut self) {
        if let Some(a) = mm_core::frames() {
            for &(p, o) in &self.0 {
                a.free(p, o, mm_core::Kind::User);
            }
        }
    }
}

fn free_bytes() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().free_bytes)
}

/// The hole-punch loop with almost no free memory: a tmpfs file is written, the frames are taken until LEAVE
/// bytes (less than the file) stay free, then the file is punched and written again and again while lockless
/// readers keep sections open. Each round's pages can only come from the frames its punch left waiting: no write may fail or come up short, no allocation may fail after reclaim, no bounded
/// wait may run out, nothing may leak.
fn pressure() -> TestResult {
    const MIB: u64 = 1 << 20;
    const LEAVE: u64 = 8 * MIB;
    const FILE: u64 = 16 * MIB;
    const ROUNDS: u64 = 40;
    let Some(a) = mm_core::frames() else { return ktest::fail("no frame allocator") };
    let total = a.stats().total_bytes;
    if total == 0 || total > 5 * (512 * MIB) {
        return ktest::fail("takes all free memory: small RAM only (54-grace-pressure.sh, the 2G gate)");
    }
    if free_bytes() < LEAVE + FILE + 32 * MIB {
        return ktest::fail("too little free memory to start");
    }
    // The file's buffer and the readers come first (they need the heap); then the frames go.
    let data = vec![0x6Bu8; FILE as usize];
    let c = Arc::new(PageCache::tmpfs(Arc::new(Pool::new("pf5-pressure"))));
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for _ in 0..2 {
        let (c, stop) = (c.clone(), stop.clone());
        hs.push(
            sched::spawn("fs-ktest-pf5q", sched::Class::Normal, move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _ = c.read_quick((n % 64) * 16 * P, (16 * P) as usize, &mut |_, _, k, _| k);
                    n += 1;
                    if n % 32 == 0 {
                        sched::yield_now();
                    }
                }
            })
            .map_err(|e| format!("spawn: {e:?}"))?,
        );
    }
    // The file's pages are made before the frames go: from then on free memory plus what each punch leaves
    // waiting is all a round has (the ktest boot has no other cache to reclaim).
    match c.write_at(0, &data) {
        Ok(n) if n == data.len() => {}
        other => return Err(format!("first write {other:?}")),
    }
    let mut taken = Taken(Vec::new());
    taken.0.try_reserve((total / (2 * MIB) + 4096) as usize).map_err(|_| "no room for the frame list")?;
    // Large blocks first, smaller ones for the rest (never the allocator's reclaiming path: alloc_light). Free
    // frames on other CPUs' per-CPU lists count as free but this CPU cannot take them (with -smp 16 that was
    // ~2.7 MiB, INT-W27): the lists are drained into the buddy and the take repeated once.
    for pass in 0..2 {
        if pass == 1 {
            if let Some(b) = mm_core::boot_buddy() {
                b.drain_all();
            }
        }
        for o in (0..=9u32).rev() {
            while free_bytes() > LEAVE + (P << o) && taken.0.len() < taken.0.capacity() {
                let Some(p) = a.alloc_light(o, mm_core::Zone::Normal, mm_core::Kind::User) else { break };
                taken.0.push((p, o));
            }
        }
    }
    let low = free_bytes();
    let (g0, d0) = (grace::stats(), mm_core::reclaim::direct_stats());
    let mut failed = None;
    let mut max_pending = 0u64;
    for round in 0..ROUNDS {
        if let Err(e) = c.fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, FILE) {
            failed = Some((round, format!("punch {e}")));
            break;
        }
        max_pending = max_pending.max(grace::pending_bytes());
        match c.write_at(0, &data) {
            Ok(n) if n == data.len() => {}
            other => {
                failed = Some((round, format!("write {other:?} (free {} KiB, {:?})", free_bytes() / 1024, grace::stats())));
                break;
            }
        }
    }
    let (g1, d1) = (grace::stats(), mm_core::reclaim::direct_stats());
    drop(taken);
    stop.store(true, Ordering::Relaxed);
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(20 * SEC);
    }
    grace::expedite();
    klog::event!(Info, "ktest", "fs.pf5.pressure", total_mb = total / MIB, low_kb = low / 1024, rounds = ROUNDS,
        max_pending_kb = max_pending / 1024, drained_mb = (g1.drained_bytes - g0.drained_bytes) / MIB,
        expedites = g1.expedites - g0.expedites, reclaim_runs = d1.runs - d0.runs; "punch loop with almost no free memory");
    kassert!(joined, "a reader did not finish in 20 s");
    kassert!(low <= LEAVE + 2 * MIB, "free memory stayed at {} KiB: the frames could not be taken", low / 1024);
    kassert!(failed.is_none(), "round {:?}", failed);
    kassert!(d1.failed == d0.failed, "{} allocations failed after reclaim while frames waited", d1.failed - d0.failed);
    kassert!(g1.timeouts == g0.timeouts, "a bounded wait ran out ({} -> {})", g0.timeouts, g1.timeouts);
    kassert!(g1.leaked == g0.leaked, "items leaked ({} -> {})", g0.leaked, g1.leaked);
    kassert!(g1.drained_bytes - g0.drained_bytes >= (ROUNDS - 1) * FILE / 2, "the waiting frames were not reused ({g0:?} -> {g1:?})");
    Ok(())
}
