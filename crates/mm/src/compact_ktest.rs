//! W35 MM-3 ktests of compaction. On one staged allocator (heap-held tables, a fake page cache, the pool untouched;
//! Safe) the migrate scanner's corners: a frame the cache will not give up (pinned) keeps its block and marks it
//! skipped, a huge page and an anonymous frame keep theirs (the anonymous one counted), and the free scanner feeds a
//! migrator that moves. On the real page cache: drop-if-clean takes a clean page and leaves a dirty one. On the boot
//! allocator (QemuOnly): memory fragmented on purpose with clean cache gives a 2 MiB block back, kcompactd wakes on a
//! note and sleeps again, proactiveness 0 means no proactive work.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use ktest::{kassert, ktest, TestResult};
use mm_core::buddy::{Control, How, Status};
use mm_core::compact::{self as cp, DropClean, Migrate, Moved};
use mm_core::map::{MapRange, RangeUse};
use mm_core::reclaim::{Reclaim, Reclaimed};
use mm_core::{Buddy, BuddyFrames, FrameAlloc, Kind, Storage, Zone, PAGE};

const MIB: u64 = 1 << 20;
const BASE: u64 = 16 * MIB;
const PB: u64 = 512 * PAGE;
/// The staged map: 16 MiB, eight pageblocks.
const PBS: u64 = 8;

/// A fake page cache over the staged allocator: gives a frame back unless pinned.
struct Fake {
    b: &'static Buddy,
    pinned: ksync::SpinLock<Vec<u64>>,
    calls: AtomicU64,
}

impl Reclaim for Fake {
    fn reclaim(&self, _pages: u64, _thorough: bool) -> Reclaimed {
        Reclaimed::default()
    }
    fn reclaim_frame(&self, phys: u64, _owner: u64) -> bool {
        self.calls.fetch_add(1, Relaxed);
        if self.pinned.lock().contains(&phys) {
            return false;
        }
        self.b.set_owner(phys, 0);
        self.b.free(phys, 0, Kind::PageCache);
        true
    }
}

/// One staged allocator for all corner tests (its reclaimer is installed once; each test refills it).
static SHARED: ksync::OnceRef<Fake> = ksync::OnceRef::new();

fn shared() -> Result<&'static Fake, String> {
    if let Some(f) = SHARED.get() {
        return Ok(f);
    }
    let b = BuddyFrames::staged("compact", &[MapRange::new(BASE, BASE + PBS * PB, RangeUse::Free)], Storage::Heap).map_err(|e| alloc::format!("{e:?}"))?;
    let b: &'static BuddyFrames = alloc::boxed::Box::leak(alloc::boxed::Box::new(b));
    let b: &'static Buddy = b;
    let f: &'static Fake = alloc::boxed::Box::leak(alloc::boxed::Box::new(Fake { b, pinned: ksync::SpinLock::new(Vec::new()), calls: AtomicU64::new(0) }));
    kassert!(b.set_reclaimer(f) && SHARED.set(f), "staged reclaimer");
    Ok(f)
}

/// Every free frame of the staged allocator becomes clean cache with an owner; pins and hints cleared.
fn refill(f: &Fake) {
    f.pinned.lock().clear();
    f.calls.store(0, Relaxed);
    f.b.drain_all();
    // `alloc`, not `alloc_light`: the zone's reserve (its share of the min watermark) is cache too.
    while let Some(p) = f.b.alloc(0, Zone::Normal, Kind::PageCache) {
        f.b.set_owner(p, p / PAGE + 1);
    }
    f.b.reset_skip_hints();
}

/// Frees the cache block at `p` and takes it back as `order`/`kind`.
fn retake(f: &Fake, p: u64, order: u32, kind: Kind) -> TestResult {
    for i in 0..1u64 << order {
        f.b.set_owner(p + i * PAGE, 0);
        f.b.free(p + i * PAGE, 0, Kind::PageCache);
    }
    f.b.drain_all();
    let got = f.b.alloc(order, Zone::Normal, kind);
    kassert!(got == Some(p), "retake {p:#x} as order {order}: got {got:?}");
    Ok(())
}

/// One kcompactd-style run for order 9 over the staged zone (no deferral), inside reclaim's guard.
fn run9(f: &Fake) -> Result<(Status, mm_core::CompactStats), String> {
    let Some(_inside) = mm_core::reclaim::enter() else { return Err(String::from("already inside reclaim")) };
    kassert!(mm_core::reclaim::context_allows(), "the test's context may not reclaim");
    let mut c = Control::new(Some(9), How::Daemon, 0);
    let st = f.b.compact_zone(&mut c, u64::MAX);
    Ok((st, c.stats()))
}

/// Gives every frame of the staged allocator back (each test leaves it empty for the next refill).
fn empty(f: &Fake, extra: &[(u64, u32, Kind)]) {
    for &(p, o, k) in extra {
        f.b.free(p, o, k);
    }
    for i in 0..PBS * 512 {
        let p = BASE + i * PAGE;
        if f.b.owner(p) != 0 {
            f.b.set_owner(p, 0);
            f.b.free(p, 0, Kind::PageCache);
        }
    }
    f.b.drain_all();
}

/// A frame the page cache does not give up (mapped, held, dirty: `reclaim_frame` says no) keeps its pageblock: the
/// block is not counted free, its skip hint is set, the next pageblock is cleared.
fn pinned() -> TestResult {
    let f = shared()?;
    refill(f);
    f.pinned.lock().push(BASE + 3 * PAGE);
    let r = run9(f);
    let (st, s) = r?;
    let calls = f.calls.load(Relaxed);
    let got = f.b.alloc_light(9, Zone::Normal, Kind::Other);
    let hints = f.b.skip_hints();
    empty(f, got.map(|p| (p, 9, Kind::Other)).as_slice());
    kassert!(st == Status::Success, "no block made: {st:?} {s:?}");
    kassert!(got == Some(BASE + PB), "the block is the second pageblock, got {got:?}");
    kassert!(calls == 4 + 512, "{calls} frames asked (want three dropped and the pinned one, then a whole pageblock)");
    kassert!(hints == 1, "{hints} skip hints (want the pinned frame's pageblock)");
    Ok(())
}
ktest!("mm.compact.pinned", Safe, pinned);

/// A huge page (an order-9 user block) keeps its pageblock until MM-2 moves or splits it; the next one is cleared.
fn thp_in_block() -> TestResult {
    let f = shared()?;
    refill(f);
    retake(f, BASE, 9, Kind::User)?;
    let (st, s) = run9(f)?;
    let calls = f.calls.load(Relaxed);
    let got = f.b.alloc_light(9, Zone::Normal, Kind::Other);
    let mut extra = alloc::vec![(BASE, 9, Kind::User)];
    extra.extend(got.map(|p| (p, 9, Kind::Other)));
    empty(f, &extra);
    kassert!(st == Status::Success && got == Some(BASE + PB), "{st:?} {got:?} {s:?}");
    kassert!(calls == 512, "{calls} frames asked: the huge page's pageblock must not be touched");
    Ok(())
}
ktest!("mm.compact.thp", Safe, thp_in_block);

/// An anonymous frame cannot be dropped: its pageblock stays untouched and the frame is counted.
fn anon_in_block() -> TestResult {
    let f = shared()?;
    refill(f);
    let anon = BASE + 100 * PAGE;
    retake(f, anon, 0, Kind::User)?;
    let (st, s) = run9(f)?;
    let calls = f.calls.load(Relaxed);
    let got = f.b.alloc_light(9, Zone::Normal, Kind::Other);
    let mut extra = alloc::vec![(anon, 0, Kind::User)];
    extra.extend(got.map(|p| (p, 9, Kind::Other)));
    empty(f, &extra);
    kassert!(st == Status::Success && got == Some(BASE + PB), "{st:?} {got:?}");
    kassert!(s.skipped_anon == 1, "skipped_anon {} (want 1)", s.skipped_anon);
    kassert!(calls == 512, "{calls} frames asked: the anonymous frame's pageblock must not be touched");
    Ok(())
}
ktest!("mm.compact.anon", Safe, anon_in_block);

/// What MM-2 brings: a migrator that copies, its targets from the free scanner at the top.
struct Mover(&'static Buddy);

impl Migrate for Mover {
    fn migrate(&self, phys: u64, kind: Kind, owner: u64, dest: &mut dyn FnMut() -> Option<u64>) -> Moved {
        let Some(d) = dest() else { return Moved::Stays };
        self.0.set_owner(d, owner);
        self.0.set_owner(phys, 0);
        self.0.free(phys, 0, kind);
        Moved::Moved
    }
}

/// Holes in the upper pageblocks, a copying migrator: the lowest pageblock moves up, nothing is dropped. Installs
/// the migrator on the staged allocator for good: it runs last of the staged tests (names sort).
fn x_free_scanner() -> TestResult {
    let f = shared()?;
    refill(f);
    for i in (2 * 512..PBS * 512).step_by(2) {
        let p = BASE + i * PAGE;
        f.b.set_owner(p, 0);
        f.b.free(p, 0, Kind::PageCache);
    }
    f.b.drain_all();
    let mover: &'static Mover = alloc::boxed::Box::leak(alloc::boxed::Box::new(Mover(f.b)));
    let installed = f.b.set_migrator(mover);
    let (st, s) = run9(f)?;
    let got = f.b.alloc_light(9, Zone::Normal, Kind::Other);
    empty(f, got.map(|p| (p, 9, Kind::Other)).as_slice());
    kassert!(installed || st == Status::Success, "migrator not installed");
    kassert!(st == Status::Success && got == Some(BASE), "{st:?} {got:?} {s:?}");
    kassert!(s.moved == 512 && s.dropped == 0 && s.free_scanned >= 512, "{s:?}");
    kassert!(f.calls.load(Relaxed) == 0, "a copying migrator dropped cache");
    Ok(())
}
ktest!("mm.compact.x_free_scanner", Safe, x_free_scanner);

/// The page cache's own reclaimer (vfs `reclaim_frame`), for `DropClean` on real pages.
struct Vfs;

impl Reclaim for Vfs {
    fn reclaim(&self, _pages: u64, _thorough: bool) -> Reclaimed {
        Reclaimed::default()
    }
    fn reclaim_frame(&self, phys: u64, owner: u64) -> bool {
        vfs::reclaim::reclaim_frame(phys, owner)
    }
}

/// Drop-if-clean on the real page cache: a clean page goes, a dirty one stays (decision: compaction does not write
/// back; MM-2 may migrate dirty pages with their dirty bit), as does a page someone holds.
fn dirty() -> TestResult {
    let Some(b) = mm_core::boot_buddy() else { return ktest::fail("no boot allocator") };
    let (cache, _pat) = crate::reclaim_ktest::pattern_file(4);
    crate::reclaim_ktest::check_file(&cache, 4)?;
    cache.write_at(PAGE, &1u64.to_le_bytes()).map_err(|e| alloc::format!("write: {e}"))?;
    let phys = |i: u64| cache.cached(i).and_then(|p| p.phys());
    let (Some(clean), Some(dirty), Some(held)) = (phys(0), phys(1), phys(2)) else { return ktest::fail("pages not cached") };
    let keep = cache.cached(2);
    let m = DropClean(&Vfs);
    let mut none = || None;
    let r_dirty = m.migrate(dirty, Kind::PageCache, b.owner(dirty), &mut none);
    let r_held = m.migrate(held, Kind::PageCache, b.owner(held), &mut none);
    let r_clean = m.migrate(clean, Kind::PageCache, b.owner(clean), &mut none);
    let r_anon = m.migrate(clean, Kind::User, 1, &mut none);
    drop(keep);
    kassert!(r_dirty == Moved::Stays, "dirty page: {r_dirty:?}");
    kassert!(r_held == Moved::Stays, "held page: {r_held:?}");
    kassert!(r_clean == Moved::Dropped, "clean page: {r_clean:?}");
    kassert!(r_anon == Moved::Unsupported, "anonymous frame: {r_anon:?}");
    kassert!(cache.cached(0).is_none(), "the clean page is still cached");
    crate::reclaim_ktest::check_file(&cache, 4)?;
    Ok(())
}
ktest!("mm.compact.dirty", Safe, dirty);

/// Takes every free block of order 9 or more (held as `Kind::Other`).
fn take_huge(f: &dyn FrameAlloc, held: &mut Vec<(u64, u32)>) {
    for order in (mm_core::HUGE_ORDER..=mm_core::MAX_ORDER).rev() {
        while held.len() < held.capacity() {
            let Some(p) = f.alloc_light(order, Zone::Normal, Kind::Other) else { break };
            held.push((p, order));
        }
    }
}

/// Memory fragmented on purpose: 64 MiB of clean cache read in, then every free block of 2 MiB or more held, so free
/// memory exists only in pieces; direct compaction brings a 2 MiB block back (from a pageblock of clean cache,
/// dropped until MM-2) and the cache still reads right.
fn restore() -> TestResult {
    let (Some(f), Some(b)) = (mm_core::frames(), mm_core::boot_buddy()) else { return ktest::fail("no frame allocator") };
    const PAGES: u64 = 16384;
    let (cache, _pat) = crate::reclaim_ktest::pattern_file(PAGES);
    crate::reclaim_ktest::check_file(&cache, PAGES)?;
    let mut held: Vec<(u64, u32)> = Vec::new();
    if held.try_reserve_exact((f.stats().total_bytes / (2 * MIB)) as usize + 64).is_err() {
        return ktest::fail("no room for the list of held blocks");
    }
    let s0 = b.compact_stats();
    let mut made = false;
    for _ in 0..8 {
        take_huge(f, &mut held);
        if b.has_free_block(mm_core::HUGE_ORDER) {
            continue;
        }
        made = b.compact_direct(mm_core::HUGE_ORDER) && b.has_free_block(mm_core::HUGE_ORDER);
        if made {
            break;
        }
    }
    let s1 = b.compact_stats();
    let blocks = held.len();
    for (p, o) in held {
        f.free(p, o, Kind::Other);
    }
    klog::event!(Info, "mm", "ktest.compact_restore", made = made, held_blocks = blocks, dropped = s1.dropped - s0.dropped, cleared = s1.cleared - s0.cleared,
        scanned = s1.migrate_scanned - s0.migrate_scanned, anon = s1.skipped_anon - s0.skipped_anon, score = b.fragmentation_score());
    kassert!(made, "no 2 MiB block came back: {:?}", mm_core::block_census(mm_core::HUGE_ORDER).map(|c| alloc::format!("{c}")));
    kassert!(s1.success > s0.success && s1.cleared > s0.cleared, "counts did not move: {s0:?} -> {s1:?}");
    crate::reclaim_ktest::check_file(&cache, PAGES)
}
ktest!("mm.compact.restore", QemuOnly, restore);

/// kcompactd wakes on a note (for an order a free block serves at once) and goes back to sleep: one daemon wake,
/// then none for 600 ms.
fn kcompactd_wakes() -> TestResult {
    let w0 = cp::DAEMON_WAKE.get();
    cp::wake_kcompactd(1);
    let t0 = time::now_ns();
    while cp::DAEMON_WAKE.get() == w0 && time::now_ns() - t0 < 1_000_000_000 {
        sched::sleep_ms(5);
    }
    let w1 = cp::DAEMON_WAKE.get();
    kassert!(w1 == w0 + 1, "kcompactd woke {} times in 1 s for one note", w1 - w0);
    kassert!(cp::pending_orders() == 0, "the order is still pending");
    sched::sleep_ms(600);
    kassert!(cp::DAEMON_WAKE.get() == w1, "kcompactd kept waking without a note");
    Ok(())
}
ktest!("mm.compact.kcompactd", QemuOnly, kcompactd_wakes);

/// Restores proactiveness when the test ends.
struct Knob(u32);

impl Drop for Knob {
    fn drop(&mut self) {
        cp::set_proactiveness(u64::from(self.0));
    }
}

/// vm.compaction_proactiveness 0: no proactive run, whatever the score; the knob refuses 101.
fn proactive_off() -> TestResult {
    let Some(b) = mm_core::boot_buddy() else { return ktest::fail("no boot allocator") };
    let _back = Knob(cp::proactiveness());
    kassert!(cp::set_proactiveness(0) && !cp::set_proactiveness(101), "knob range");
    kassert!(!b.should_proactive_compact(), "proactive work asked for at 0");
    let r0 = cp::PROACTIVE_RUNS.get();
    sched::sleep_ms(1200);
    kassert!(cp::PROACTIVE_RUNS.get() == r0, "{} proactive runs at proactiveness 0", cp::PROACTIVE_RUNS.get() - r0);
    Ok(())
}
ktest!("mm.compact.proactive_off", QemuOnly, proactive_off);
