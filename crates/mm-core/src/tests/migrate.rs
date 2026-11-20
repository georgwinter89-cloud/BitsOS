//! W35 MM-2: the scanners with a migrator that keeps what it moves (crate mm's `Migrator` on the kernel), on a
//! fake address space: anonymous frames whose owner word names their page, entries frozen while the frame is
//! copied (a reader that meets a frozen entry waits, as a fault waits for the space's lock), pinned frames that
//! stay and count as skipped anonymous memory, Linux's `compaction_suitable` checks applying again, and a racing
//! reader that checks every page's contents through the entries during the runs. Plus the bench of a direct run
//! that moves a whole pageblock.
use super::*;
use crate::buddy::How;
use crate::compact::{Migrate, Moved};
use crate::Buddy;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const PB: u64 = 512 * PAGE;
const BASE: u64 = 2 * MIB;

/// One page of the fake space: its frame and whether a migration holds it frozen.
#[derive(Clone, Copy)]
struct Entry {
    phys: u64,
    frozen: bool,
}

/// A fake address space: page -> entry, and the frames' contents (one word per frame stands for its bytes).
struct Space {
    b: &'static Buddy,
    map: Mutex<BTreeMap<u64, Entry>>,
    mem: Mutex<BTreeMap<u64, u64>>,
    /// Frames something besides the space holds (a share count no entry explains, a device reference).
    pinned: Mutex<BTreeSet<u64>>,
    moves: AtomicU64,
}

impl Space {
    fn value(va: u64) -> u64 {
        0xA0_0000_0000 | va
    }

    /// The fake space's protocol, as crate mm's: find the page by the owner word, check the entry, freeze it,
    /// copy, put it back over the target, free the old frame.
    fn migrate_page<'b>(&self, phys: u64, owner: u64, dest: &mut dyn FnMut() -> Option<crate::Target<'b>>) -> Moved {
        let va = owner;
        if self.pinned.lock().unwrap().contains(&phys) {
            return Moved::Unsupported;
        }
        let Ok(mut map) = self.map.try_lock() else { return Moved::Stays };
        let Some(e) = map.get_mut(&va).filter(|e| e.phys == phys && !e.frozen) else { return Moved::Unsupported };
        let Some(t) = dest() else { return Moved::Stays };
        assert!(self.b.run_allocated(t.phys(), 1, Kind::User), "the target is an allocated user frame");
        let new = t.into_phys();
        e.frozen = true;
        let mut mem = self.mem.lock().unwrap();
        let v = mem.remove(&phys).expect("the frame holds the page");
        mem.insert(new, v);
        drop(mem);
        *e = Entry { phys: new, frozen: false };
        self.b.set_owner(new, va);
        self.b.set_owner(phys, 0);
        self.b.free(phys, 0, Kind::User);
        self.moves.fetch_add(1, SeqCst);
        Moved::Moved
    }

    /// Every page reads its own value through its entry; a frozen one is waited for (never read).
    fn check_all(&self) -> u64 {
        let map = self.map.lock().unwrap();
        let mem = self.mem.lock().unwrap();
        let mut bad = 0;
        for (&va, e) in map.iter() {
            assert!(!e.frozen, "an entry stayed frozen after its migration");
            if mem.get(&e.phys) != Some(&Self::value(va)) || !self.b.run_allocated(e.phys, 1, Kind::User) {
                bad += 1;
            }
        }
        bad
    }
}

struct FakeMigrator(&'static Space);

impl Migrate for FakeMigrator {
    fn migrate<'b>(&self, phys: u64, kind: Kind, owner: u64, dest: &mut dyn FnMut() -> Option<crate::Target<'b>>) -> Moved {
        match kind {
            Kind::User => self.0.migrate_page(phys, owner, dest),
            _ => Moved::Unsupported,
        }
    }
}

/// `mib` MiB of anonymous memory, every frame mapped in the fake space (page = frame offset), then every other
/// frame of the pageblocks from `holes_from` freed (targets for the free scanner, no two free frames buddies).
fn anon_space(mib: u64, holes_from: u64) -> (&'static Buddy, &'static Space) {
    let b: &'static BuddyFrames = Box::leak(Box::new(staged(&[free(BASE, BASE + mib * MIB)])));
    let b: &'static Buddy = b;
    let s: &'static Space = Box::leak(Box::new(Space {
        b,
        map: Mutex::new(BTreeMap::new()),
        mem: Mutex::new(BTreeMap::new()),
        pinned: Mutex::new(BTreeSet::new()),
        moves: AtomicU64::new(0),
    }));
    while let Some(p) = b.alloc(0, Zone::Normal, Kind::User) {
        let va = (p - BASE) / PAGE + 1;
        b.set_owner(p, va);
        s.map.lock().unwrap().insert(va, Entry { phys: p, frozen: false });
        s.mem.lock().unwrap().insert(p, Space::value(va));
    }
    for i in (holes_from * 512..mib * 256).step_by(2) {
        let p = BASE + i * PAGE;
        let va = (p - BASE) / PAGE + 1;
        s.map.lock().unwrap().remove(&va);
        s.mem.lock().unwrap().remove(&p);
        b.set_owner(p, 0);
        b.free(p, 0, Kind::User);
    }
    b.drain_all();
    assert!(b.set_migrator(Box::leak(Box::new(FakeMigrator(s)))));
    (b, s)
}

#[test]
fn anonymous_frames_move_and_pinned_ones_stay_counted() {
    let (b, s) = anon_space(16, 2);
    assert!(!b.has_free_block(1));
    // A pinned frame in the lowest pageblock: that block cannot come free, the next one does.
    s.pinned.lock().unwrap().insert(BASE + 7 * PAGE);
    assert!(b.compaction_suitable(0, 9), "half the upper pageblocks are free: enough targets");
    assert!(b.compact_direct(9));
    let st = b.compact_stats();
    assert_eq!(st.dropped, 0, "nothing dropped");
    assert_eq!(st.skipped_anon, 1, "the pinned frame, counted");
    assert!(st.moved >= 512, "a whole pageblock moved: {st:?}");
    assert_eq!(b.alloc(9, Zone::Normal, Kind::Other), Some(BASE + PB), "the second pageblock came free");
    assert_eq!(s.check_all(), 0, "every page reads its value through its entry");
    assert_eq!(b.owner(BASE + 7 * PAGE), 8, "the pinned frame kept its page");
    b.check().unwrap();
}

#[test]
fn without_targets_compaction_is_not_suitable() {
    // No free frame at all: Linux's watermark check (`compaction_suitable`) says no, as with a real migrator.
    let (b, _s) = anon_space(8, 8);
    assert!(!b.compaction_suitable(0, 9), "nothing free: no targets for a migrator that keeps pages");
    b.check().unwrap();
}

/// The lowest pageblock full of anonymous frames, the others kernel frames with every other frame free, so no
/// Movable pageblock holds a free frame: only a last try (`Buddy::compact_last`) finds targets.
fn between_kernel_frames() -> (&'static Buddy, &'static Space) {
    let b: &'static BuddyFrames = Box::leak(Box::new(staged(&[free(BASE, BASE + 8 * MIB)])));
    let b: &'static Buddy = b;
    let s: &'static Space = Box::leak(Box::new(Space {
        b,
        map: Mutex::new(BTreeMap::new()),
        mem: Mutex::new(BTreeMap::new()),
        pinned: Mutex::new(BTreeSet::new()),
        moves: AtomicU64::new(0),
    }));
    for _ in 0..512 {
        let p = b.alloc(0, Zone::Normal, Kind::User).unwrap();
        assert!(p < BASE + PB, "{p:#x}: the program's frames fill the lowest pageblock");
        let va = (p - BASE) / PAGE + 1;
        b.set_owner(p, va);
        s.map.lock().unwrap().insert(va, Entry { phys: p, frozen: false });
        s.mem.lock().unwrap().insert(p, Space::value(va));
    }
    let mut kernel = Vec::new();
    while let Some(p) = b.alloc(0, Zone::Normal, Kind::Other) {
        kernel.push(p);
    }
    for &p in kernel.iter().filter(|&&p| (p / PAGE) % 2 == 1) {
        b.free(p, 0, Kind::Other);
    }
    b.drain_all();
    assert!(b.set_migrator(Box::leak(Box::new(FakeMigrator(s)))));
    let unmovable = crate::MigrateType::Unmovable;
    assert_eq!((b.pageblock_type(BASE), b.pageblock_type(BASE + PB)), (Some(crate::MigrateType::Movable), Some(unmovable)));
    assert!(!b.has_free_block(4) && b.compaction_suitable(0, 4), "768 free frames, none beside another");
    (b, s)
}

/// W35 MM-2 (`Buddy::compact_last`): the first run finds no target; the last try takes its targets between the
/// kernel frames and makes the block.
#[test]
fn a_last_try_takes_its_targets_between_kernel_frames() {
    let (b, s) = between_kernel_frames();
    assert!(b.compact_direct(4), "no block of 16 frames though 16 pages only had to move between the kernel's frames");
    assert_eq!(s.moves.load(SeqCst), 16, "one chunk of the program's pageblock moved");
    let map = s.map.lock().unwrap();
    assert_eq!(map.values().filter(|e| e.phys >= BASE + PB).count(), 16, "into the free frames between the kernel's");
    drop(map);
    assert_eq!(b.pageblock_type(BASE + PB), Some(crate::MigrateType::Unmovable), "the kernel's pageblocks keep their type");
    assert_eq!(s.check_all(), 0);
    b.check().unwrap();
}

/// A last try has a budget as every run: the next one goes on where it stopped (positions of its own: it reads no
/// hints), so the chunks made earlier are not read again from the zone's start each time.
#[test]
fn last_tries_go_on_where_the_one_before_stopped() {
    let (b, s) = between_kernel_frames();
    let mut scanned = Vec::new();
    for n in 1..=8u64 {
        let before = b.compact_stats().migrate_scanned;
        assert!(b.compact_direct(4), "block {n}");
        assert_eq!(b.alloc(4, Zone::Normal, Kind::Other), Some(BASE + (n - 1) * 16 * PAGE));
        assert_eq!(s.moves.load(SeqCst), 16 * n, "one chunk a run");
        scanned.push(b.compact_stats().migrate_scanned - before);
    }
    assert!(scanned[7] <= scanned[3], "the migrate scanner read {scanned:?} frames run by run: more each time");
    assert_eq!(s.check_all(), 0);
    b.check().unwrap();
}

/// Its scanners met without a block, but it did not start at the zone's ends: it goes on there once (what earlier
/// tries passed may be movable now), and only a try that read from end to end has failed.
#[test]
fn a_last_try_that_meets_starts_at_the_ends_once() {
    let (b, s) = between_kernel_frames();
    // The first frame held in every chunk of the program's pageblock but the one at frame 320.
    for chunk in (0..32u64).filter(|c| *c != 20) {
        s.pinned.lock().unwrap().insert(BASE + chunk * 16 * PAGE);
    }
    assert!(b.compact_direct(4));
    assert_eq!(b.alloc(4, Zone::Normal, Kind::Other), Some(BASE + 320 * PAGE));
    // Chunk 3 can move now. The last tries' migrate scanner stands behind chunk 20.
    s.pinned.lock().unwrap().remove(&(BASE + 3 * 16 * PAGE));
    assert!(b.compact_direct(4), "the chunk below the cached position: {:?}", b.compact_stats());
    assert_eq!(b.alloc(4, Zone::Normal, Kind::Other), Some(BASE + 48 * PAGE));
    assert_eq!(s.moves.load(SeqCst), 32);
    // Nothing else can move: this try reads from end to end and fails.
    assert!(!b.compact_direct(4));
    assert_eq!(s.check_all(), 0);
    b.check().unwrap();
}

#[test]
fn a_racing_reader_sees_every_page_through_the_runs() {
    let (b, s) = anon_space(16, 2);
    let stop = Arc::new(AtomicBool::new(false));
    let (bad, looks, waits) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (st, bd, lk, wt) = (stop.clone(), bad.clone(), looks.clone(), waits.clone());
    let reader = std::thread::spawn(move || {
        while !st.load(SeqCst) {
            let pages: Vec<u64> = s.map.lock().unwrap().keys().copied().collect();
            for va in pages {
                // A frozen entry (or a migration holding the space) is waited for, as a fault waits for the lock.
                let Ok(map) = s.map.try_lock() else {
                    wt.fetch_add(1, SeqCst);
                    continue;
                };
                let Some(e) = map.get(&va).copied() else { continue };
                assert!(!e.frozen, "a reader under the space's lock met a frozen entry");
                let v = s.mem.lock().unwrap().get(&e.phys).copied();
                drop(map);
                lk.fetch_add(1, SeqCst);
                if v != Some(Space::value(va)) {
                    bd.fetch_add(1, SeqCst);
                }
            }
        }
    });
    // A run gives a pageblock up at the first page it finds the space's lock taken for, so a run that moves nothing
    // takes microseconds and four of them fit into one hold of the reader's (its list of the pages): 8 of 748 runs
    // of this test moved nothing. At least four runs, and on until one moved something.
    let mut runs = 0;
    while runs < 4 || (s.moves.load(SeqCst) == 0 && runs < 100_000) {
        b.compact_all(How::Manual, &mut || {});
        runs += 1;
    }
    stop.store(true, SeqCst);
    reader.join().unwrap();
    assert_eq!(bad.load(SeqCst), 0, "{} wrong reads of {} ({} waits)", bad.load(SeqCst), looks.load(SeqCst), waits.load(SeqCst));
    assert!(s.moves.load(SeqCst) > 0, "the runs moved nothing");
    assert_eq!(s.check_all(), 0);
    b.check().unwrap();
}

/// `cargo test -p mm-core --release migrate_bench -- --nocapture` for numbers: a direct run that moves one whole
/// pageblock of anonymous frames into the free scanner's targets (the fake space's copy is a map insert: the
/// number is the scanners' and the allocator's share of a move; the kernel adds a 4 KiB copy and a shootdown).
#[test]
fn migrate_bench() {
    let n = if cfg!(debug_assertions) { 2 } else { 32 };
    let (mut total, mut moved) = (std::time::Duration::ZERO, 0u64);
    for _ in 0..n {
        // A fresh space each time (set-up not timed): the lowest pageblock full of anonymous frames.
        let (b, s) = anon_space(16, 2);
        let t = Instant::now();
        assert!(b.compact_direct(9));
        total += t.elapsed();
        moved += s.moves.load(SeqCst);
        assert_eq!(s.check_all(), 0);
        b.check().unwrap();
    }
    let per_run_us = total.as_nanos() as f64 / n as f64 / 1000.0;
    let per_frame_ns = total.as_nanos() as f64 / moved.max(1) as f64;
    std::println!("migrate_bench: direct run moving one 2 MiB block of anonymous frames {per_run_us:.1} us, {per_frame_ns:.0} ns per moved frame incl. the fake copy ({moved} moved in {n} runs)");
    assert!(moved >= 512 * n);
    // No timing bound (INT-W35-MM2): in the ordinary `cargo test` run this shares the host with 134 other tests,
    // some of them spinning threads (the racing reader), and measured 106 us per frame against 0.5 us alone. Speed is
    // judged from the printed number of a run of this test alone, never by a pass/fail here.
}

/// A target is the allocator's handle for a block nobody else knows: it goes back when dropped, stays out when
/// handed over, and copies only from an allocated block of its own kind that is not itself.
#[test]
fn a_target_frees_on_drop_and_copies_only_from_its_kind() {
    let b = staged(&[free(BASE, BASE + 4 * MIB)]);
    let free0 = b.stats().free_bytes;
    let t = b.alloc_target(0, Zone::Normal, Kind::User).expect("a target");
    assert_eq!(b.stats().free_bytes, free0 - PAGE);
    assert!(b.run_allocated(t.phys(), 1, Kind::User));
    let src = b.alloc(0, Zone::Normal, Kind::User).expect("a source");
    let cache = b.alloc(0, Zone::Normal, Kind::PageCache).expect("a cache frame");
    // The host has no direct map: these only check the refusals; the copy itself is the kernel's.
    assert!(!t.copy_from(t.phys()), "a target never copies from itself");
    assert!(!t.copy_from(cache), "a page-cache frame is not a user frame");
    assert!(!t.copy_from(BASE + 3 * MIB), "a free frame is no source");
    assert!(t.copy_from(src), "an allocated user frame is");
    drop(t);
    assert_eq!(b.stats().free_bytes, free0 - 2 * PAGE, "the dropped target went back");
    let t = b.alloc_target(0, Zone::Normal, Kind::User).expect("a target");
    let kept = t.into_phys();
    assert!(b.run_allocated(kept, 1, Kind::User), "a handed-over target stays allocated");
    for (p, k) in [(kept, Kind::User), (src, Kind::User), (cache, Kind::PageCache)] {
        b.free(p, 0, k);
    }
    b.drain_all();
    assert_eq!(b.stats().free_bytes, free0);
    b.check().unwrap();
}
