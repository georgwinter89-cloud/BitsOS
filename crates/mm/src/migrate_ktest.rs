//! W35 MM-2 ktests of page migration (`mm.migrate.*`): an anonymous page and a page-cache page moved again and
//! again while a thread on another CPU reads every byte (and, for the anonymous one, writes and reads back), a
//! fork-shared frame moved through the walk (both spaces follow), a huge page moved whole and one split and moved,
//! the frames that must stay (dirty, held as a mapping's pin holds a page, a DMA frame, an anonymous frame with a
//! reference no entry explains), whole-memory compaction moving anonymous memory for real, and a thread of the
//! program that maps and unmaps while compaction works through its memory (it is never kept waiting).

use crate::linux::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, PROT_READ};
use crate::migrate::{self, HugeMoved};
use crate::{share, AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use hal::paging::{self, probe, PageSize};
use ktest::{kassert, ktest, TestResult};
use mm_core::compact::{Migrate, Moved};
use mm_core::Kind;

const P: u64 = 4096;
const M2: u64 = 2 << 20;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = 3;
const PF_U: u64 = 4;
const PF_W: u64 = 2;
const SEC: u64 = 1_000_000_000;
const BASE: u64 = 0x34_0000_0000;
/// Pages and rounds of the racing tests: every page moves `ROUNDS` times under the other CPU's reads.
const PAGES: u64 = 8;
const ROUNDS: u64 = 32;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

fn user() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[Kind::User as usize])
}

fn phys_of(a: &AddressSpace, va: u64) -> Result<u64, String> {
    paging::translate(a.root(), va).map(|(p, _)| p & !(P - 1)).ok_or_else(|| format!("{va:#x} not mapped"))
}

/// The word `w` of page `pg` of the anonymous test pages (word 0 is the racing thread's own).
fn word(pg: u64, w: u64) -> u64 {
    0x5A00_0000_0000_0000 | pg << 16 | w
}

/// Moves `phys` (of `kind`), retrying while a lock is taken or a target is missing (the racing thread holds the
/// space's lock during its faults); a move that does not succeed says why.
fn moved(phys: u64, kind: Kind) -> Result<(), String> {
    let mut last = Moved::Stays;
    for _ in 0..1000 {
        last = migrate::migrate_frame(phys, kind);
        if last == Moved::Moved {
            return Ok(());
        }
        if last == Moved::Unsupported {
            break;
        }
        sched::yield_now();
    }
    Err(format!("{phys:#x} ({kind:?}) not moved: {last:?}"))
}

/// One access of the racing thread through the loaded space: a fault (the page is frozen) waits for the space's
/// lock in the fault path, then the access goes again. None: refused or the fault failed.
fn access(a: &AddressSpace, va: u64, write: Option<u64>, faults: &AtomicU64) -> Option<u64> {
    loop {
        let r = {
            let _p = ksync::PreemptGuard::new();
            paging::activate(a.root());
            match write {
                Some(v) => probe::user_write(va, v).map(|r| r.map(|()| v)),
                None => probe::user_read(va),
            }
        };
        match r {
            Ok(Ok(v)) => return Some(v),
            Ok(Err(_)) => {
                faults.fetch_add(1, SeqCst);
                let err = if write.is_some() { PF_U | PF_W } else { PF_U };
                a.handle_user_fault(va, err).ok()?;
            }
            Err(_) => return None,
        }
    }
}

/// Shared state of a racing thread and the test.
struct Race {
    stop: AtomicBool,
    bad: AtomicU64,
    faults: AtomicU64,
    passes: AtomicU64,
    last: Vec<AtomicU64>,
}

impl Race {
    fn new() -> Arc<Race> {
        Arc::new(Race { stop: AtomicBool::new(false), bad: AtomicU64::new(0), faults: AtomicU64::new(0), passes: AtomicU64::new(0), last: (0..PAGES).map(|_| AtomicU64::new(0)).collect() })
    }

    /// Lets the thread finish two more passes (at most 10 s), then stops it.
    fn end(&self, h: &sched::JoinHandle) -> bool {
        let (p0, t0) = (self.passes.load(SeqCst), time::now_ns());
        while self.passes.load(SeqCst) < p0 + 2 && time::now_ns() - t0 < 10 * SEC {
            sched::sleep_ms(1);
        }
        self.stop.store(true, SeqCst);
        h.join_timeout(30 * SEC)
    }
}

/// The other CPU: writes word 0 of each page and reads it back at once, reads every other word.
fn racer(a: &AddressSpace, m: u64, r: &Race) {
    let mut n = 1u64;
    while !r.stop.load(SeqCst) {
        for pg in 0..PAGES {
            let va = m + pg * P;
            n += 1;
            if access(a, va, Some(n), &r.faults).is_none() || access(a, va, None, &r.faults) != Some(n) {
                r.bad.fetch_add(1, SeqCst);
            }
            r.last[pg as usize].store(n, SeqCst);
            for w in 1..512 {
                if access(a, va + w * 8, None, &r.faults) != Some(word(pg, w)) {
                    r.bad.fetch_add(1, SeqCst);
                }
            }
        }
        r.passes.fetch_add(1, SeqCst);
    }
}

/// An anonymous page moved 32 times while another CPU reads every byte of it and writes and reads back word 0: no
/// read sees a wrong value, no write is lost, the frames all come back.
fn anon_racing() -> TestResult {
    let aps = crate::tlb_ktest::aps();
    let Some(&cpu) = aps.first() else { return ktest::fail("one CPU: the racing thread needs another (run with -smp 2 or more)") };
    let before = user();
    let moved_before = migrate::ANON_MOVED.get();
    {
        let a = Arc::new(lazy()?);
        let m = a.sys_mmap(BASE, PAGES * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
        for pg in 0..PAGES {
            let mut page = [0u8; P as usize];
            for w in 0..512 {
                page[w as usize * 8..][..8].copy_from_slice(&word(pg, w).to_le_bytes());
            }
            a.copy_to_user(m + pg * P, &page).map_err(e)?;
        }
        let _s = probe::user_probes().map_err(e)?;
        let race = Race::new();
        let (a2, r2) = (a.clone(), race.clone());
        let o = sched::SpawnOpts { cpu: Some(cpu), pin: true, ..sched::SpawnOpts::default() };
        let h = sched::spawn_with("mm-migrate-racer", sched::Class::Normal, o, move || racer(&a2, m, &r2)).map_err(e)?;
        let mut err = None;
        'rounds: for _ in 0..ROUNDS {
            for pg in 0..PAGES {
                let old = phys_of(&a, m + pg * P)?;
                if let Err(x) = moved(old, Kind::User) {
                    err = Some(x);
                    break 'rounds;
                }
                if phys_of(&a, m + pg * P)? == old {
                    err = Some(format!("page {pg} still at {old:#x} after a move"));
                    break 'rounds;
                }
            }
        }
        kassert!(race.end(&h), "the racing thread did not end");
        if let Some(x) = err {
            return Err(x);
        }
        let (bad, faults, passes) = (race.bad.load(SeqCst), race.faults.load(SeqCst), race.passes.load(SeqCst));
        kassert!(bad == 0, "{bad} wrong reads or lost writes ({faults} faults, {passes} passes)");
        for pg in 0..PAGES {
            let w0 = a.read_u64(m + pg * P).map_err(e)?;
            let want = race.last[pg as usize].load(SeqCst);
            kassert!(w0 == want, "page {pg}: word 0 is {w0:#x}, the last write was {want:#x}");
            for w in 1..512 {
                let v = a.read_u64(m + pg * P + w * 8).map_err(e)?;
                kassert!(v == word(pg, w), "page {pg} word {w}: {v:#x}");
            }
        }
        klog::event!(Info, "mm", "ktest.migrate_anon", moves = PAGES * ROUNDS, faults = faults, passes = passes);
    }
    kassert!(migrate::ANON_MOVED.get() - moved_before >= PAGES * ROUNDS, "anon_moved counted {}", migrate::ANON_MOVED.get() - moved_before);
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.migrate.anon", Safe, anon_racing);

/// A clean page-cache page moved 32 times while another CPU reads it (both ends hold the page's index, the rest
/// zero): every read sees exactly that, the file keeps its pages.
fn cache_racing() -> TestResult {
    let aps = crate::tlb_ktest::aps();
    let Some(&cpu) = aps.first() else { return ktest::fail("one CPU: the racing thread needs another (run with -smp 2 or more)") };
    let (c, _pat) = crate::reclaim_ktest::pattern_file(PAGES);
    let c = Arc::new(c);
    crate::reclaim_ktest::check_file(&c, PAGES)?;
    let phys = |i: u64| c.cached(i).and_then(|p| p.phys());
    let race = Race::new();
    let (c2, r2) = (c.clone(), race.clone());
    let o = sched::SpawnOpts { cpu: Some(cpu), pin: true, ..sched::SpawnOpts::default() };
    let h = sched::spawn_with("mm-migrate-reader", sched::Class::Normal, o, move || {
        let mut buf = alloc::vec![0u8; P as usize];
        while !r2.stop.load(SeqCst) {
            for i in 0..PAGES {
                buf.fill(0xEE);
                let ok = c2.read_at(i * P, &mut buf).is_ok_and(|n| n == P as usize);
                let ends = buf[..8] == i.to_le_bytes() && buf[P as usize - 8..] == i.to_le_bytes();
                if !ok || !ends || buf[8..P as usize - 8].iter().any(|&b| b != 0) {
                    r2.bad.fetch_add(1, SeqCst);
                }
            }
            r2.passes.fetch_add(1, SeqCst);
        }
    })
    .map_err(e)?;
    let before = migrate::CACHE_MOVED.get();
    let mut err = None;
    'rounds: for _ in 0..ROUNDS {
        for i in 0..PAGES {
            let Some(old) = phys(i) else {
                err = Some(format!("page {i} left the cache"));
                break 'rounds;
            };
            if let Err(x) = moved(old, Kind::PageCache) {
                err = Some(x);
                break 'rounds;
            }
            if phys(i) == Some(old) {
                err = Some(format!("page {i} still at {old:#x} after a move"));
                break 'rounds;
            }
        }
    }
    kassert!(race.end(&h), "the reading thread did not end");
    if let Some(x) = err {
        return Err(x);
    }
    let (bad, passes) = (race.bad.load(SeqCst), race.passes.load(SeqCst));
    kassert!(bad == 0, "{bad} wrong reads in {passes} passes");
    kassert!(migrate::CACHE_MOVED.get() - before >= PAGES * ROUNDS, "cache_moved counted {}", migrate::CACHE_MOVED.get() - before);
    crate::reclaim_ktest::check_file(&c, PAGES)?;
    Ok(())
}
ktest!("mm.migrate.cache", Safe, cache_racing);

/// A frame fork shares moves through the walk over the spaces: both entries follow, the share count moves along,
/// and copy-on-write still works afterwards; a frame whose owner space ended is found the same way.
fn shared() -> TestResult {
    let before = user();
    let walks = crate::aspace::migrate::WALKS.get();
    {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
        a.write_u64(m, 0xF0F0).map_err(e)?;
        let b = a.fork().map_err(e)?;
        let old = phys_of(&a, m)?;
        kassert!(phys_of(&b, m)? == old && share::others(old) == 1, "fork shares the frame");
        moved(old, Kind::User)?;
        let new = phys_of(&a, m)?;
        kassert!(new != old && phys_of(&b, m)? == new, "both spaces map the target: {new:#x} / {:#x}", phys_of(&b, m)?);
        kassert!(share::others(new) == 1, "the share count moved: {}", share::others(new));
        kassert!(crate::aspace::migrate::WALKS.get() > walks, "no walk over the spaces");
        kassert!(a.read_u64(m).map_err(e)? == 0xF0F0 && b.read_u64(m).map_err(e)? == 0xF0F0, "content after the move");
        b.write_u64(m, 0xB).map_err(e)?;
        kassert!(a.read_u64(m).map_err(e)? == 0xF0F0 && b.read_u64(m).map_err(e)? == 0xB, "copy-on-write after the move");
        // The parent ends first: the child's frame keeps an owner word naming the parent; the walk finds it.
        let c = a.fork().map_err(e)?;
        drop(a);
        let f = phys_of(&c, m)?;
        moved(f, Kind::User)?;
        kassert!(c.read_u64(m).map_err(e)? == 0xF0F0 && phys_of(&c, m)? != f, "the orphan's frame moved");
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.migrate.shared", Safe, shared);

/// A huge page moves whole into a free 2 MiB block; another one is split and its 512 pages moved one by one.
fn thp() -> TestResult {
    crate::thp_ktest::room();
    let before = user();
    {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, 2 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        for half in 0..2 {
            a.handle_user_fault(m + half * M2, PF_U | PF_W).map_err(e)?;
            kassert!(paging::translate_leaf(a.root(), m + half * M2).is_some_and(|l| l.size == PageSize::Size2M), "no huge page at {half} ({})",
                mm_core::block_census(mm_core::HUGE_ORDER).map_or(String::new(), |c| format!("{c}")));
        }
        for i in 0..1024 {
            a.write_u64(m + i * P + 8, 0xAB00_0000 + i).map_err(e)?;
        }
        let head = phys_of(&a, m)?;
        let r = migrate::migrate_huge(head);
        kassert!(r == HugeMoved::Whole, "the first huge page did not move whole: {r:?}");
        let l = paging::translate_leaf(a.root(), m).ok_or("gone")?;
        kassert!(l.size == PageSize::Size2M && l.phys != head && mm_core::is_user_block(l.phys, mm_core::HUGE_ORDER), "moved whole: {l:?}");
        let head2 = phys_of(&a, m + M2)?;
        let b = mm_core::boot_buddy().ok_or("no boot allocator")?;
        let mut dest = || b.alloc_target(0, mm_core::Zone::Normal, Kind::User);
        let r = crate::aspace::migrate::migrate_huge(head2, b.owner(head2), false, &mut dest);
        crate::aspace::migrate::drop_dying();
        kassert!(r == crate::aspace::migrate::Huge::Split(512), "the second huge page: {r:?}");
        kassert!(paging::translate_leaf(a.root(), m + M2).is_some_and(|l| l.size == PageSize::Size4K), "split into 4 KiB entries");
        kassert!((0..512).all(|i| phys_of(&a, m + M2 + i * P).is_ok_and(|p| p != head2 + i * P)), "every page moved");
        for i in 0..1024 {
            let v = a.read_u64(m + i * P + 8).map_err(e)?;
            kassert!(v == 0xAB00_0000 + i, "page {i} reads {v:#x}");
        }
        kassert!(a.mem_stats() == a.mem_stats_walked(), "kept counters differ from the walk");
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.migrate.thp", Safe, thp);

/// What stays: a dirty cache page, a page someone holds (as a mapping's pin, a read or a block request hold it),
/// a DMA frame, an anonymous frame with a reference no entry explains (as a device would hold one), and an mlocked
/// one while vm.compact_unevictable_allowed is 0 (it moves with 1).
fn pinned() -> TestResult {
    let (c, _pat) = crate::reclaim_ktest::pattern_file(4);
    crate::reclaim_ktest::check_file(&c, 4)?;
    // Dirty with the bytes it holds (the check below reads the pattern).
    c.write_at(P, &1u64.to_le_bytes()).map_err(|x| format!("write: {x}"))?;
    let phys = |i: u64| c.cached(i).and_then(|p| p.phys());
    let (Some(dirty), Some(held)) = (phys(1), phys(2)) else { return ktest::fail("pages not cached") };
    let pin = c.cached(2);
    let (d0, h0) = (migrate::CACHE_DIRTY.get(), migrate::CACHE_HELD.get());
    let rd = migrate::migrate_frame(dirty, Kind::PageCache);
    let rh = migrate::migrate_frame(held, Kind::PageCache);
    kassert!(rd == Moved::Stays && migrate::CACHE_DIRTY.get() > d0, "dirty page: {rd:?}");
    kassert!(rh == Moved::Stays && migrate::CACHE_HELD.get() > h0, "held page: {rh:?}");
    kassert!(phys(1) == Some(dirty) && phys(2) == Some(held), "a page moved anyway");
    drop(pin);
    moved(held, Kind::PageCache)?;
    let fa = mm_core::frames().ok_or("no frames")?;
    let dma = fa.alloc(0, mm_core::Zone::Normal, Kind::Dma).ok_or("no DMA frame")?;
    let rdma = migrate::MIGRATOR.migrate(dma, Kind::Dma, 0, &mut || None);
    fa.free(dma, 0, Kind::Dma);
    kassert!(rdma == Moved::Unsupported, "DMA frame: {rdma:?}");
    let before = user();
    {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
        a.write_u64(m, 0x77).map_err(e)?;
        let f = phys_of(&a, m)?;
        kassert!(share::share(f), "no room for a share count");
        let r = migrate::migrate_frame(f, Kind::User);
        let _ = share::release(f);
        kassert!(r == Moved::Unsupported && phys_of(&a, m)? == f, "a frame with an unexplained reference: {r:?}");
        moved(f, Kind::User)?;
        kassert!(a.read_u64(m).map_err(e)? == 0x77, "content after the move");
        // mlocked: stays while vm.compact_unevictable_allowed is 0, moves with Linux's default 1.
        let cap = crate::LockLimit { bytes: 0, capable: true };
        kassert!(a.sys_mlock(m, P, 0, cap).is_ok(), "mlock");
        let f = phys_of(&a, m)?;
        let l0 = migrate::ANON_LOCKED.get();
        mm_core::compact::set_unevictable_allowed(0);
        let r = migrate::migrate_frame(f, Kind::User);
        mm_core::compact::set_unevictable_allowed(1);
        kassert!(r == Moved::Unsupported && migrate::ANON_LOCKED.get() > l0 && phys_of(&a, m)? == f, "mlocked with 0: {r:?}");
        moved(f, Kind::User)?;
        kassert!(a.read_u64(m).map_err(e)? == 0x77, "mlocked page after the move");
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    crate::reclaim_ktest::check_file(&c, 4)?;
    Ok(())
}
ktest!("mm.migrate.pinned", Safe, pinned);

/// compact_memory repairs fragmentation with anonymous memory now: a 64 MiB program heap (allocated low, the free
/// scanner's targets are high) with every other page given back (no two free frames buddies inside it) is partly
/// moved, at least one pageblock comes free whole, and every kept page reads back intact. QemuOnly: it compacts
/// the whole machine.
fn compact_anon() -> TestResult {
    let before = migrate::ANON_MOVED.get();
    let a = lazy()?;
    // 4 KiB pages: compaction leaves huge pages where they are (Linux `skip_isolation_on_order`).
    a.set_thp_disabled(true);
    let pages = 64 * 256;
    let m = a.sys_mmap(BASE, pages * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for i in 0..pages {
        a.write_u64(m + i * P, 0xC0_0000 + i).map_err(e)?;
    }
    for i in (1..pages).step_by(2) {
        a.sys_munmap(m + i * P, P).map_err(e)?;
    }
    let s = crate::compact::compact_memory();
    let n = migrate::ANON_MOVED.get() - before;
    kassert!(n > 0, "compact_memory moved no anonymous page: {s:?}");
    kassert!(s.cleared > 0, "compact_memory freed no pageblock whole: {s:?}");
    for i in (0..pages).step_by(2) {
        let v = a.read_u64(m + i * P).map_err(e)?;
        kassert!(v == 0xC0_0000 + i, "page {i} reads {v:#x} after compaction");
    }
    klog::event!(Info, "mm", "ktest.migrate_compact", anon_moved = n, moved = s.moved, skipped_anon = s.skipped_anon, cleared = s.cleared);
    Ok(())
}
ktest!("mm.migrate.compact", QemuOnly, compact_anon);

/// Calls the mapper of `waiter_first` makes and the pause between two of them.
const CALLS: u64 = 200;
const PAUSE_NS: u64 = 50_000;
/// Migrations that may end during one call: the one in flight when the caller went to sleep for the space's lock,
/// one more whose count came late; twice that for what a preemption of the caller can add. Without the standing
/// back a call saw hundreds (626 and 1613 in the runs of 11.11.2026).
const AHEAD_MAX: u64 = 4;
/// How long the program's other thread keeps interrupts off at a time: a CPU that answers a shootdown slowly, so
/// a migration holds the space longer than a waiter spins and the waiter sleeps.
const DEAF_NS: u64 = 150_000;

/// What the mapper of `waiter_first` saw.
#[derive(Clone, Default)]
struct Mapped {
    calls: u64,
    ahead_max: u64,
    ahead_sum: u64,
    worst_ns: u64,
    err: Option<String>,
}

/// One round of the mapper: a page of its own mapping unmapped, mapped again, written, protected and opened,
/// with a pause after each call; for every call the migrations that ended while it ran.
fn mapper(a: &AddressSpace, m: u64, pages: u64, out: &mut Mapped) -> Result<(), String> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    while out.calls < CALLS {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let (pg, va) = (x % pages, m + (x % pages) * P);
        for step in 0..5 {
            let (t, n0) = (time::now_ns(), migrate::ANON_MOVED.get());
            match step {
                0 => a.sys_munmap(va, P).map_err(e)?,
                1 => a.sys_mmap(va, P, RW, AP | MAP_FIXED, 0).map(|_| ()).map_err(e)?,
                2 => a.write_u64(va, word(pg, 0)).map_err(e)?,
                3 => a.sys_mprotect(va, P, PROT_READ).map(|_| ()).map_err(e)?,
                _ => a.sys_mprotect(va, P, RW).map(|_| ()).map_err(e)?,
            }
            let n = migrate::ANON_MOVED.get() - n0;
            out.calls += 1;
            out.ahead_max = out.ahead_max.max(n);
            out.ahead_sum += n;
            out.worst_ns = out.worst_ns.max(time::now_ns() - t);
            sched::sleep_ns(PAUSE_NS);
        }
    }
    Ok(())
}

/// The program's other thread: the space loaded on its CPU, reading the heap with interrupts off for `DEAF_NS` at
/// a time; a read that meets a page a migration holds faults and waits for the space's lock like any fault.
fn deaf_reader(a: &AddressSpace, m: u64, pages: u64, r: &Race) {
    let mut i = 0u64;
    while !r.stop.load(SeqCst) {
        let mut frozen = None;
        {
            let _irq = hal::cpu::IrqGuard::new();
            paging::activate(a.root());
            let t = time::now_ns();
            while frozen.is_none() && time::now_ns() - t < DEAF_NS {
                i = (i + 2) % pages;
                match probe::user_read(m + i * P) {
                    Ok(Ok(v)) if v == 0xC0_0000 + i => {}
                    Ok(Err(_)) => frozen = Some(m + i * P),
                    _ => {
                        r.bad.fetch_add(1, SeqCst);
                    }
                }
            }
        }
        if let Some(va) = frozen {
            r.faults.fetch_add(1, SeqCst);
            if a.handle_user_fault(va, PF_U).is_err() {
                r.bad.fetch_add(1, SeqCst);
            }
        }
        r.passes.fetch_add(1, SeqCst);
    }
}

/// A program is never kept waiting by compaction working through its memory: while whole-memory compaction
/// (kcompactd's scanners, pass after pass) moves a fragmented 64 MiB heap, a thread of the program on the
/// compactor's CPU keeps unmapping, mapping, writing and protecting pages of the same space, and another on the
/// other CPU reads the heap and answers shootdowns slowly. No call of the mapper has more than `AHEAD_MAX`
/// migrations end while it runs (the scanner takes the space's lock for every frame; it stands back once a thread
/// sleeps for it), the scanner did stand back and did move pages, and nothing reads wrong. QemuOnly: it compacts
/// the whole machine and keeps a CPU's interrupts off for 150 us at a time.
fn waiter_first() -> TestResult {
    let aps = crate::tlb_ktest::aps();
    let Some(&cpu) = aps.first() else { return ktest::fail("one CPU: the compactor needs another (run with -smp 2 or more)") };
    let a = Arc::new(lazy()?);
    a.set_thp_disabled(true);
    let pages = 64 * 256;
    let m = a.sys_mmap(BASE, pages * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for i in 0..pages {
        a.write_u64(m + i * P, 0xC0_0000 + i).map_err(e)?;
    }
    for i in (1..pages).step_by(2) {
        a.sys_munmap(m + i * P, P).map_err(e)?;
    }
    let own = 64;
    let m2 = a.sys_mmap(BASE + (1 << 32), own * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    for pg in 0..own {
        a.write_u64(m2 + pg * P, word(pg, 0)).map_err(e)?;
    }
    let _s = probe::user_probes().map_err(e)?;
    let (moved0, yielded0) = (migrate::ANON_MOVED.get(), crate::aspace::migrate::YIELDED.get());
    let race = Race::new();
    let passes = Arc::new(AtomicU64::new(0));
    let there = sched::SpawnOpts { cpu: Some(cpu), pin: true, ..sched::SpawnOpts::default() };
    let here = sched::SpawnOpts { cpu: Some(sched::current_cpu()), ..there };
    let (r2, p2) = (race.clone(), passes.clone());
    let compactor = sched::spawn_with("mm-migrate-compactor", sched::Class::Background, there, move || {
        let Some(b) = mm_core::boot_buddy() else { return };
        while !r2.stop.load(SeqCst) {
            b.compact_all(mm_core::buddy::How::Manual, &mut sched::yield_now);
            crate::aspace::migrate::drop_dying();
            p2.fetch_add(1, SeqCst);
        }
    })
    .map_err(e)?;
    let (a3, r3) = (a.clone(), race.clone());
    let reader = sched::spawn_with("mm-migrate-deaf", sched::Class::Normal, here, move || deaf_reader(&a3, m, pages, &r3)).map_err(e);
    let seen = Arc::new(ksync::SpinLock::new(Mapped::default()));
    let (a4, s4) = (a.clone(), seen.clone());
    let mapping = reader.and_then(|reader| {
        let h = sched::spawn_with("mm-migrate-mapper", sched::Class::Normal, there, move || {
            let mut out = Mapped::default();
            out.err = mapper(&a4, m2, own, &mut out).err();
            *s4.lock() = out;
        });
        h.map(|h| (reader, h)).map_err(e)
    });
    let ended = match &mapping {
        Ok((_, h)) => h.join_timeout(120 * SEC),
        Err(_) => false,
    };
    race.stop.store(true, SeqCst);
    let mut all = compactor.join_timeout(30 * SEC);
    if let Ok((reader, _)) = &mapping {
        all &= reader.join_timeout(30 * SEC);
    }
    mapping?;
    kassert!(ended && all, "a thread did not end (mapper {ended})");
    let s = seen.lock().clone();
    let (moved, yielded) = (migrate::ANON_MOVED.get() - moved0, crate::aspace::migrate::YIELDED.get() - yielded0);
    klog::event!(Info, "mm", "ktest.migrate_waiter", calls = s.calls, ahead_max = s.ahead_max, ahead_sum = s.ahead_sum, worst_us = s.worst_ns / 1000,
        moved = moved, yielded = yielded, passes = passes.load(SeqCst), reader_faults = race.faults.load(SeqCst), reader_rounds = race.passes.load(SeqCst));
    if let Some(x) = s.err {
        return Err(x);
    }
    kassert!(s.ahead_max <= AHEAD_MAX, "{} migrations ended while one call of the mapper ran ({} over {} calls, the worst took {} us): the scanner went ahead of a waiting thread",
        s.ahead_max, s.ahead_sum, s.calls, s.worst_ns / 1000);
    kassert!(moved > 0, "compaction moved no page of the heap while the mapper ran: nothing was tested");
    kassert!(yielded > 0, "the scanner never met a thread waiting for the space: nothing was tested ({moved} moved)");
    let bad = race.bad.load(SeqCst);
    kassert!(bad == 0, "{bad} wrong reads of the heap");
    for i in (0..pages).step_by(2) {
        let v = a.read_u64(m + i * P).map_err(e)?;
        kassert!(v == 0xC0_0000 + i, "heap page {i} reads {v:#x}");
    }
    Ok(())
}
ktest!("mm.migrate.waiter_first", QemuOnly, waiter_first);
