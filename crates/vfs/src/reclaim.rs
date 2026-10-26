//! MM-9: page-cache reclaim, the Linux way but small (linux-6.6/mm/vmscan.c `shrink_folio_list` :1705,
//! `folio_check_references` :1524). Only clean, unmapped pages of block file systems (FAT, ext4, block files)
//! go: their content is on the device and a later miss reads it again. tmpfs and memfd pages never go (no swap),
//! nor a page anyone else holds (a mapping's pin, a running read or write-back, a block request: the index's
//! reference is then not the only one) or a dirty one (the flusher writes it back first, `writeback`).
//!
//! Order: first the start-up preload's pages nobody used since (FS-13 marks them `PRELOADED`), then a clock over
//! every cache (second chance: a used page is passed over once and marked `COLD`, an unused one is taken),
//! which approximates "least recently used" without a list per page. Two rings name the caches (weakly, so a
//! dropped inode leaves by itself): every reclaimable cache, and the caches that may still hold unused preloaded
//! pages.
//!
//! Callers: the allocator's slow path (`Mode::Direct`, through crate mm's `mm_core::reclaim::Reclaim`): try-locks
//! only and no allocation (the caller may hold any lock, a cache's own `vfs.inode.data` included); the
//! background thread `mm.kswapd` (`Mode::Background`). Both bounded: a light call looks at a budget of pages;
//! a thorough background one at every cached page at most twice; a thorough direct one at `DIRECT_SCAN` pages
//! and cache visits and `SPINS` lock tries in all (B4: the allocating thread's work does not grow with the cache;
//! the hands stay where it stopped, so the next round goes on from there, and kswapd does the exhaustive scan). Freed frames go back while the cache lock is held (as
//! truncate does); the ring lock is never held while a cache is locked (order vfs.inode.data > the rings).

use crate::pagecache::{frames_held, tmpfs_frames, Inner, COLD, PRELOADED, REFERENCED};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::{Mutex, MutexGuard, SpinLock, SpinLockGuard};

type Cache = Mutex<Inner>;

/// Who reclaims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The allocator's slow path: never waits for a lock.
    Direct,
    /// The background thread: may wait for a cache's lock.
    Background,
}

/// What one call did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Pages freed.
    pub freed: u64,
    /// Of those, unused preloaded pages.
    pub preloaded: u64,
    /// Pages looked at.
    pub scanned: u64,
    /// Pages left because someone else holds them (mapped, being read or written back).
    pub held: u64,
    /// Pages left because they are dirty.
    pub dirty: u64,
    /// Caches passed over because their lock was taken (direct reclaim only).
    pub busy: u64,
    /// The call's scan budget ran out before it had what it wanted: what it did not see may be reclaimable.
    pub cut: bool,
}

/// Pages a light call may look at per page it is asked for (Linux scans `nr_to_reclaim` << a priority).
pub const SCAN_RATIO: u64 = 32;
/// A light call's smallest budget.
pub const MIN_SCAN: u64 = 1024;
/// A thorough direct call's budget (pages looked at plus caches visited): about a millisecond of work; the
/// allocator's rounds go on from where the last one stopped.
#[cfg(not(test))]
pub const DIRECT_SCAN: u64 = 64 * 1024;
/// Host tests: small, so a bound test needs few pages.
#[cfg(test)]
pub const DIRECT_SCAN: u64 = 2 * MIN_SCAN;

/// A ring of caches with a hand (a clock over files).
pub struct Ring {
    slots: SpinLock<Slots>,
}

struct Slots {
    caches: Vec<Weak<Cache>>,
    hand: usize,
}

/// What a visit says about the cache it saw.
enum Step {
    Keep,
    /// It has nothing more for this ring (dead entries go by themselves).
    Drop,
    /// The call has what it wanted: keep the cache, leave the hand on it.
    Stop,
}

impl Ring {
    pub const fn new(name: &'static str) -> Ring {
        Ring { slots: SpinLock::named(name, Slots { caches: Vec::new(), hand: 0 }) }
    }

    /// Lists `c` (false: the list could not grow; the cache is then not reclaimed through this ring).
    pub(crate) fn add(&self, c: &Arc<Cache>) -> bool {
        let mut s = self.slots.lock();
        if s.caches.len() == s.caches.capacity() {
            s.caches.retain(|w| w.strong_count() > 0);
            s.hand = 0;
        }
        if s.caches.try_reserve(1).is_err() {
            return false;
        }
        s.caches.push(Arc::downgrade(c));
        true
    }

    /// Entries listed now (dead ones included until a walk meets them).
    pub fn len(&self) -> usize {
        self.slots.lock().caches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn slots(&self, mode: Mode) -> Option<SpinLockGuard<'_, Slots>> {
        match mode {
            Mode::Direct => spin_try(|| self.slots.try_lock()),
            Mode::Background => Some(self.slots.lock()),
        }
    }

    /// Visits each listed cache at most once, from the hand on; dead entries are taken out on the way.
    /// Returns false when the ring's lock was taken (direct reclaim).
    fn walk(&self, mode: Mode, visit: &mut dyn FnMut(&Arc<Cache>) -> Step) -> bool {
        let Some(n) = self.slots(mode).map(|s| s.caches.len()) else { return false };
        for _ in 0..n {
            let w = {
                let Some(mut s) = self.slots(mode) else { return false };
                if s.caches.is_empty() {
                    return true;
                }
                if s.hand >= s.caches.len() {
                    s.hand = 0;
                }
                s.caches[s.hand].clone()
            };
            let Some(c) = w.upgrade() else {
                self.take_out(mode, &w);
                continue;
            };
            match visit(&c) {
                Step::Keep => {
                    if let Some(mut s) = self.slots(mode) {
                        s.hand += 1;
                    }
                }
                Step::Drop => self.take_out(mode, &w),
                Step::Stop => return true,
            }
        }
        true
    }

    /// Takes `w`'s entry out (the one at the hand when it still is there, else the first one naming it).
    fn take_out(&self, mode: Mode, w: &Weak<Cache>) {
        let Some(mut s) = self.slots(mode) else { return };
        let hand = s.hand;
        let at = if s.caches.get(hand).is_some_and(|x| Weak::ptr_eq(x, w)) { Some(hand) } else { s.caches.iter().position(|x| Weak::ptr_eq(x, w)) };
        if let Some(i) = at {
            // The last entry moves into the slot: the hand stays and looks at it next.
            let gone = s.caches.swap_remove(i);
            drop(s);
            drop(gone);
        }
    }
}

/// Every reclaimable cache, and the caches that may hold unused preloaded pages.
pub struct Reclaimer {
    all: Ring,
    preloaded: Ring,
}

impl Reclaimer {
    pub const fn new() -> Reclaimer {
        Reclaimer { all: Ring::new("vfs.reclaim.caches"), preloaded: Ring::new("vfs.reclaim.preloaded") }
    }

    /// A private reclaimer over `caches` only (tests: the global one also sees every other file; a cache that
    /// is not reclaimable, tmpfs, is left out).
    pub fn of(caches: &[&crate::pagecache::PageCache]) -> Reclaimer {
        let r = Reclaimer::new();
        for c in caches.iter().filter(|c| c.reclaimable()) {
            r.all.add(c.inner_arc());
            if c.inner_arc().lock().preload_at.is_some() {
                r.preloaded.add(c.inner_arc());
            }
        }
        r
    }

    /// Caches listed (all, preloaded).
    pub fn listed(&self) -> (usize, usize) {
        (self.all.len(), self.preloaded.len())
    }

    /// Frees up to `want` pages: unused preloaded pages first, then the clock. `thorough`: the allocator's later
    /// rounds and kswapd's second try; in the background no page budget (every cached page looked at most
    /// twice), in direct reclaim `DIRECT_SCAN`.
    pub fn shrink(&self, want: u64, mode: Mode, thorough: bool) -> Outcome {
        let mut out = Outcome::default();
        if want == 0 {
            return out;
        }
        let light = want.saturating_mul(SCAN_RATIO).max(MIN_SCAN);
        let mut budget = match (thorough, mode) {
            (false, _) => light,
            (true, Mode::Direct) => light.max(DIRECT_SCAN),
            (true, Mode::Background) => u64::MAX,
        };
        // Direct: every cache visit costs one (the number of caches is unbounded, R1), and the lock tries of the
        // whole call are bounded (each cache still gets one try).
        let visit_cost = u64::from(mode == Mode::Direct);
        let mut spins = SPINS;
        let done = |out: &Outcome, budget: u64| out.freed >= want || budget == 0;
        self.preloaded.walk(mode, &mut |c| {
            if done(&out, budget) {
                return Step::Stop;
            }
            // In chunks: the cache lock is never held for more than `CHUNK` pages (a reader of the file, or a
            // direct reclaimer spinning for it, waits at most that long).
            loop {
                budget = budget.saturating_sub(visit_cost);
                let Some(mut g) = lock(c, mode, &mut spins) else {
                    out.busy += 1;
                    return Step::Keep;
                };
                let mut chunk = CHUNK.min(budget);
                let given = chunk;
                sweep_preloaded(&mut g, want, &mut chunk, &mut out);
                budget -= given - chunk;
                if g.preload_at.is_none() {
                    return Step::Drop;
                }
                drop(g);
                if chunk > 0 || done(&out, budget) {
                    return Step::Keep;
                }
            }
        });
        if !done(&out, budget) {
            self.all.walk(mode, &mut |c| {
                if done(&out, budget) {
                    return Step::Stop;
                }
                let mut turns = None;
                loop {
                    budget = budget.saturating_sub(visit_cost);
                    let Some(mut g) = lock(c, mode, &mut spins) else {
                        out.busy += 1;
                        return Step::Keep;
                    };
                    let turns = turns.get_or_insert((g.pages.len() as u64).saturating_mul(2));
                    let mut chunk = CHUNK.min(budget);
                    let given = chunk;
                    clock(&mut g, want, &mut chunk, turns, &mut out);
                    budget -= given - chunk;
                    drop(g);
                    if chunk > 0 || *turns == 0 || done(&out, budget) {
                        return Step::Keep;
                    }
                }
            });
        }
        out.cut = out.freed < want && budget == 0;
        out
    }
}

impl Default for Reclaimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Pages looked at per hold of a cache lock.
pub const CHUNK: u64 = 512;
/// Tries of a direct reclaimer for a lock someone holds (a chunk of the background thread lasts microseconds;
/// the caller's own lock is never freed by spinning, so the tries are bounded); for the cache locks the bound
/// is per call (`Reclaimer::shrink`), for the rings per acquisition.
const SPINS: u32 = 4096;

fn spin_try<T>(f: impl Fn() -> Option<T>) -> Option<T> {
    for _ in 0..SPINS {
        if let Some(g) = f() {
            return Some(g);
        }
        core::hint::spin_loop();
    }
    None
}

/// A cache's lock; direct reclaim tries once and then while `spins` (the call's) lasts.
fn lock<'a>(c: &'a Cache, mode: Mode, spins: &mut u32) -> Option<MutexGuard<'a, Inner>> {
    if mode == Mode::Background {
        return Some(c.lock());
    }
    loop {
        if let Some(g) = c.try_lock() {
            return Some(g);
        }
        if *spins == 0 {
            return None;
        }
        *spins -= 1;
        core::hint::spin_loop();
    }
}

/// What the scan does with one page.
enum Verdict {
    Take(u8),
    Held,
    Dirty,
    Pass,
}

fn judge(p: &Arc<crate::pagecache::CachePage>, only_preloaded: bool) -> Verdict {
    let st = p.state();
    if only_preloaded && st != PRELOADED {
        return Verdict::Pass;
    }
    if Arc::strong_count(p) > 1 {
        return Verdict::Held;
    }
    if p.is_dirty() {
        return Verdict::Dirty;
    }
    if st == REFERENCED {
        // Second chance (`folio_check_references`): used since the hand last passed.
        p.set_state(COLD);
        return Verdict::Pass;
    }
    Verdict::Take(st)
}

/// Counts and applies a verdict on page `i`; true when the page was taken.
fn apply(g: &mut Inner, i: u64, v: Verdict, out: &mut Outcome) -> bool {
    match v {
        Verdict::Take(st) => {
            // The index held the only reference (checked under this lock, which every new reference takes):
            // dropping it frees the frame now.
            drop(g.pages.remove(i));
            out.freed += 1;
            if st == PRELOADED {
                out.preloaded += 1;
            }
            true
        }
        Verdict::Held => {
            out.held += 1;
            false
        }
        Verdict::Dirty => {
            out.dirty += 1;
            false
        }
        Verdict::Pass => false,
    }
}

/// Takes unused preloaded pages from `g.preload_at` on; moves the mark past what it looked at.
fn sweep_preloaded(g: &mut Inner, want: u64, budget: &mut u64, out: &mut Outcome) {
    let Some(mut pos) = g.preload_at else { return };
    let mut left: Option<u64> = None;
    let mut at_end = false;
    while out.freed < want && *budget > 0 {
        let Some((i, v)) = g.pages.next_from(pos).map(|(i, p)| (i, judge(p, true))) else {
            at_end = true;
            break;
        };
        *budget -= 1;
        out.scanned += 1;
        let preloaded_left = matches!(v, Verdict::Held | Verdict::Dirty);
        apply(g, i, v, out);
        if preloaded_left {
            left.get_or_insert(i);
        }
        match i.checked_add(1) {
            Some(n) => pos = n,
            None => {
                at_end = true;
                break;
            }
        }
    }
    g.preload_at = if at_end { left } else { Some(left.map_or(pos, |l| l.min(pos))) };
}

/// The clock over one cache from its hand: at most two turns (the first may only mark used pages cold).
fn clock(g: &mut Inner, want: u64, budget: &mut u64, turns: &mut u64, out: &mut Outcome) {
    let mut pos = g.clock;
    while *turns > 0 && out.freed < want && *budget > 0 {
        let next = g.pages.next_from(pos).or_else(|| g.pages.next_from(0));
        let Some((i, v)) = next.map(|(i, p)| (i, judge(p, false))) else {
            *turns = 0;
            break;
        };
        *turns -= 1;
        *budget -= 1;
        out.scanned += 1;
        apply(g, i, v, out);
        pos = i.wrapping_add(1);
    }
    g.clock = pos;
}

static GLOBAL: Reclaimer = Reclaimer::new();

klog::counter!(PAGES, "vfs.reclaim.pages");
klog::counter!(PRELOADED_PAGES, "vfs.reclaim.preloaded_pages");
klog::counter!(SCANNED, "vfs.reclaim.scanned");
klog::counter!(DIRECT_CALLS, "vfs.reclaim.direct_calls");
klog::counter!(DIRECT_PAGES, "vfs.reclaim.direct_pages");
klog::counter!(BACKGROUND_CALLS, "vfs.reclaim.background_calls");
klog::counter!(BACKGROUND_PAGES, "vfs.reclaim.background_pages");
klog::counter!(EMPTY_CALLS, "vfs.reclaim.empty_calls");
klog::counter!(BUSY_CACHES, "vfs.reclaim.busy_caches");
klog::counter!(NOT_LISTED, "vfs.reclaim.not_listed");

/// Lists a new block file system cache (`PageCache::build`).
pub(crate) fn register(c: &Arc<Cache>) {
    if !GLOBAL.all.add(c) {
        NOT_LISTED.inc();
    }
}

/// A cache got its first unused preloaded page (`fill_as`, under its lock).
pub(crate) fn note_preloaded(c: &Arc<Cache>) {
    if !GLOBAL.preloaded.add(c) {
        NOT_LISTED.inc();
    }
}

/// Reclaims up to `want` pages from every block file system cache (see the module docs), counted.
pub fn shrink(want: u64, mode: Mode, thorough: bool) -> Outcome {
    let o = GLOBAL.shrink(want, mode, thorough);
    let (calls, pages) = match mode {
        Mode::Direct => (&DIRECT_CALLS, &DIRECT_PAGES),
        Mode::Background => (&BACKGROUND_CALLS, &BACKGROUND_PAGES),
    };
    calls.inc();
    pages.add(o.freed);
    PAGES.add(o.freed);
    PRELOADED_PAGES.add(o.preloaded);
    SCANNED.add(o.scanned);
    BUSY_CACHES.add(o.busy);
    if o.freed == 0 {
        EMPTY_CALLS.inc();
    }
    o
}

/// The counters since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub pages: u64,
    pub preloaded_pages: u64,
    pub scanned: u64,
    pub direct_calls: u64,
    pub direct_pages: u64,
    pub background_calls: u64,
    pub background_pages: u64,
    pub empty_calls: u64,
    pub busy_caches: u64,
    pub not_listed: u64,
    /// Caches listed now (all, with preloaded pages).
    pub listed: (usize, usize),
}

pub fn stats() -> Stats {
    Stats {
        pages: PAGES.get(),
        preloaded_pages: PRELOADED_PAGES.get(),
        scanned: SCANNED.get(),
        direct_calls: DIRECT_CALLS.get(),
        direct_pages: DIRECT_PAGES.get(),
        background_calls: BACKGROUND_CALLS.get(),
        background_pages: BACKGROUND_PAGES.get(),
        empty_calls: EMPTY_CALLS.get(),
        busy_caches: BUSY_CACHES.get(),
        not_listed: NOT_LISTED.get(),
        listed: GLOBAL.listed(),
    }
}

/// FS-13b: preloaded pages reclaim took since boot (one counter load; the preload pass stops when it grows).
pub fn preloaded_taken() -> u64 {
    PRELOADED_PAGES.get()
}

/// Page-cache pages of block file systems now (tmpfs excluded): Linux's file LRU pages, the cache part of
/// MemAvailable. Mapped and dirty ones are counted too, as Linux counts them.
pub fn file_pages() -> u64 {
    frames_held().saturating_sub(tmpfs_frames())
}

/// `/proc/meminfo` MemAvailable in bytes (`mm_core::reclaim::mem_available` over the frame allocator's numbers
/// and `file_pages`); 0 without a frame allocator (host tests).
pub fn mem_available_bytes() -> u64 {
    let Some(s) = mm_core::frames().map(|f| f.stats()) else { return 0 };
    mm_core::reclaim::mem_available(s.total_bytes, s.free_bytes, file_pages().saturating_mul(4096))
}
