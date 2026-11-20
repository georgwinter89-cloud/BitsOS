//! MM2-NIGHT ktests of migration's edges (the checkers of MM2-NIGHT): a frame the scanner saw as anonymous that is
//! a page-cache frame by the time its holder is locked stays (the owner word and the kind are checked again under
//! the lock), a cache page whose file lock a thread sleeps for stands back, and a cache page whose new `Arc` finds
//! no heap stays with every count as before.

use crate::linux::{MAP_FIXED, MAP_SHARED};
use crate::migrate;
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use hal::paging;
use ktest::{kassert, ktest, TestResult};
use mm_core::compact::{Migrate, Moved};
use mm_core::Kind;

const P: u64 = 4096;
const RW: u64 = 3;
const BASE: u64 = 0x35_0000_0000;
const SEC: u64 = 1_000_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn phys_of(a: &AddressSpace, va: u64) -> Result<u64, String> {
    paging::translate(a.root(), va).map(|(p, _)| p & !(P - 1)).ok_or_else(|| format!("{va:#x} not mapped"))
}

fn page_cache_bytes() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[Kind::PageCache as usize])
}

/// Moves `phys` (a page-cache frame), retrying while its file is busy.
fn moved(phys: u64) -> Result<(), String> {
    let mut last = Moved::Stays;
    for _ in 0..1000 {
        last = migrate::migrate_frame(phys, Kind::PageCache);
        if last == Moved::Moved {
            return Ok(());
        }
        sched::yield_now();
    }
    Err(format!("{phys:#x} not moved: {last:?}"))
}

/// The race of the checkers' finding: the scanner read kind User and owner (space, va) of a frame; the program
/// unmapped it, the frame was handed out again as a page-cache frame and the same space maps that at the same
/// address. Handing the stale pair to the migrator is that moment. It must answer Elsewhere (Unsupported) without
/// touching the leaf. Before, only the target's copy (`Target::copy_from` checks the source's kind) stopped it: the
/// shared leaf was frozen and shot down on every CPU, then put back, and the frame counted as one without a target
/// (Stays, so the scanner gave up its chunk). Nothing but that copy check kept a cache frame from being freed.
fn stale_owner() -> TestResult {
    if vfs::root_ns().lookup("/dev/shm").is_err() {
        return ktest::fail("no /dev/shm in this image (memfds live there)");
    }
    let f = vfs::memfd::create(b"mm-migrate-stale", 0, vfs::Cred::ROOT).map_err(|x| format!("memfd: {x}"))?;
    let i = f.inode().ok_or("no inode")?.clone();
    i.truncate(P).map_err(|x| format!("size: {x}"))?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = a
        .sys_mmap_file(BASE, P, RW, MAP_SHARED | MAP_FIXED, 0, &mut |len| vfs::mmap::check(&f, RW, MAP_SHARED | MAP_FIXED, 0, len))
        .map_err(e)?;
    a.write_u64(m, 0x1111).map_err(e)?;
    let phys = phys_of(&a, m)?;
    kassert!(!mm_core::is_user_frame(phys), "a shared file page is a program frame: {phys:#x}");
    let b = mm_core::boot_buddy().ok_or("no boot allocator")?;
    let e0 = migrate::ANON_ELSEWHERE.get();
    let mut dest = || b.alloc_target(0, mm_core::Zone::Normal, Kind::User);
    let r = migrate::MIGRATOR.migrate(phys, Kind::User, a.anon_word(m), &mut dest);
    crate::aspace::migrate::drop_dying();
    kassert!(r == Moved::Unsupported && migrate::ANON_ELSEWHERE.get() > e0, "a page-cache frame under a stale owner word: {r:?}");
    kassert!(phys_of(&a, m)? == phys, "the mapping left the cache frame: {:#x}", phys_of(&a, m)?);
    kassert!(b.run_allocated(phys, 1, Kind::PageCache), "the cache frame {phys:#x} is no longer an allocated cache frame");
    // The mapping still is the file: a store reaches it, and the file's page reads back through the mapping.
    a.write_u64(m, 0x2222).map_err(e)?;
    let mut w = [0u8; 8];
    i.read_at(0, &mut w).map_err(e)?;
    kassert!(u64::from_le_bytes(w) == 0x2222, "the file reads {:#x} after a store through its mapping", u64::from_le_bytes(w));
    i.write_at(0, &0x3333u64.to_le_bytes()).map_err(e)?;
    kassert!(a.read_u64(m).map_err(e)? == 0x3333, "the mapping does not see the file's write");
    Ok(())
}
ktest!("mm.migrate.stale_owner", Safe, stale_owner);

/// A clean cache page whose file lock a thread sleeps for stays (Busy, `mm.migrate.cache_yielded`), as the
/// anonymous side stands back for `mm.as`; once the lock is free and nobody waits it moves.
fn cache_yield() -> TestResult {
    let (c, _pat) = crate::reclaim_ktest::pattern_file(4);
    crate::reclaim_ktest::check_file(&c, 4)?;
    let c = Arc::new(c);
    let old = c.cached(0).and_then(|p| p.phys()).ok_or("page 0 not cached")?;
    let y0 = vfs::reclaim::MIGRATE_YIELDED.get();
    let hold = c.hold_data_for_test();
    let c2 = c.clone();
    let h = sched::spawn("mm-migrate-waiter", sched::Class::Normal, move || {
        let _ = c2.size();
    })
    .map_err(e)?;
    let t0 = time::now_ns();
    let mut last = Moved::Stays;
    while vfs::reclaim::MIGRATE_YIELDED.get() == y0 && time::now_ns() - t0 < 2 * SEC {
        last = migrate::migrate_frame(old, Kind::PageCache);
        if last != Moved::Stays {
            break;
        }
        sched::sleep_ms(1);
    }
    let yielded = vfs::reclaim::MIGRATE_YIELDED.get() - y0;
    let still = c.cached(0).and_then(|p| p.phys()) == Some(old);
    drop(hold);
    kassert!(h.join_timeout(5 * SEC), "the waiting thread did not end");
    kassert!(last == Moved::Stays && still, "the page moved under a held file lock: {last:?}");
    kassert!(yielded > 0, "no migration stood back for the thread sleeping for the file's lock");
    moved(old)?;
    kassert!(c.cached(0).and_then(|p| p.phys()) != Some(old), "page 0 still at {old:#x} after the move");
    crate::reclaim_ktest::check_file(&c, 4)?;
    Ok(())
}
ktest!("mm.migrate.cache_yield", Safe, cache_yield);

/// A cache page whose new `Arc` finds no heap (`fail_next_move_for_test`) stays: NoTarget, the old page in the
/// index, the target frame and the cache's counts back as they were (eight tries: a leak would be eight frames).
fn cache_no_heap() -> TestResult {
    let (c, _pat) = crate::reclaim_ktest::pattern_file(2);
    crate::reclaim_ktest::check_file(&c, 2)?;
    let old = c.cached(0).and_then(|p| p.phys()).ok_or("page 0 not cached")?;
    let (n0, held0, bytes0) = (migrate::NO_TARGET.get(), vfs::pagecache::frames_held(), page_cache_bytes());
    for t in 0..8 {
        vfs::pagecache::fail_next_move_for_test();
        let r = migrate::migrate_frame(old, Kind::PageCache);
        kassert!(r == Moved::Stays, "try {t}: {r:?} with no heap for the page");
    }
    let (held1, bytes1) = (vfs::pagecache::frames_held(), page_cache_bytes());
    kassert!(migrate::NO_TARGET.get() - n0 == 8, "NoTarget counted {} times for 8 tries", migrate::NO_TARGET.get() - n0);
    kassert!(c.cached(0).and_then(|p| p.phys()) == Some(old), "the page left {old:#x}");
    kassert!(held1 < held0 + 8, "frames held by the cache {held0} -> {held1}: the failed pages kept theirs");
    kassert!(bytes1 < bytes0 + 8 * P, "page-cache frames {bytes0} -> {bytes1} bytes: the targets did not go back");
    moved(old)?;
    crate::reclaim_ktest::check_file(&c, 2)?;
    Ok(())
}
ktest!("mm.migrate.cache_no_heap", Safe, cache_no_heap);
