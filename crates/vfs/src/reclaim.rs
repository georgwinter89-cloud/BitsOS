//! MM-9: page-cache reclaim, the Linux way but small (linux-7.3.1/mm/vmscan.c `shrink_folio_list` :1058,
//! `folio_check_references` :868). Only clean, unmapped pages of block file systems (FAT, ext4, block files)
//! go: their content is on the device and a later miss reads it again. tmpfs and memfd pages never go (no swap),
//! nor a page anyone else holds (a mapping's pin, a running read or write-back, a block request: the index's
//! reference is then not the only one) or a dirty one (the flusher writes it back first, `writeback`).
//!
//! Order: first the start-up preload's pages nobody used since (FS-13 marks them `PRELOADED`), then a clock over
//! every cache (second chance: a used page is passed over once and marked `COLD`, an unused one is taken),
//! which approximates "least recently used" without a list per page. Two rings name the caches (weakly, so a
//! dropped inode leaves by itself): every reclaimable cache, and the caches that may still hold unused preloaded
//! pages. That order is `shrink`'s. `reclaim_frame` takes the page of one frame, whatever its state: the heap and
//! compaction clear a block by position with it (W35 MM-3, in place of a copy until MM-2), counted apart.
//!
//! Callers: the allocator's slow path (`Mode::Direct`, through crate mm's `mm_core::reclaim::Reclaim`): try-locks
//! only and no allocation (the caller may hold any lock, a cache's own `vfs.inode.data` included); the
//! background thread `mm.kswapd` (`Mode::Background`). Both bounded: a light call looks at a budget of pages;
//! a thorough background one at every cached page at most twice; a thorough direct one at `DIRECT_SCAN` pages
//! and cache visits and `SPINS` lock tries in all (B4: the allocating thread's work does not grow with the cache;
//! the hands stay where it stopped, so the next round goes on from there, and kswapd does the exhaustive scan). Freed frames go back while the cache lock is held (as
//! truncate does); the ring lock is never held while a cache is locked (order vfs.inode.data > the rings).

use crate::pagecache::{frames_held, invalidate_clean, tmpfs_frames, Inner, Invalidated, COLD, PRELOADED, REFERENCED};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::{Mutex, MutexGuard, Spare, SpinLock, SpinLockGuard};

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

    /// Lists `c` (false: no memory for a longer list, even after reclaim; the cache is then not reclaimed through
    /// this ring). The list grows with the ring's lock dropped (`ksync::Spare`): it has an entry per cached file,
    /// and under the spin lock the heap could neither reclaim nor build a large block from single frames (a
    /// browser start doubled it to 256 KiB there, Mini-PC 12.11.2026). Thread context only (`PageCache::build`,
    /// `note_preloaded` under the cache's sleeping lock).
    pub(crate) fn add(&self, c: &Arc<Cache>) -> bool {
        let w = Arc::downgrade(c);
        let mut spare = Spare::new();
        loop {
            let mut s = self.slots.lock();
            if s.caches.len() == s.caches.capacity() {
                s.caches.retain(|w| w.strong_count() > 0);
                s.hand = 0;
            }
            if !spare.fits(&mut s.caches, 1) {
                drop(s);
                if spare.grow().is_err() {
                    return false;
                }
                continue;
            }
            s.caches.push(w);
            return true;
        }
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
            let step = visit(&c);
            release(c);
            match step {
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

    /// BL-0: every listed cache's clean pages that only the index holds leave (`drop_caches` bit 1).
    pub fn drop_clean(&self) -> Invalidated {
        let mut out = Invalidated::default();
        self.all.walk(Mode::Background, &mut |c| {
            let r = invalidate_clean(c, 0, u64::MAX);
            out.dropped += r.dropped;
            out.kept += r.kept;
            Step::Keep
        });
        out
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
        // A cache with unused preloaded pages was passed over while kswapd runs: kswapd holds it and is freeing
        // exactly those pages, so a light direct call does not go on to take used pages of other files for them
        // (each such call took a cluster of them, up to a whole read file in one allocation spike). Its caller
        // takes what kswapd freed instead (`busy`, `Buddy::alloc_reclaiming`), as Linux's direct reclaimers wait
        // for one another (`too_many_isolated` -> `reclaim_throttle`). A thorough call (the allocator's later
        // rounds) goes on with the clock: a slow kswapd in a large cache must not leave the allocation to the
        // reserve while other caches have clean pages. Another holder (a reader of the file) leaves the clock on.
        let yield_to_kswapd = mode == Mode::Direct && !thorough && out.busy > 0 && mm_core::reclaim::kswapd_running();
        if !done(&out, budget) && !yield_to_kswapd {
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

fn judge(p: &crate::cache_view::PageRef<'_>, only_preloaded: bool) -> Verdict {
    let st = p.state();
    if only_preloaded && st != PRELOADED {
        return Verdict::Pass;
    }
    if p.strong_count() > 1 {
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
            // The index holds the only reference (PF-4: checked again under the index's write side, which a
            // locked reader's new reference waits for; a reader may have taken one since `judge`): dropping it
            // frees the frame after a grace period (PF-5: lockless readers may still copy from it; the
            // allocator waits for that before it retries, `grace::expedite`).
            let Some(page) = g.pages.try_take_alone(i) else {
                out.held += 1;
                return false;
            };
            // Never waits here (this cache's lock is held; the allocation or kswapd waits afterwards).
            page.drop_nowait();
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
        let Some((i, v)) = g.pages.next_with(pos, |_, p| judge(p, true)) else {
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
        let next = g.pages.next_with(pos, |_, p| judge(p, false)).or_else(|| g.pages.next_with(0, |_, p| judge(p, false)));
        let Some((i, v)) = next else {
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

/// Lists a new block file system cache (`PageCache::build`); its id for frame owners (0: none).
pub(crate) fn register(c: &Arc<Cache>) -> u64 {
    if !GLOBAL.all.add(c) {
        NOT_LISTED.inc();
    }
    ids::add(c)
}

/// The cache of id `id` is gone (`PageCache`'s drop).
pub(crate) fn unregister(id: u64) {
    ids::remove(id);
}

/// A reference to a cache that reclaim took from a weak one: when it is the last (the file was dropped meanwhile)
/// the cache is freed after a grace period by the queue, not here: reclaim runs inside an allocation, maybe with
/// the caller's locks held (review item 7). A count of more than two means another holder besides the list's
/// owner; two holders dropping at once may still leave the drop here, rarely.
fn release(c: Arc<Cache>) {
    if Arc::strong_count(&c) > 2 {
        drop(c);
    } else {
        mm_core::grace::defer_arc_nowait(c, 0);
    }
}

/// The owner word of page `index` of cache `id` (`mm_core::reclaim::set_frame_owner`).
pub(crate) fn owner_word(id: u64, index: u64) -> u64 {
    if index >= 1 << INDEX_BITS { 0 } else { (id << INDEX_BITS) | index }
}

const INDEX_BITS: u32 = 40;

/// FS-STALL review item 4: takes back the page that frame `phys` holds, found through the frame's owner word
/// (cache id and index), when it is clean, unmapped and held by nobody else: the heap frees a block by its frames
/// instead of walking caches. Direct reclaim's rules: try-locks only, nothing allocated. True: the page went.
pub fn reclaim_frame(phys: u64, owner: u64) -> bool {
    let (id, index) = (owner >> INDEX_BITS, owner & ((1 << INDEX_BITS) - 1));
    let Some(c) = ids::get(id) else { return false };
    let mut spins = SPINS;
    // Some(state): the page went.
    let taken = match lock(&c, Mode::Direct, &mut spins) {
        Some(g) => {
            let st = g.pages.next_with(index, |i, p| (i == index && p.phys() == Some(phys) && p.strong_count() == 1 && !p.is_dirty()).then(|| p.state())).and_then(|(_, st)| st);
            st.filter(|_| g.pages.try_take_alone(index).map(|page| page.drop_nowait()).is_some())
        }
        None => None,
    };
    release(c);
    if let Some(st) = taken {
        PAGES.inc();
        FRAME_PAGES.inc();
        // Counted apart from `PRELOADED_PAGES`: that one tells the preload pass of pressure (`preloaded_taken`),
        // and a block cleared by position is none.
        if st == PRELOADED {
            FRAME_PRELOADED_PAGES.inc();
        }
    }
    taken.is_some()
}

klog::counter!(pub FRAME_PAGES, "vfs.reclaim.frame_pages");
klog::counter!(FRAME_PRELOADED_PAGES, "vfs.reclaim.frame_preloaded_pages");

/// What `migrate_frame` did with one page-cache frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Migrated {
    /// The page lives on the target now; the old frame goes back after a grace period.
    Moved,
    /// Dirty (CONTRACTS BL-3/MM-2: dirty pages stay; write-back cleans them first).
    Dirty,
    /// Someone besides the index holds it: a mapping's pin, a read, a block request (I/O), a write-back batch.
    Held,
    /// The file's lock or its index's write side was taken, or a thread sleeps for the lock (never waited for).
    Busy,
    /// The owner word names no page with this frame (freed meanwhile, being read in, a reused id).
    Stale,
    /// No target frame (the free scanner found none), or none it could use.
    NoTarget,
}

/// W35 MM-2 (CONTRACTS BL-3/MM-2): moves the clean, unmapped page frame `phys` holds (found through its owner word,
/// as `reclaim_frame`) to a target from `dest` (a `Target`: an allocated page-cache frame nobody else knows; it
/// either becomes the page's or goes back when its handle drops): a new `CachePage` on the target with the
/// bytes copied replaces the index entry under the file's lock (`vfs.inode.data`, try-lock) and the index's write
/// side (try), the epoch unchanged; the old page is retired through PF-5's grace queue, so a lockless reader still
/// holding it finishes on the old frame. Direct reclaim's rules: try-locks only; one page is allocated (the new
/// `CachePage`, a small heap object) with no spin lock held.
pub fn migrate_frame<'b>(phys: u64, owner: u64, dest: &mut dyn FnMut() -> Option<mm_core::Target<'b>>) -> Migrated {
    let (id, index) = (owner >> INDEX_BITS, owner & ((1 << INDEX_BITS) - 1));
    let Some(c) = ids::get(id) else { return Migrated::Stale };
    // One try, no spinning: a busy file's pages stay for this run. The scanner hands over frame after frame of
    // the same file, and `SPINS` tries for each (as a reclaim call has for all of its caches together) kept a
    // compaction run spinning for one file's lock for most of its time.
    // A thread sleeping for the lock (a read, a write, a write-back) goes first, as the anonymous side stands back
    // for `mm.as` (`aspace::migrate::waited_for`): a scanner taking the lock frame after frame would otherwise win
    // it every time its holder lets go (generation 137: waits for `vfs.inode.data` up to 1.57 s).
    let r = if c.contended() {
        MIGRATE_YIELDED.inc();
        Migrated::Busy
    } else {
        match c.try_lock() {
            Some(g) => migrate_locked(&g, index, phys, dest),
            None => Migrated::Busy,
        }
    };
    release(c);
    r
}

klog::counter!(pub MIGRATE_YIELDED, "mm.migrate.cache_yielded");

fn migrate_locked<'b>(g: &Inner, index: u64, phys: u64, dest: &mut dyn FnMut() -> Option<mm_core::Target<'b>>) -> Migrated {
    let Some(old) = g.pages.get(index).filter(|p| p.phys() == Some(phys)) else { return Migrated::Stale };
    // The index's reference and this one: anything more is a holder (pin, read, request, write-back).
    if Arc::strong_count(&old) > 2 {
        return Migrated::Held;
    }
    if old.is_dirty() {
        return Migrated::Dirty;
    }
    let Some(t) = dest() else { return Migrated::NoTarget };
    let Some(new) = old.moved_to(t) else { return Migrated::NoTarget };
    drop(old);
    let keep = new.clone();
    // On a refusal the new page drops with `keep` (its frame goes back); the old one stays.
    match g.pages.try_replace_alone(index, phys, new) {
        Ok(gone) => {
            if let Some(gone) = gone {
                keep.take_charge(&gone);
                gone.drop_nowait();
            }
            Migrated::Moved
        }
        Err(true) => Migrated::Busy,
        Err(false) => Migrated::Held,
    }
}


/// Ids of reclaimable caches: a slot per live cache, reused after its drop. A stale owner word naming a reused
/// slot finds another cache, whose page at that index has another frame: `reclaim_frame` checks the frame.
/// The free slots are chained through the table itself, so `remove` (a cache's drop, under any lock) never needs
/// room; the table grows with its lock dropped (`ksync::Spare`, as `Ring::add`).
mod ids {
    use super::Cache;
    use alloc::sync::{Arc, Weak};
    use alloc::vec::Vec;
    use ksync::{Spare, SpinLock};

    enum Slot {
        Live(Weak<Cache>),
        /// Free; the next free slot's index plus one (0: none).
        Free(u32),
    }

    struct Table {
        slots: Vec<Slot>,
        /// The first free slot's index plus one (0: none).
        free: u32,
    }

    static IDS: SpinLock<Table> = SpinLock::named("vfs.reclaim.ids", Table::new());

    /// Ids above 24 bits do not fit the owner word: such a cache is reclaimed by the clock only.
    const MAX: usize = (1 << 24) - 1;

    impl Table {
        const fn new() -> Table {
            Table { slots: Vec::new(), free: 0 }
        }
    }

    pub(super) fn add(c: &Arc<Cache>) -> u64 {
        add_in(&IDS, c)
    }

    pub(super) fn remove(id: u64) {
        remove_in(&IDS, id)
    }

    /// The live cache of `id`; never waits for the table (reclaim).
    pub(super) fn get(id: u64) -> Option<Arc<Cache>> {
        get_in(&IDS, id)
    }

    fn add_in(ids: &SpinLock<Table>, c: &Arc<Cache>) -> u64 {
        let w = Arc::downgrade(c);
        let mut spare = Spare::new();
        loop {
            let mut t = ids.lock();
            let head = t.free as usize;
            if let Some(&Slot::Free(next)) = head.checked_sub(1).and_then(|i| t.slots.get(i)) {
                t.free = next;
                t.slots[head - 1] = Slot::Live(w);
                return head as u64;
            }
            if t.slots.len() >= MAX {
                return 0;
            }
            if !spare.fits(&mut t.slots, 1) {
                drop(t);
                if spare.grow().is_err() {
                    return 0;
                }
                continue;
            }
            t.slots.push(Slot::Live(w));
            return t.slots.len() as u64;
        }
    }

    fn remove_in(ids: &SpinLock<Table>, id: u64) {
        if id == 0 {
            return;
        }
        let mut t = ids.lock();
        let i = (id - 1) as usize;
        if matches!(t.slots.get(i), Some(Slot::Live(_))) {
            let next = t.free;
            let gone = core::mem::replace(&mut t.slots[i], Slot::Free(next));
            t.free = id as u32;
            drop(t);
            drop(gone);
        }
    }

    fn get_in(ids: &SpinLock<Table>, id: u64) -> Option<Arc<Cache>> {
        let t = ids.try_lock()?;
        match t.slots.get(id.checked_sub(1)? as usize)? {
            Slot::Live(w) => w.upgrade(),
            Slot::Free(_) => None,
        }
    }

    /// Ids freed (under a spin lock, as a cache's drop may be) come back newest first, a freed id names nothing,
    /// a twice-freed one is not chained twice, and the table only grows when no slot is free.
    #[test]
    fn freed_ids_are_chained_and_reused() {
        let t = SpinLock::named("vfs.test.ids", Table::new());
        let held = SpinLock::named("vfs.test.ids.held", ());
        // tmpfs caches: never registered globally, so this table is the only one naming them.
        let files: Vec<crate::pagecache::PageCache> = (0..64).map(|_| crate::pagecache::PageCache::tmpfs(Arc::new(crate::pool::Pool::new("ids")))).collect();
        let caches: Vec<Arc<Cache>> = files.iter().map(|f| f.inner_arc().clone()).collect();
        let ids: Vec<u64> = caches.iter().map(|c| add_in(&t, c)).collect();
        assert_eq!(ids, (1..=64).collect::<Vec<u64>>());
        for (c, &id) in caches.iter().zip(&ids) {
            assert!(get_in(&t, id).is_some_and(|g| Arc::ptr_eq(&g, c)), "id {id} names its cache");
        }
        {
            let _g = held.lock();
            ids[..32].iter().for_each(|&id| remove_in(&t, id));
        }
        remove_in(&t, ids[0]);
        assert!(ids[..32].iter().all(|&id| get_in(&t, id).is_none()), "a freed id names no cache");
        let again: Vec<u64> = caches[..33].iter().map(|c| add_in(&t, c)).collect();
        assert_eq!(&again[..32], &(1..=32).rev().collect::<Vec<u64>>()[..], "newest first, each once");
        assert_eq!(again[32], 65, "a new slot only when none is free");
        assert_eq!(t.lock().slots.len(), 65);
    }
}

/// A cache got its first unused preloaded page (`fill_as`, under its lock).
pub(crate) fn note_preloaded(c: &Arc<Cache>) {
    if !GLOBAL.preloaded.add(c) {
        NOT_LISTED.inc();
    }
}

klog::counter!(DROP_PAGECACHE, "vfs.drop_caches.pagecache");
klog::counter!(DROP_SLAB, "vfs.drop_caches.slab");
static DROP_QUIET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// BL-0: what a write of `what` (1..=4) to `/proc/sys/vm/drop_caches` does (linux-7/fs/drop_caches.c:51
/// `drop_caches_sysctl_handler`). Bit 1 (`iterate_supers(drop_pagecache_sb)`, every super block and the block
/// devices' own): the clean pages nobody else holds leave every block file system cache and every block device
/// cache (ext4's metadata under `CachedDiskDev`, `block::bdev::DiskFile`), all of them listed in the reclaim
/// ring; dirty, mapped and held pages stay (`invalidate_clean`), tmpfs is never listed. Bit 2 (`drop_slab`): the
/// kernel heap's magazines and empty slabs go back (`mm_core::HEAP.shrink`), after the file systems gave back
/// what they can make again (`meta_shrink`: ext4's directory indexes; V2's dentries and inodes are no LRU
/// cache, they live while used). Bit 4 only quiets the event from then on,
/// as Linux's `stfu`. Thread context with no lock held: it takes each cache's data lock in turn.
pub fn drop_caches(what: u64, by: &str, pid: u64) -> Invalidated {
    let mut out = Invalidated::default();
    if what & 1 != 0 {
        out = GLOBAL.drop_clean();
        // The frames are free when the write returns (bounded wait for the grace period, no lock held).
        mm_core::grace::expedite();
        DROP_PAGECACHE.inc();
    }
    if what & 2 != 0 {
        crate::meta_shrink::run(crate::meta_shrink::Level::All);
        mm_core::HEAP.shrink();
        DROP_SLAB.inc();
    }
    if !DROP_QUIET.load(core::sync::atomic::Ordering::Relaxed) {
        klog::event!(Info, "vfs", "drop_caches", value = what, comm = by, pid = pid, dropped = out.dropped, kept = out.kept);
    }
    if what & 4 != 0 {
        DROP_QUIET.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    out
}

/// Page-cache drops since boot (`drop_caches` with bit 1).
pub fn drop_caches_done() -> u64 {
    DROP_PAGECACHE.get()
}

/// Reclaims up to `want` pages from every block file system cache (see the module docs), counted.
pub fn shrink(want: u64, mode: Mode, thorough: bool) -> Outcome {
    let o = GLOBAL.shrink(want, mode, thorough);
    // BIGDIR: memory the file systems can make again (directory indexes). Only from the background thread: the
    // allocator's own slow path may hold any lock.
    if mode == Mode::Background {
        crate::meta_shrink::run(if thorough { crate::meta_shrink::Level::Hard } else { crate::meta_shrink::Level::Idle });
    }
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
    /// Of `pages`, those taken by their frame (`reclaim_frame`: a block cleared by position, in no order), and
    /// the never-used preloaded ones among them (not part of `preloaded_pages`, which is `shrink`'s).
    pub frame_pages: u64,
    pub frame_preloaded_pages: u64,
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
        frame_pages: FRAME_PAGES.get(),
        frame_preloaded_pages: FRAME_PRELOADED_PAGES.get(),
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
/// and `file_pages`); 0 without a frame allocator (host tests). PF-5: frames that wait for a grace period count
/// as free (they left `file_pages` when they left the index, `CachePage::account_out`); the HighAtomic reserve
/// does not (no program gets it).
pub fn mem_available_bytes() -> u64 {
    let Some(s) = mm_core::frames().map(|f| f.stats()) else { return 0 };
    mm_core::reclaim::mem_available(s.total_bytes, s.usable_free_bytes().saturating_add(s.pending_bytes), file_pages().saturating_mul(4096))
}
