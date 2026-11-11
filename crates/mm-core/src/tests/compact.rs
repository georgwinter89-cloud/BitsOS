//! W35 MM-3: compaction on fake maps: the migrate scanner's chunks and corners (a frame that stays, an anonymous
//! frame, a huge page, a kernel frame), skip hints and deferral, the free scanner feeding a migrator that moves,
//! whole-zone runs, the proactive score and Linux's fragmentation numbers.
use super::*;
use crate::buddy::{extfrag_for_order, fragmentation_index, Control, How};
use crate::compact::{self as cp, Migrate, Moved};
use crate::reclaim::{Reclaim, Reclaimed};
use crate::Buddy;
use std::boxed::Box;
use std::collections::BTreeSet;
use core::sync::atomic::Ordering::Relaxed;
use std::sync::Mutex;

/// The page cache: gives back a frame (straight to the allocator, no grace period on the host) unless pinned
/// (mapped, dirty or held: `reclaim_frame` says no).
pub(super) struct Cache {
    b: &'static Buddy,
    pinned: Mutex<BTreeSet<u64>>,
    calls: Mutex<u64>,
}

impl Reclaim for Cache {
    fn reclaim(&self, _pages: u64, _thorough: bool) -> Reclaimed {
        Reclaimed::default()
    }
    fn reclaim_frame(&self, phys: u64, owner: u64) -> bool {
        *self.calls.lock().unwrap() += 1;
        assert_eq!(owner, phys / PAGE + 1, "the owner noted for the frame");
        if self.pinned.lock().unwrap().contains(&phys) {
            return false;
        }
        self.b.set_owner(phys, 0);
        self.b.free(phys, 0, Kind::PageCache);
        true
    }
}

impl Cache {
    fn calls(&self) -> u64 {
        *self.calls.lock().unwrap()
    }
    pub(super) fn pin(&self, p: u64) {
        self.pinned.lock().unwrap().insert(p);
    }
}

/// `mib` MiB from 2 MiB (pageblock-aligned), every frame clean cache with its owner noted.
pub(super) fn cached(mib: u64) -> (&'static Buddy, &'static Cache) {
    cached_below_reserve(mib, 0)
}

/// `cached` with the top `reserve` pageblocks made the HighAtomic reserve first (they stay free).
pub(super) fn cached_below_reserve(mib: u64, reserve: u64) -> (&'static Buddy, &'static Cache) {
    let b: &'static BuddyFrames = Box::leak(Box::new(staged(&[free(2 * MIB, (2 + mib) * MIB)])));
    let b: &'static Buddy = b;
    assert_eq!(b.reserve_highatomic_in(0, reserve), reserve);
    let c: &'static Cache = Box::leak(Box::new(Cache { b, pinned: Mutex::new(BTreeSet::new()), calls: Mutex::new(0) }));
    while let Some(p) = b.alloc(0, Zone::Normal, Kind::PageCache) {
        b.set_owner(p, p / PAGE + 1);
    }
    assert!(!b.has_free_block(0));
    assert!(b.set_reclaimer(c));
    (b, c)
}

const PB: u64 = 512 * PAGE;
const BASE: u64 = 2 * MIB;

/// Frees the cache block at `p` and takes it back as `order`/`kind` (the only free memory: the same frames).
fn retake(b: &Buddy, p: u64, order: u32, kind: Kind) {
    for i in 0..1u64 << order {
        b.set_owner(p + i * PAGE, 0);
        b.free(p + i * PAGE, 0, Kind::PageCache);
    }
    b.drain_all();
    assert_eq!(b.alloc(order, Zone::Normal, kind), Some(p));
}

#[test]
fn compaction_drops_one_block_and_stops_once_it_exists() {
    let (b, c) = cached(4);
    assert!(!b.has_free_block(3));
    assert!(b.compact_direct(3));
    assert_eq!(c.calls(), 8, "the eight cache pages of one block, no more");
    assert!(b.has_free_block(3));
    let s = b.compact_stats();
    assert_eq!((s.stall, s.success, s.fail, s.dropped, s.cleared), (1, 1, 0, 8, 1));
    assert!(b.compact_direct(3), "a block exists: nothing to do");
    assert_eq!(c.calls(), 8);
    b.check().unwrap();
}

#[test]
fn frames_that_stay_keep_their_blocks_skipped_and_compaction_deferred() {
    let (b, c) = cached(4);
    for i in 0..1024 {
        c.pin(BASE + i * PAGE);
    }
    assert!(!b.compact_direct(9));
    assert_eq!(c.calls(), 2, "one refused page per pageblock, then the next pageblock");
    assert_eq!(b.skip_hints(), 2);
    let s = b.compact_stats();
    assert_eq!((s.stall, s.fail, s.success), (1, 1, 0));
    // Deferred once (Linux defer_shift 1: one attempt skipped), then the skip hints keep the scanner off.
    assert!(!b.compact_direct(9));
    assert!(!b.compact_direct(9));
    assert_eq!(c.calls(), 2, "deferred, then both pageblocks skipped");
    c.pinned.lock().unwrap().clear();
    b.reset_skip_hints();
    assert_eq!(b.skip_hints(), 0);
    let mut made = false;
    for _ in 0..4 {
        made |= b.compact_direct(9);
    }
    assert!(made, "with the hints cleared and the pins gone the block comes (after the deferral)");
    b.check().unwrap();
}

#[test]
fn one_frame_that_stays_spoils_only_its_chunk() {
    let (b, c) = cached(4);
    c.pin(BASE + 3 * PAGE);
    assert!(b.compact_direct(3));
    assert_eq!(c.calls(), 4 + 8, "three dropped and the pinned one in chunk 0, then all of chunk 1");
    assert_eq!(b.alloc(3, Zone::Normal, Kind::Other), Some(BASE + 8 * PAGE));
    assert_eq!(b.skip_hints(), 0, "the pageblock gave a block: no hint");
}

#[test]
fn anonymous_frames_are_skipped_and_counted() {
    let (b, c) = cached(4);
    retake(b, BASE + 2 * PAGE, 0, Kind::User);
    assert!(b.compact_direct(3));
    assert_eq!(c.calls(), 8, "chunk 0 is not touched: its anonymous frame cannot move before MM-2");
    assert_eq!(b.compact_stats().skipped_anon, 1);
    assert_eq!(b.alloc(3, Zone::Normal, Kind::Other), Some(BASE + 8 * PAGE));
}

#[test]
fn a_huge_page_or_a_kernel_frame_keeps_its_pageblock() {
    let (b, c) = cached(8);
    retake(b, BASE, 9, Kind::User);
    retake(b, BASE + PB + 77 * PAGE, 0, Kind::Heap);
    assert!(b.compact_direct(9));
    assert_eq!(c.calls(), 512, "only the third pageblock was cleared");
    assert_eq!(b.skip_hints(), 2, "the huge page's and the kernel frame's pageblocks are skipped next time");
    assert_eq!(b.alloc(9, Zone::Normal, Kind::Other), Some(BASE + 2 * PB));
    b.check().unwrap();
}

#[test]
fn no_compaction_where_reclaim_may_not_run() {
    let (b, c) = cached(4);
    {
        let _p = ksync::PreemptGuard::new();
        assert!(!b.compact_direct(3));
    }
    assert_eq!(c.calls(), 0);
    assert!(!b.compact_direct(0) && !b.compact_direct(crate::MAX_ORDER + 1));
    let none = staged(&[free(2 * MIB, 4 * MIB)]);
    let _all = take_all(&none, Zone::Normal);
    assert!(!none.compact_direct(3), "no reclaimer and no migrator: nothing can move");
}

/// A migrator that copies (what MM-2 brings): the target from the free scanner, the old frame freed.
struct Mover(&'static Buddy);

impl Migrate for Mover {
    fn migrate(&self, phys: u64, kind: Kind, owner: u64, dest: &mut dyn FnMut() -> Option<u64>) -> Moved {
        let Some(d) = dest() else { return Moved::Stays };
        assert!(d / PB > phys / PB, "the target lies above the block being cleared");
        self.0.set_owner(d, owner);
        self.0.set_owner(phys, 0);
        self.0.free(phys, 0, kind);
        Moved::Moved
    }
}

#[test]
fn the_free_scanner_feeds_a_moving_migrator_from_the_top() {
    let (b, c) = cached(16);
    // Holes in the upper six pageblocks (enough free frames for Linux's `compaction_suitable`): every other
    // frame, so no two free frames are buddies.
    for i in (2 * 512..8 * 512).step_by(2) {
        let p = BASE + i * PAGE;
        b.set_owner(p, 0);
        b.free(p, 0, Kind::PageCache);
    }
    b.drain_all();
    assert!(!b.has_free_block(1));
    let free = b.stats().free_bytes;
    assert!(b.set_migrator(Box::leak(Box::new(Mover(b)))));
    assert!(b.compact_direct(9));
    let s = b.compact_stats();
    assert_eq!((s.moved, s.dropped), (512, 0), "the lowest pageblock moved up, nothing dropped");
    assert!(s.free_scanned >= 512 && s.isolated >= 1024, "{s:?}");
    assert_eq!(c.calls(), 0, "a moving migrator does not drop cache");
    assert_eq!(b.stats().free_bytes, free, "moved, not freed");
    assert_eq!(b.alloc(9, Zone::Normal, Kind::Other), Some(BASE));
    b.check().unwrap();
}

#[test]
fn whole_zone_runs_clear_only_blocks_with_holes() {
    let (b, c) = cached(8);
    for i in (1..20).step_by(2) {
        let p = BASE + PB + i * PAGE;
        b.set_owner(p, 0);
        b.free(p, 0, Kind::PageCache);
    }
    let s = b.compact_all(How::Manual, &mut || {});
    assert_eq!(c.calls(), 502, "the pageblock with holes, not the full ones (that would be reclaim)");
    assert_eq!((s.cleared, s.dropped), (1, 502));
    assert!(b.has_free_block(9));
    b.check().unwrap();
}

static KNOB: Mutex<()> = Mutex::new(());

#[test]
fn proactive_compaction_follows_the_score_and_the_knob() {
    let _g = KNOB.lock().unwrap_or_else(|p| p.into_inner());
    let (b, c) = cached(8);
    for i in (512..1024).step_by(2) {
        let p = BASE + i * PAGE;
        b.set_owner(p, 0);
        b.free(p, 0, Kind::PageCache);
    }
    b.drain_all();
    assert!(b.fragmentation_score() > cp::score_wmark(false), "score {}", b.fragmentation_score());
    assert!(b.should_proactive_compact());
    assert!(cp::set_proactiveness(0));
    assert!(!b.should_proactive_compact(), "proactiveness 0: no proactive work");
    assert!(!cp::set_proactiveness(101));
    assert!(cp::set_proactiveness(20));
    assert_eq!((cp::score_wmark(true), cp::score_wmark(false)), (80, 90), "Linux's marks for 20");
    let s = b.compact_all(How::Proactive, &mut || {});
    assert_eq!((s.cleared, c.calls()), (1, 256));
    assert!(b.fragmentation_score() <= cp::score_wmark(true));
    assert!(!b.should_proactive_compact());
}

#[test]
fn kcompactd_work_makes_the_block_and_counts_as_daemon() {
    let (b, c) = cached(8);
    for i in 0..3 * 512 {
        c.pin(BASE + i * PAGE);
    }
    assert!(b.kcompactd_work(9, &mut || {}));
    let s = b.compact_stats();
    assert_eq!((s.daemon_wake, s.stall), (1, 0), "kcompactd's run is no stall");
    assert_eq!(s.daemon_migrate_scanned, s.migrate_scanned);
    assert!(s.migrate_scanned >= 4 * 512);
    assert_eq!(b.alloc(9, Zone::Normal, Kind::Other), Some(BASE + 3 * PB));
}

#[test]
fn a_run_resumes_where_its_budget_ended() {
    let (b, c) = cached(8);
    for i in 0..3 * 512 {
        c.pin(BASE + i * PAGE);
    }
    let mut ctl = Control::new(Some(9), How::Daemon, 0);
    let _inside = crate::reclaim::enter();
    assert_eq!(b.compact_zone(&mut ctl, 512), crate::buddy::Status::Continue);
    assert_eq!(b.compact_zone(&mut ctl, 512), crate::buddy::Status::Continue);
    assert_eq!(b.compact_zone(&mut ctl, 512), crate::buddy::Status::Continue);
    assert_eq!(b.compact_zone(&mut ctl, 512), crate::buddy::Status::Success);
    assert_eq!(ctl.stats().migrate_scanned, 4 * 512);
}

#[test]
fn fragmentation_numbers_match_linux() {
    let mut fb = [0u64; crate::ORDERS];
    assert_eq!((fragmentation_index(&fb, 9), extfrag_for_order(&fb, 9)), (0, 0), "no free memory");
    fb[0] = 100;
    fb[1] = 50;
    // Linux: 1000 - (1000 + 200 * 1000 / 512) / 150 = 991; all 200 free frames outside order-9 blocks.
    assert_eq!((fragmentation_index(&fb, 9), extfrag_for_order(&fb, 9)), (991, 100));
    fb[9] = 2;
    assert_eq!((fragmentation_index(&fb, 9), extfrag_for_order(&fb, 9)), (-1000, 16), "(1224 - 1024) * 100 / 1224");
    // Few large pieces: lack of memory (below extfrag_threshold 500), not fragmentation.
    let mut few = [0u64; crate::ORDERS];
    few[8] = 3;
    assert_eq!(fragmentation_index(&few, 9), 1000 - (1000 + 768 * 1000 / 512) / 3);
    assert!(fragmentation_index(&few, 9) < 500);
}

#[test]
fn a_rebuilt_allocator_starts_without_hints() {
    let (b, c) = cached(4);
    for i in 0..1024 {
        c.pin(BASE + i * PAGE);
    }
    assert!(!b.compact_direct(9));
    assert_eq!(b.skip_hints(), 2);
    // Reload rebuilds the buddy over a zeroed blob: the hints are not handed over.
    let (fresh, _) = cached(4);
    assert_eq!(fresh.skip_hints(), 0);
}

/// A page cache that frees a frame only while it still owns it under the same owner word (vfs `reclaim_frame`'s
/// `try_take_alone`: a stale owner the scanner read before the frame changed hands is refused).
struct RacyCache {
    b: &'static Buddy,
    owned: Mutex<std::collections::BTreeMap<u64, u64>>,
    dropped: std::sync::atomic::AtomicU64,
}

impl Reclaim for RacyCache {
    fn reclaim(&self, _pages: u64, _thorough: bool) -> Reclaimed {
        Reclaimed::default()
    }
    fn reclaim_frame(&self, phys: u64, owner: u64) -> bool {
        let mut owned = self.owned.lock().unwrap();
        if owned.get(&phys) != Some(&owner) {
            return false;
        }
        owned.remove(&phys);
        self.b.set_owner(phys, 0);
        self.b.free(phys, 0, Kind::PageCache);
        self.dropped.fetch_add(1, Relaxed);
        true
    }
}

#[test]
fn compaction_racing_allocations_frees_and_kcompactd_keeps_the_frames_exact() {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    // 32 MiB: three threads keep allocating and freeing cache, anonymous and kernel frames in it while direct runs,
    // kcompactd runs and whole-zone runs scan the same pageblocks.
    let b: &'static BuddyFrames = Box::leak(Box::new(staged(&[free(16 * MIB, 48 * MIB)])));
    let b: &'static Buddy = b;
    let total = b.stats().free_bytes;
    let c: &'static RacyCache = Box::leak(Box::new(RacyCache { b, owned: Mutex::new(Default::default()), dropped: AtomicU64::new(0) }));
    assert!(b.set_reclaimer(c));
    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let next_owner: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(1)));
    const KINDS: [Kind; 3] = [Kind::PageCache, Kind::User, Kind::Heap];
    let workers: Vec<_> = (0..3u64)
        .map(|t| {
            std::thread::spawn(move || {
                let mut rng = Rng(0xc0de_0000 + t);
                let mut mine: Vec<(u64, u32, Kind)> = Vec::new();
                let mut cache: Vec<u64> = Vec::new();
                for _ in 0..12_000 {
                    if rng.below(3) != 0 {
                        let k = KINDS[rng.below(3) as usize];
                        let o = if k == Kind::PageCache { 0 } else { rng.below(3) as u32 };
                        if let Some(p) = b.alloc(o, Zone::Normal, k) {
                            if k == Kind::PageCache {
                                let g = next_owner.fetch_add(1, Relaxed);
                                c.owned.lock().unwrap().insert(p, g);
                                b.set_owner(p, g);
                                cache.push(p);
                            } else {
                                mine.push((p, o, k));
                            }
                        }
                    } else if rng.below(2) == 0 && !cache.is_empty() {
                        let p = cache.swap_remove(rng.below(cache.len() as u64) as usize);
                        // Gone already when compaction dropped it.
                        if c.owned.lock().unwrap().remove(&p).is_some() {
                            b.set_owner(p, 0);
                            b.free(p, 0, Kind::PageCache);
                        }
                    } else if !mine.is_empty() {
                        let (p, o, k) = mine.swap_remove(rng.below(mine.len() as u64) as usize);
                        b.free(p, o, k);
                    }
                }
                for (p, o, k) in mine {
                    b.free(p, o, k);
                }
            })
        })
        .collect();
    let daemon = std::thread::spawn(move || {
        let mut runs = 0u64;
        while !stop.load(Relaxed) {
            b.kcompactd_work(4, &mut std::thread::yield_now);
            b.compact_all(How::Manual, &mut std::thread::yield_now);
            runs += 1;
        }
        runs
    });
    let mut direct = 0u64;
    while !workers.iter().all(|h| h.is_finished()) {
        for order in [2, 5, 9] {
            b.compact_direct(order);
            direct += 1;
        }
    }
    for h in workers {
        h.join().unwrap();
    }
    stop.store(true, Relaxed);
    let runs = daemon.join().unwrap();
    // The cache gives back what it still owns; then every frame must be free again, once.
    let left: Vec<(u64, u64)> = c.owned.lock().unwrap().iter().map(|(&p, &o)| (p, o)).collect();
    for (p, owner) in left {
        assert!(c.reclaim_frame(p, owner));
    }
    b.drain_all();
    b.check().unwrap();
    assert_eq!(b.stats().free_bytes, total, "every frame back exactly once");
    assert!(b.has_free_block(9));
    let s = b.compact_stats();
    assert!(direct > 0 && runs > 0 && s.migrate_scanned > 0, "direct {direct}, daemon {runs}, {s:?}");
    std::println!("racing compaction: {direct} direct runs, {runs} daemon rounds, {} dropped by compaction, {s:?}", c.dropped.load(Relaxed));
}
