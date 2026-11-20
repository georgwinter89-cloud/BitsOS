//! W35 MM-3: memory compaction (Linux 7.x mm/compaction.c: `compact_zone`, `compact_finished`,
//! `isolate_migratepages_block`, `isolate_freepages_block`, `isolation_suitable` / `update_pageblock_skip` /
//! `reset_isolation_suitable`, `compaction_suitable` / `__compaction_suitable`, `fragmentation_score_zone` /
//! `_node`, `defer_compaction` / `compaction_deferred` / `compaction_defer_reset`, `kcompactd_do_work`,
//! `compact_node`, `try_to_compact_pages`; mm/vmstat.c `fill_contig_page_info`, `__fragmentation_index`,
//! `extfrag_for_order`).
//!
//! Two scanners per zone: the migrate scanner walks up from the bottom over Movable pageblocks and hands every
//! movable frame of a block it means to clear to the `Migrate` interface; the free scanner walks down from the top
//! and isolates free frames as targets, lazily, only when a migrator asks for one (`compaction_alloc`). A run stops
//! when a free block of the wanted order exists, the scanners meet, its budget is spent or (drop-only migrator)
//! its drop cap is reached.
//!
//! The migrate scanner works in aligned chunks of the wanted order (a pageblock for whole-zone runs): before it
//! moves anything it reads the chunk's frame states (lock-free, a hint: the retry decides) and touches the chunk
//! only when every frame is free, on a per-CPU list or an order-0 frame the migrator can move. Linux isolates
//! every movable page of a block regardless; where the only migrator drops clean cache (`DropClean`: no copying
//! migrator installed), a frame dropped in a block that cannot end up free is cache lost for nothing. Whole-zone
//! runs (compact_memory, proactive) with a dropping migrator clear only chunks that hold a free frame already:
//! clearing a block full of cache would be reclaim, not compaction.
//!
//! Pageblock skip hints (Linux `PB_migrate_skip`): one bit per pageblock in the region's side array, set where a
//! scanner found nothing to take (a frame that stays, an unmovable frame, no free target), honoured by direct and
//! kcompactd runs, ignored by whole-zone runs (both scanners: `Targets`), cleared when the scanners of a direct or
//! kcompactd run met and kcompactd goes to sleep (Linux: kswapd going to sleep with `compact_blockskip_flush`, set
//! by direct runs only). A pageblock that is free whole gets none.
//! Rebuilt zeroed with the buddy at boot and Reload: nothing to hand over.

use super::{zone_index, Buddy, Region, ZONES};
use super::group::{MigrateType, PAGEBLOCK_FRAMES, PAGEBLOCK_ORDER};
use crate::compact::{self as cp, DropClean, Migrate, Moved};
use crate::meta::{Meta, State};
use crate::{FrameAlloc, Kind, Zone, MAX_ORDER, PAGE};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

/// Linux `PAGE_ALLOC_COSTLY_ORDER`.
const COSTLY_ORDER: u32 = 3;
/// Linux `COMPACT_MAX_DEFER_SHIFT`.
pub const MAX_DEFER_SHIFT: u32 = 6;
/// Free frames the free scanner isolates per step (Linux `COMPACT_CLUSTER_MAX`).
const CLUSTER: usize = 32;
/// Frames the migrate scanner reads per direct run (1 GiB of frame states, about a millisecond; a chunk that
/// fails at its first frame counts whole).
pub const DIRECT_SCAN: u64 = 1 << 18;
/// Frames the free scanner reads per slice of any run (256 MiB of frame states): the run then stops where it
/// is, the next one goes on from there (the cached positions). 12.11.2026, Mini-PC generation 137: the free
/// scanner had no bound and no cached position, every run walked down from the top of the zone again, 1.2
/// billion frames in 30 minutes.
pub const FREE_SCAN: u64 = 1 << 16;
/// Frames one direct run may drop (16 MiB of clean cache, as kswapd's old `ORDER_PAGES`).
pub const DIRECT_DROP: u64 = 4096;
/// Frames per slice of a background run (kcompactd yields between slices).
pub const SLICE: u64 = 1 << 16;
/// Slices one pass of kcompactd for an order makes at most (2 GiB of frame states for each scanner); what is left
/// of the zone is the next pass's, from the cached positions.
pub const PASS_SLICES: u32 = 8;
/// Frames one kcompactd run for an order may drop while the migrator drops (64 MiB).
pub const DAEMON_DROP: u64 = 4 * DIRECT_DROP;
/// Frames one proactive run may drop while the migrator drops (it compacts for the score, nobody waits).
pub const PROACTIVE_DROP: u64 = 4096;

/// Who started a run (Linux `compact_control`'s flags).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// An allocation that may sleep (`__alloc_pages_direct_compact`): skip hints, cached positions, bounded.
    Direct,
    /// kcompactd for a wanted order (`kcompactd_do_work`).
    Daemon,
    /// kcompactd for the fragmentation score (`proactive_compact_node`): the whole zone pass by pass, until the
    /// low mark.
    Proactive,
    /// /proc/sys/vm/compact_memory (`compact_node`): whole zone.
    Manual,
}

/// Where the free scanner takes its targets and what the skip hints count for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Targets {
    /// Movable pageblocks without a skip hint; one that gave nothing gets the hint (runs for an order).
    Hinted,
    /// Movable pageblocks, hints neither read nor set: whole-zone runs (Linux `compact_node` sets
    /// `ignore_skip_hint`, which `isolation_suitable` obeys for both scanners). 12.11.2026: hints left by the runs of
    /// a moment before kept `compact_memory` off nine pageblocks in ten.
    Unhinted,
    /// Every pageblock but the reserve's, hints neither read nor set (a last try, Linux `ignore_block_suitable`).
    Any,
}

impl How {
    fn whole(self) -> bool {
        matches!(self, How::Proactive | How::Manual)
    }
    fn daemon(self) -> bool {
        matches!(self, How::Daemon | How::Proactive)
    }
}

/// How a run ended (Linux `enum compact_result`, the cases BitsOS tells apart).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not run: no migrator, deferred, not suitable, empty zone.
    Skipped,
    /// The budget is spent: call again with the same `Control`.
    Continue,
    /// A free block of the order exists (proactive: the score reached the low mark).
    Success,
    /// The scanners met.
    Complete,
    /// The drop cap was reached.
    Partial,
}

/// Per-zone state kept between runs (Linux `zone->compact_cached_*`, `compact_considered`, `compact_defer_shift`,
/// `compact_order_failed`, `compact_blockskip_flush`).
pub(super) struct ZoneCompact {
    migrate_pfn: AtomicU64,
    free_pfn: AtomicU64,
    /// Where the last tries stopped (`Buddy::compact_last`): they read no hints, so they keep positions of their
    /// own, and one bounded try after the other goes round the zone.
    last_migrate_pfn: AtomicU64,
    last_free_pfn: AtomicU64,
    /// Where the proactive runs stopped: each is one pass (`Buddy::pass`), the next goes on.
    pro_migrate_pfn: AtomicU64,
    pro_free_pfn: AtomicU64,
    considered: AtomicU32,
    defer_shift: AtomicU32,
    order_failed: AtomicU32,
    flush: AtomicBool,
}

impl ZoneCompact {
    /// The cached scanner positions `c` starts from and stops at (migrate, free): the hinted runs', the last
    /// tries' or the proactive runs'; none for compact_memory (one run from end to end).
    fn cached(&self, c: &Control) -> Option<(&AtomicU64, &AtomicU64)> {
        match c.how {
            _ if c.last => Some((&self.last_migrate_pfn, &self.last_free_pfn)),
            How::Proactive => Some((&self.pro_migrate_pfn, &self.pro_free_pfn)),
            How::Manual => None,
            How::Direct | How::Daemon => Some((&self.migrate_pfn, &self.free_pfn)),
        }
    }

    pub(super) const fn new() -> Self {
        ZoneCompact {
            migrate_pfn: AtomicU64::new(0),
            free_pfn: AtomicU64::new(0),
            last_migrate_pfn: AtomicU64::new(0),
            last_free_pfn: AtomicU64::new(0),
            pro_migrate_pfn: AtomicU64::new(0),
            pro_free_pfn: AtomicU64::new(0),
            considered: AtomicU32::new(0),
            defer_shift: AtomicU32::new(0),
            order_failed: AtomicU32::new(MAX_ORDER + 1),
            flush: AtomicBool::new(false),
        }
    }
}

/// Counts of this allocator's runs (the boot allocator's also go to the global `compact::*` counters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactStats {
    pub stall: u64,
    pub fail: u64,
    pub success: u64,
    pub migrate_scanned: u64,
    pub free_scanned: u64,
    pub isolated: u64,
    pub daemon_wake: u64,
    pub daemon_migrate_scanned: u64,
    pub daemon_free_scanned: u64,
    pub skipped_anon: u64,
    pub dropped: u64,
    pub moved: u64,
    /// Chunks the migrate scanner cleared.
    pub cleared: u64,
}

const N: usize = 13;

pub(super) struct CompactCounters([AtomicU64; N]);

impl CompactCounters {
    pub(super) const fn new() -> Self {
        CompactCounters([const { AtomicU64::new(0) }; N])
    }
    fn add(&self, i: usize, v: u64) {
        if v > 0 {
            self.0[i].fetch_add(v, Relaxed);
        }
    }
    fn get(&self) -> CompactStats {
        let g = |i: usize| self.0[i].load(Relaxed);
        CompactStats {
            stall: g(0),
            fail: g(1),
            success: g(2),
            migrate_scanned: g(3),
            free_scanned: g(4),
            isolated: g(5),
            daemon_wake: g(6),
            daemon_migrate_scanned: g(7),
            daemon_free_scanned: g(8),
            skipped_anon: g(9),
            dropped: g(10),
            moved: g(11),
            cleared: g(12),
        }
    }
}

/// How a last try ended (`Buddy::compact_last`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    Made,
    /// Its scanners met without a block (or the migrator drops: no last try).
    Failed,
    /// Its budget ended first.
    Stopped,
}

/// The free scanner of a run.
struct FreeScan {
    /// The end of what it has not read yet (a pageblock's end or the zone's); it reads downwards.
    pfn: u64,
    /// Where it goes on inside the pageblock below `pfn` (0: at the pageblock's start).
    resume: u64,
    /// Target frames taken and not used yet.
    frames: [u64; CLUSTER],
    n: usize,
    /// Frames it may still read in this slice (`FREE_SCAN`).
    left: u64,
}

impl FreeScan {
    fn at(&mut self, pfn: u64) {
        (self.pfn, self.resume) = (pfn, 0);
    }
}

/// One run over one zone (Linux `struct compact_control`).
pub struct Control {
    order: Option<u32>,
    how: How,
    zone: usize,
    started: bool,
    migrate_pfn: u64,
    fs: FreeScan,
    /// Frames dropped or moved by this run whose frames have not been waited for yet.
    unsettled: u64,
    drop_cap: u64,
    grace: crate::grace::WaitBudget,
    /// The pageblock the migrate scanner is in and whether a chunk of it came free (skip hint below the pageblock
    /// order: set when none did).
    pb_at: u64,
    pb_good: bool,
    pb_bad: bool,
    /// The last try after a run whose scanners met (`Buddy::compact_last`).
    last: bool,
    /// A last try that did not start at the zone's ends starts there once when its scanners meet (`wrapped`).
    wrap: bool,
    wrapped: bool,
    s: CompactStats,
}

impl Control {
    /// `order`: the block wanted (None: whole zone, pageblock chunks).
    pub fn new(order: Option<u32>, how: How, zone: usize) -> Self {
        Control {
            order: order.map(|o| o.min(MAX_ORDER)),
            how,
            zone: zone.min(ZONES - 1),
            started: false,
            migrate_pfn: 0,
            fs: FreeScan { pfn: 0, resume: 0, frames: [0; CLUSTER], n: 0, left: 0 },
            unsettled: 0,
            drop_cap: u64::MAX,
            grace: crate::grace::WaitBudget::default(),
            pb_at: u64::MAX,
            pb_good: false,
            pb_bad: false,
            last: false,
            wrap: false,
            wrapped: false,
            s: CompactStats::default(),
        }
    }

    /// What this run counted so far.
    pub fn stats(&self) -> CompactStats {
        self.s
    }

    /// The run starts at the zone's ends and neither reads nor sets skip hints (Linux `whole_zone` and
    /// `ignore_skip_hint`): whole-zone runs and a last try.
    fn unhinted(&self) -> bool {
        self.how.whole() || self.last
    }

    fn chunk(&self) -> u64 {
        match self.order {
            Some(o) if o > 0 => 1 << o,
            Some(_) => 1,
            None => PAGEBLOCK_FRAMES,
        }
    }
}

fn kind_of(i: usize) -> Option<Kind> {
    if i == Kind::User as usize {
        Some(Kind::User)
    } else if i == Kind::PageCache as usize {
        Some(Kind::PageCache)
    } else {
        None
    }
}

/// Linux `fill_contig_page_info` for one zone: (free frames, free blocks, free blocks of `order` or more counted
/// in blocks of `order`).
fn contig(free_blocks: &[u64], order: u32) -> (u64, u64, u64) {
    let (mut pages, mut total, mut suitable) = (0u64, 0u64, 0u64);
    for (k, &n) in free_blocks.iter().enumerate() {
        let k = k as u32;
        total += n;
        pages += n << k;
        if k >= order {
            suitable = suitable.saturating_add(n << (k - order).min(40));
        }
    }
    (pages, total, suitable)
}

/// Linux `__fragmentation_index`: -1000 when a block of `order` is free, else 0 (lack of memory) .. 1000
/// (fragmentation); 0 without free memory.
pub fn fragmentation_index(free_blocks: &[u64], order: u32) -> i32 {
    let (pages, total, suitable) = contig(free_blocks, order);
    if total == 0 {
        return 0;
    }
    if suitable > 0 {
        return -1000;
    }
    let requested = 1u128 << order;
    1000 - ((1000 + pages as u128 * 1000 / requested) / total as u128).min(1000) as i32
}

/// Linux `extfrag_for_order`: the share (0..=100) of free memory not in blocks of `order` or more.
pub fn extfrag_for_order(free_blocks: &[u64], order: u32) -> u32 {
    let (pages, _, suitable) = contig(free_blocks, order);
    if pages == 0 {
        return 0;
    }
    let in_blocks = (suitable as u128) << order;
    ((pages as u128).saturating_sub(in_blocks) * 100 / pages as u128) as u32
}

impl Buddy {
    /// Installs the migrator (MM-2: crate mm's, on the boot allocator). False: one was installed already. Without
    /// one, compaction drops clean cache through the reclaimer (`DropClean`).
    pub fn set_migrator(&self, m: &'static dyn Migrate) -> bool {
        self.migrator.set(m)
    }

    /// This allocator's compaction counts.
    pub fn compact_stats(&self) -> CompactStats {
        self.compact_counts.get()
    }

    /// Runs `f` with the migrator: the installed one, else drop-if-clean through the reclaimer (None: neither).
    fn with_migrator<T>(&self, f: impl FnOnce(&dyn Migrate) -> T) -> Option<T> {
        if let Some(m) = self.migrator.get() {
            return Some(f(m));
        }
        let r = self.reclaimer.get()?;
        Some(f(&DropClean(r)))
    }

    fn zone_span(&self, zone: usize) -> Option<(u64, u64)> {
        let mut it = self.zone_regions(zone);
        let first = it.next()?;
        let end = self.zone_regions(zone).map(|r| r.end).max().unwrap_or(first.end);
        Some((first.start, end))
    }

    /// The region of `zone` holding `pfn`, or the first one above it (None: none above).
    fn region_at_or_above(&self, zone: usize, pfn: u64) -> Option<&Region> {
        self.zone_regions(zone).find(|r| r.end > pfn)
    }

    /// The region of `zone` holding `pfn - 1`, or the last one below it.
    fn region_below(&self, zone: usize, pfn: u64) -> Option<&Region> {
        self.zone_regions(zone).filter(|r| r.start < pfn).last()
    }

    // ---- skip hints ----

    fn skip(&self, r: &Region, pfn: u64) -> bool {
        let i = (pfn >> PAGEBLOCK_ORDER) - (r.start >> PAGEBLOCK_ORDER);
        self.blob()[r.skip_off + (i / 64) as usize].load(Relaxed) & (1 << (i % 64)) != 0
    }

    fn set_skip(&self, r: &Region, pfn: u64) {
        let i = (pfn >> PAGEBLOCK_ORDER) - (r.start >> PAGEBLOCK_ORDER);
        self.blob()[r.skip_off + (i / 64) as usize].fetch_or(1 << (i % 64), Relaxed);
    }

    /// Linux `__reset_isolation_suitable`: every skip hint cleared and the cached positions back at the ends.
    pub fn reset_skip_hints(&self) {
        for r in &self.regions {
            let words = super::group::pageblocks(r.start, r.end).div_ceil(64) as usize;
            for w in &self.blob()[r.skip_off..r.skip_off + words] {
                w.store(0, Relaxed);
            }
        }
        for z in &self.compact {
            z.migrate_pfn.store(0, Relaxed);
            z.free_pfn.store(0, Relaxed);
            z.flush.store(false, Relaxed);
        }
    }

    /// Linux `reset_isolation_suitable` when kswapd sleeps: only after a direct run's scanners met.
    pub fn reset_skip_if_flushed(&self) -> bool {
        if self.compact.iter().any(|z| z.flush.load(Relaxed)) {
            self.reset_skip_hints();
            return true;
        }
        false
    }

    /// Pageblocks whose skip hint is set (tests, MCP views).
    pub fn skip_hints(&self) -> u64 {
        self.regions.iter().map(|r| (0..super::group::pageblocks(r.start, r.end)).filter(|&i| self.skip(r, ((r.start >> PAGEBLOCK_ORDER) + i) << PAGEBLOCK_ORDER)).count() as u64).sum()
    }

    // ---- deferral (Linux defer_compaction, compaction_deferred, compaction_defer_reset) ----

    fn deferred(&self, zone: usize, order: u32) -> bool {
        let z = &self.compact[zone];
        if order < z.order_failed.load(Relaxed) {
            return false;
        }
        let limit = 1u32 << z.defer_shift.load(Relaxed);
        let n = z.considered.load(Relaxed).saturating_add(1);
        z.considered.store(n.min(limit), Relaxed);
        n < limit
    }

    fn defer(&self, zone: usize, order: u32) {
        let z = &self.compact[zone];
        z.considered.store(0, Relaxed);
        let s = z.defer_shift.load(Relaxed);
        if s < MAX_DEFER_SHIFT {
            z.defer_shift.store(s + 1, Relaxed);
        }
        if order < z.order_failed.load(Relaxed) {
            z.order_failed.store(order, Relaxed);
        }
    }

    fn defer_reset(&self, zone: usize, order: u32) {
        let z = &self.compact[zone];
        z.considered.store(0, Relaxed);
        z.defer_shift.store(0, Relaxed);
        if order >= z.order_failed.load(Relaxed) {
            z.order_failed.store(order + 1, Relaxed);
        }
    }

    // ---- scores ----

    fn zone_has_block(&self, zone: usize, order: u32) -> bool {
        self.zones[zone].lock().has_usable_block(order.min(MAX_ORDER))
    }

    /// Linux `fragmentation_score_zone`: external fragmentation for the pageblock order, 0..=100.
    pub fn fragmentation_score_zone(&self, zone: usize) -> u32 {
        let fb = self.zones[zone].lock().usable_blocks();
        extfrag_for_order(&fb, PAGEBLOCK_ORDER)
    }

    /// Linux `fragmentation_score_node`: the zones' scores weighted by their size (a small DMA32 counts little).
    pub fn fragmentation_score(&self) -> u32 {
        let total: u64 = (0..ZONES).map(|z| self.managed[z].load(Relaxed)).sum();
        (0..ZONES)
            .filter(|&z| self.managed[z].load(Relaxed) > 0)
            .map(|z| (self.managed[z].load(Relaxed) as u128 * self.fragmentation_score_zone(z) as u128 / (total as u128 + 1)) as u32)
            .sum()
    }

    /// Linux `should_proactive_compact_node`: proactiveness on, kswapd not reclaiming (`kswapd_is_running`: the two
    /// would compete for the same clean cache), the score above the high mark.
    pub fn should_proactive_compact(&self) -> bool {
        cp::proactiveness() > 0 && !crate::reclaim::kswapd_running() && self.fragmentation_score() > cp::score_wmark(false)
    }

    /// Linux `compaction_suitable` with the min watermark: enough free frames for the targets (`compact_gap`, more
    /// for costly orders) and, for a costly order, a fragmentation index above `extfrag_threshold` (below, the
    /// failure is lack of memory: reclaim, not compaction). Both are about a migrator that needs targets and keeps
    /// what it moves: with a dropping one (`DropClean`, where no copying one is installed) compaction is reclaim aimed at one block, which
    /// for a high order beats reclaim spread over memory (the cursor scan it replaced did the same), so it is always
    /// suitable then.
    pub fn compaction_suitable(&self, zone: usize, order: u32) -> bool {
        if self.with_migrator(|m| m.drops()).unwrap_or(true) {
            return true;
        }
        let zs = *self.zones[zone].lock();
        let w = self.zone_watermarks(zone);
        let mut mark = w.min + (2u64 << order.min(MAX_ORDER));
        if order > COSTLY_ORDER {
            mark += w.low - w.min;
        }
        if zs.usable_free() <= mark {
            return false;
        }
        if order > COSTLY_ORDER {
            let fi = fragmentation_index(&zs.usable_blocks(), order);
            if fi >= 0 && fi as u32 <= cp::extfrag_threshold() {
                return false;
            }
        }
        true
    }

    // ---- the run ----

    /// One slice of a run over `c`'s zone, at most `budget` frames read by the migrate scanner and `FREE_SCAN` by
    /// the free scanner. Call from a context that may reclaim and inside `reclaim::enter` (the callers below check).
    pub fn compact_zone(&self, c: &mut Control, budget: u64) -> Status {
        let Some((zs, ze)) = self.zone_span(c.zone) else { return Status::Skipped };
        c.fs.left = budget.min(FREE_SCAN);
        let st = self.with_migrator(|m| self.run(c, m, zs, ze, budget)).unwrap_or(Status::Skipped);
        self.release_free(c);
        if st != Status::Continue && c.unsettled > 0 {
            self.settle(c);
        }
        st
    }

    fn run(&self, c: &mut Control, m: &dyn Migrate, zs: u64, ze: u64, budget: u64) -> Status {
        let zc = &self.compact[c.zone];
        if !c.started {
            c.started = true;
            if m.drops() {
                c.drop_cap = match c.how {
                    How::Direct => DIRECT_DROP,
                    How::Daemon => DAEMON_DROP,
                    How::Proactive => PROACTIVE_DROP,
                    How::Manual => u64::MAX,
                };
            }
            c.migrate_pfn = zs;
            c.fs.at(ze);
            if let Some((m, f)) = zc.cached(c) {
                let (mp, fp) = (m.load(Relaxed), f.load(Relaxed));
                if mp > zs && mp < ze {
                    c.migrate_pfn = mp;
                }
                if fp > zs && fp <= ze {
                    c.fs.at(fp);
                }
                c.wrap = c.last && (c.migrate_pfn, c.fs.pfn) != (zs, ze);
            }
        }
        let chunk = c.chunk();
        let mut read = 0u64;
        loop {
            if self.finished(c) {
                self.park(c, c.migrate_pfn);
                return Status::Success;
            }
            let at = c.migrate_pfn.next_multiple_of(chunk);
            if at + chunk > c.fs.pfn {
                self.pb_done(c);
                if c.last {
                    zc.last_migrate_pfn.store(0, Relaxed);
                    zc.last_free_pfn.store(0, Relaxed);
                    if c.wrap {
                        // The tries before this one read the zone's ends: this one goes on there with what is
                        // left of its budget, and only a try that read from end to end has failed.
                        (c.wrap, c.wrapped) = (false, true);
                        self.release_free(c);
                        (c.migrate_pfn, c.pb_at) = (zs, u64::MAX);
                        c.fs.at(ze);
                        continue;
                    }
                    return Status::Complete;
                }
                if c.how == How::Proactive {
                    zc.pro_migrate_pfn.store(0, Relaxed);
                    zc.pro_free_pfn.store(0, Relaxed);
                }
                // Linux `reset_cached_positions`; the run asks for the hints to be cleared when kcompactd sleeps
                // (Linux asks only for direct runs; here kcompactd may be the only one compacting, as callers under
                // a spin lock only note their order, and stale hints would keep it off memory for good).
                zc.migrate_pfn.store(0, Relaxed);
                zc.free_pfn.store(0, Relaxed);
                if !c.unhinted() {
                    zc.flush.store(true, Relaxed);
                    // kcompactd clears the hints on its next pass: a direct run rings it, so a hint set by a
                    // transient blocker (a dirty or held cache page) lasts one pass, not until the next wake.
                    if c.how == How::Direct {
                        crate::compact::ring_for_flush();
                    }
                }
                return Status::Complete;
            }
            if c.s.dropped >= c.drop_cap {
                self.park(c, at);
                return Status::Partial;
            }
            if read >= budget || c.fs.left == 0 {
                self.park(c, at);
                return Status::Continue;
            }
            let Some(r) = self.region_at_or_above(c.zone, at) else {
                c.migrate_pfn = c.fs.pfn;
                continue;
            };
            if at < r.start || at + chunk > r.end {
                // A region's edge: go on at the first whole chunk of the region.
                c.migrate_pfn = if at < r.start { r.start } else { r.end };
                continue;
            }
            // A chunk in the reserve can never be a free block for the asker (`highatomic`): nothing to make there.
            if self.chunk_reserved(r, at, chunk) {
                // Past the whole pageblock: all of it is reserve.
                c.migrate_pfn = (at + chunk).max((at & !(PAGEBLOCK_FRAMES - 1)) + PAGEBLOCK_FRAMES);
                continue;
            }
            let pb = at & !(PAGEBLOCK_FRAMES - 1);
            if pb != c.pb_at {
                self.pb_done(c);
                (c.pb_at, c.pb_good, c.pb_bad) = (pb, false, false);
            }
            if !c.unhinted() && self.chunk_skipped(r, at, chunk) {
                c.migrate_pfn = (at + chunk).max(pb + PAGEBLOCK_FRAMES);
                continue;
            }
            read += chunk;
            c.s.migrate_scanned += chunk;
            let cleared = c.s.cleared;
            match self.clear_chunk(c, m, r, at, chunk) {
                Ok(()) => c.pb_good = true,
                // The free scanner's budget ran out under this chunk: no fault of the chunk's (no hint), and the
                // next slice or run takes it up again.
                Err(_) if c.fs.left == 0 => {
                    self.park(c, at);
                    return Status::Continue;
                }
                Err(blocker) => {
                    c.pb_bad = true;
                    if chunk >= PAGEBLOCK_FRAMES && !c.unhinted() && let Some(b) = blocker {
                        self.set_skip(r, b);
                    }
                }
            }
            c.migrate_pfn = at + chunk;
            // A direct run made its block: done, whoever takes it (the asker tries again, and comes back if another
            // CPU was first). Looking for the block first (`finished`) and going on when it is gone already made
            // one allocation clear chunk after chunk for the others, up to its whole budget. Linux isolates
            // COMPACT_CLUSTER_MAX pages at a time for the same reason. A dropping migrator keeps its drop cap.
            if c.how == How::Direct && c.order.is_some() && !m.drops() && c.s.cleared > cleared {
                self.park(c, c.migrate_pfn);
                return Status::Success;
            }
        }
    }

    /// The run stops here for now: the next run for an order goes on from these positions (Linux
    /// `compact_cached_migrate_pfn`, `compact_cached_free_pfn`). The free scanner goes back above the highest
    /// target it took and did not use (`release_free` gives those back).
    fn park(&self, c: &Control, migrate_pfn: u64) {
        let Some((m, f)) = self.compact[c.zone].cached(c) else { return };
        let unused = c.fs.frames[..c.fs.n].iter().copied().max().map_or(0, |p| (p | (PAGEBLOCK_FRAMES - 1)) + 1);
        m.store(migrate_pfn, Relaxed);
        f.store(c.fs.pfn.max(unused), Relaxed);
    }

    /// The migrate scanner left pageblock `c.pb_at`: below the pageblock order its skip hint is set when no chunk
    /// of it came free and one could not (Linux `update_pageblock_skip` when nothing was isolated).
    fn pb_done(&self, c: &mut Control) {
        if c.pb_at != u64::MAX && c.chunk() < PAGEBLOCK_FRAMES && c.pb_bad && !c.pb_good && !c.unhinted()
            && let Some(r) = self.region_at_or_above(c.zone, c.pb_at)
            && r.start >> PAGEBLOCK_ORDER <= c.pb_at >> PAGEBLOCK_ORDER
        {
            self.set_skip(r, c.pb_at);
        }
        c.pb_at = u64::MAX;
    }

    fn chunk_skipped(&self, r: &Region, at: u64, chunk: u64) -> bool {
        let mut pb = at & !(PAGEBLOCK_FRAMES - 1);
        while pb < at + chunk {
            if pb >> PAGEBLOCK_ORDER >= r.start >> PAGEBLOCK_ORDER && pb < r.end && self.skip(r, pb) {
                return true;
            }
            pb += PAGEBLOCK_FRAMES;
        }
        false
    }

    /// Whether the run is done (Linux `__compact_finished`): a free block of the order in the zone, the score at
    /// the low mark (proactive); never for compact_memory (until the scanners meet).
    fn finished(&self, c: &Control) -> bool {
        match (c.order, c.how) {
            (Some(o), _) => self.zone_has_block(c.zone, o),
            (None, How::Proactive) => self.fragmentation_score() <= cp::score_wmark(true),
            (None, _) => false,
        }
    }

    /// Clears [at, at + chunk) when every frame there is free, on a per-CPU list or movable by `m`; Err(the frame
    /// that keeps the chunk) otherwise (None: a race, nothing to mark).
    fn clear_chunk(&self, c: &mut Control, m: &dyn Migrate, r: &Region, at: u64, chunk: u64) -> Result<(), Option<u64>> {
        let end = at + chunk;
        let (mut p, mut free, mut movable, mut cached) = (at, 0u64, 0u64, false);
        while p < end {
            let mt = self.get_meta(r, p);
            match mt.state {
                State::Free => {
                    free += 1 << mt.order;
                    p += 1 << mt.order;
                }
                State::Cached => {
                    free += 1;
                    cached = true;
                    p += 1;
                }
                State::Alloc if mt.order == 0 && kind_of(mt.kind).is_some() => {
                    let kind = kind_of(mt.kind).unwrap_or(Kind::PageCache);
                    if self.pb_type(r, p) != MigrateType::Movable {
                        return Err(Some(p));
                    }
                    if kind == Kind::User && m.drops() {
                        // Anonymous memory cannot be dropped: only a migrator that copies moves it (MM-2).
                        c.s.skipped_anon += 1;
                        return Err(Some(p));
                    }
                    if kind == Kind::PageCache && self.owner(p * PAGE) == 0 {
                        return Err(Some(p));
                    }
                    movable += 1;
                    p += 1;
                }
                State::Alloc => return Err(Some(p)),
                State::None => return Err(None),
            }
        }
        if movable == 0 {
            // All free and no block: per-CPU lists hold part of it.
            if cached {
                self.try_drain_all();
            }
            return Ok(());
        }
        if m.drops() && c.how.whole() && free == 0 {
            return Err(None);
        }
        let mut p = at;
        // The free scanner takes no more targets than this chunk still has frames to move (Linux
        // `isolate_freepages` stops at `nr_migratepages`): what a run takes and does not use, the next run reads
        // again.
        let mut todo = movable;
        while p < end {
            let mt = self.get_meta(r, p);
            match mt.state {
                State::Free => p += 1 << mt.order,
                State::Cached | State::None => p += 1,
                State::Alloc if mt.order == 0 => {
                    let Some(kind) = kind_of(mt.kind) else { return Err(Some(p)) };
                    let owner = self.owner(p * PAGE);
                    let from = if c.last { Targets::Any } else if c.how.whole() { Targets::Unhinted } else { Targets::Hinted };
                    let want = todo.clamp(1, CLUSTER as u64) as usize;
                    let mut dest = || self.compaction_alloc(c.zone, &mut c.fs, want, end, &mut c.s, kind, from).map(|t| crate::compact::Target::new(self, t, 0, kind));
                    todo = todo.saturating_sub(1);
                    match m.migrate(p * PAGE, kind, owner, &mut dest) {
                        Moved::Moved => c.s.moved += 1,
                        Moved::Dropped => c.s.dropped += 1,
                        Moved::Stays => return Err(Some(p)),
                        Moved::Unsupported => {
                            if kind == Kind::User {
                                c.s.skipped_anon += 1;
                            }
                            return Err(Some(p));
                        }
                    }
                    c.s.isolated += 1;
                    c.unsettled += 1;
                    p += 1;
                }
                State::Alloc => return Err(Some(p)),
            }
        }
        c.s.cleared += 1;
        // A target-order run wants the block now: the frames come back after their grace period.
        if c.order.is_some() {
            self.settle(c);
        }
        Ok(())
    }

    /// The frames this run dropped or moved come back (their grace period, bounded by the run's budget) and the
    /// per-CPU lists go back into the buddy (try-locks), so the block merges.
    fn settle(&self, c: &mut Control) {
        crate::grace::expedite_with(&mut c.grace);
        self.try_drain_all();
        c.unsettled = 0;
    }

    /// Linux `compaction_alloc`: a free target frame of `kind`, from the free scanner's isolated frames (refilled
    /// from the top of the zone down, never at or below the migrate scanner).
    /// `want`: the targets the caller can still use.
    #[allow(clippy::too_many_arguments)]
    fn compaction_alloc(&self, zone: usize, fs: &mut FreeScan, want: usize, low: u64, s: &mut CompactStats, kind: Kind, from: Targets) -> Option<u64> {
        if fs.n == 0 {
            self.isolate_free(zone, fs, want, low, s, from);
        }
        if fs.n == 0 {
            return None;
        }
        fs.n -= 1;
        let pfn = fs.frames[fs.n];
        let r = self.region_of(pfn)?;
        let _moving = ksync::PreemptGuard::new();
        self.set_meta(r, pfn, Meta::new(State::Alloc, 0, kind as usize));
        self.used[Kind::Other as usize].fetch_sub(1, Relaxed);
        self.used[kind as usize].fetch_add(1, Relaxed);
        Some(pfn * PAGE)
    }

    /// Linux `isolate_freepages` / `isolate_freepages_block`: free blocks below the pageblock order out of Movable
    /// pageblocks from `fs.pfn` down to the pageblock above `low` (the end of the chunk being cleared; a pageblock
    /// with a free pageblock-sized block is no target: Linux `suitable_migration_target`), split into frames until
    /// `want` are taken or `fs.left` frames were read. Each block is taken under the zone lock after the lock-free
    /// read saw it. `from` says which pageblocks and what the skip hints count for (`Targets`).
    fn isolate_free(&self, zone: usize, fs: &mut FreeScan, want: usize, low: u64, s: &mut CompactStats, from: Targets) {
        let floor = low.next_multiple_of(PAGEBLOCK_FRAMES);
        let want = want.clamp(1, CLUSTER);
        while fs.n < want && fs.left > 0 {
            // A pageblock passed without a look inside costs one frame of the budget: the walk is bounded too.
            fs.left -= 1;
            let Some(r) = self.region_below(zone, fs.pfn) else { return };
            let top = fs.pfn.min(r.end);
            let pb = (top - 1) & !(PAGEBLOCK_FRAMES - 1);
            if pb < floor || top <= r.start {
                return;
            }
            let lo = pb.max(r.start);
            let mt = self.pb_type(r, lo);
            let target = match from {
                Targets::Any => mt != MigrateType::HighAtomic,
                Targets::Unhinted => mt == MigrateType::Movable,
                Targets::Hinted => mt == MigrateType::Movable && !self.skip(r, lo),
            };
            // A pageblock that is free whole is no target by rule, not for want of free frames: it gets no hint
            // (Linux skips it before `isolation_suitable`), or the hint would keep the scanner off it once it is in
            // use and has holes.
            if target && !self.in_free_pageblock(r, lo) {
                // A step that stopped inside this pageblock left the frames below `resume` read.
                let resumed = fs.resume > lo && fs.resume < top;
                let mut p = if resumed { fs.resume } else { lo };
                let mut took = resumed;
                while p < top && fs.n < want && fs.left > 0 {
                    let m = self.get_meta(r, p);
                    s.free_scanned += 1;
                    fs.left -= 1;
                    match m.state {
                        State::Free if m.order >= PAGEBLOCK_ORDER => break,
                        State::Free => {
                            let n = self.take_free_block(r, p, m.order, &mut fs.frames[fs.n..want]);
                            took |= n > 0;
                            fs.n += n;
                            s.isolated += n as u64;
                            // Only the part taken is behind the scanner: the rest of the block is still free.
                            p += if n > 0 { n as u64 } else { 1 << m.order };
                        }
                        State::Alloc => p += 1 << m.order,
                        _ => p += 1,
                    }
                }
                if (fs.n >= want || fs.left == 0) && p < top {
                    // Enough targets, or the budget ran out, inside this pageblock: the next step goes on here.
                    (fs.pfn, fs.resume) = (top, p);
                    return;
                }
                if !took && from == Targets::Hinted {
                    self.set_skip(r, lo);
                }
            }
            fs.at(lo);
        }
    }

    /// Whether the pageblock at `pb` lies in a free block of the pageblock order or more (a hint: no lock).
    fn in_free_pageblock(&self, r: &Region, pb: u64) -> bool {
        (PAGEBLOCK_ORDER..=MAX_ORDER).any(|o| {
            let head = pb & !((1u64 << o) - 1);
            r.holds(head, o) && self.get_meta(r, head) == Meta::new(State::Free, o, 0)
        })
    }

    /// Takes the free block of `order` at `pfn` (if it still is one) or its first part that fits `room`; the rest
    /// stays free. Returns the frames written into `room`.
    fn take_free_block(&self, r: &Region, pfn: u64, order: u32, room: &mut [u64]) -> usize {
        if room.is_empty() {
            return 0;
        }
        let want = (usize::BITS - 1 - room.len().leading_zeros()).min(order);
        let _moving = ksync::PreemptGuard::new();
        let mut zs = self.zones[r.zone].lock();
        if self.get_meta(r, pfn) != Meta::new(State::Free, order, 0) {
            return 0;
        }
        self.remove_free(r, &mut zs, pfn, order);
        self.expand(r, &mut zs, pfn, want, order);
        let n = 1usize << want;
        for (i, slot) in room.iter_mut().take(n).enumerate() {
            let f = pfn + i as u64;
            self.set_meta(r, f, Meta::new(State::Alloc, 0, Kind::Other as usize));
            *slot = f;
        }
        self.used[Kind::Other as usize].fetch_add(n as u64, Relaxed);
        n
    }

    /// Linux `release_free_list`: isolated targets not used go back.
    fn release_free(&self, c: &mut Control) {
        for &pfn in &c.fs.frames[..c.fs.n] {
            self.free(pfn * PAGE, 0, Kind::Other);
        }
        c.fs.n = 0;
    }

    /// Adds a finished run's counts to this allocator's and (boot allocator) the global ones.
    pub(super) fn count_run(&self, c: &Control, outcome: Option<bool>) {
        let s = &c.s;
        let k = &self.compact_counts;
        let daemon = c.how.daemon();
        let (dm, df) = if daemon { (s.migrate_scanned, s.free_scanned) } else { (0, 0) };
        for (i, v) in [(3, s.migrate_scanned), (4, s.free_scanned), (5, s.isolated), (7, dm), (8, df), (9, s.skipped_anon), (10, s.dropped), (11, s.moved), (12, s.cleared)] {
            k.add(i, v);
        }
        match outcome {
            Some(true) => k.add(2, 1),
            Some(false) => k.add(1, 1),
            None => {}
        }
        if self.global_reports {
            cp::MIGRATE_SCANNED.add(s.migrate_scanned);
            cp::FREE_SCANNED.add(s.free_scanned);
            cp::ISOLATED.add(s.isolated);
            cp::DAEMON_MIGRATE_SCANNED.add(dm);
            cp::DAEMON_FREE_SCANNED.add(df);
            cp::SKIPPED_ANON.add(s.skipped_anon);
            cp::DROPPED.add(s.dropped);
            match outcome {
                Some(true) => cp::SUCCESS.inc(),
                Some(false) => cp::FAIL.inc(),
                None => {}
            }
        }
    }

    /// The zones a block is wanted from: Normal first, then DMA32 (the heap's and the faults' order).
    fn zones_for(&self) -> [usize; 2] {
        [zone_index(Zone::Normal), zone_index(Zone::Dma32)]
    }

    /// Linux `try_to_compact_pages` + `__alloc_pages_direct_compact` for an allocation of `order` that may sleep:
    /// one bounded run per zone that is neither deferred nor unsuitable, until a free block of the order exists.
    /// Counts a stall and its success or failure. Only where the context may reclaim; not inside reclaim already
    /// (`reclaim_for_heap` holds that guard itself and calls `compact_direct_inside`).
    pub fn compact_direct(&self, order: u32) -> bool {
        self.compact_direct_with(order, &mut crate::grace::WaitBudget::default())
    }

    /// `compact_direct` within the allocation's grace wait budget `grace` (B4, PF-5: one allocation, one budget,
    /// whatever steps it takes: its reclaim rounds and its compaction runs share it).
    pub fn compact_direct_with(&self, order: u32, grace: &mut crate::grace::WaitBudget) -> bool {
        if order == 0 || order > MAX_ORDER || !crate::reclaim::context_allows() {
            return false;
        }
        let Some(_inside) = crate::reclaim::enter() else { return false };
        self.compact_direct_inside(order, grace)
    }

    pub(super) fn compact_direct_inside(&self, order: u32, grace: &mut crate::grace::WaitBudget) -> bool {
        if self.migrator.get().is_none() && self.reclaimer.get().is_none() {
            return false;
        }
        self.compact_counts.add(0, 1);
        if self.global_reports {
            cp::STALL.inc();
        }
        let mut made = false;
        let mut ran = None;
        for zone in self.zones_for() {
            if self.managed[zone].load(Relaxed) == 0 {
                continue;
            }
            if self.zone_has_block(zone, order) {
                made = true;
                break;
            }
            if self.deferred(zone, order) || !self.compaction_suitable(zone, order) {
                continue;
            }
            let mut c = Control::new(Some(order), How::Direct, zone);
            c.grace = *grace;
            let st = self.compact_zone(&mut c, DIRECT_SCAN);
            *grace = c.grace;
            made = st == Status::Success || self.zone_has_block(zone, order);
            let mut last = Last::Failed;
            if !made && st == Status::Complete {
                last = self.compact_last(order, How::Direct, zone, grace, None);
                made = last == Last::Made;
            }
            if made {
                self.defer_reset(zone, order);
            } else if st == Status::Complete && last == Last::Failed {
                // Linux defers after a sync run whose scanners met (COMPACT_COMPLETE); a run stopped by the drop
                // cap (Partial) or a scan budget (Continue) leaves the shift: the next asker goes on where it
                // stopped, and the askers together read the zone once before the scanners meet.
                self.defer(zone, order);
            }
            ran = Some(c);
            if made {
                break;
            }
        }
        match ran {
            Some(c) => self.count_run(&c, Some(made)),
            None => self.count_run(&Control::new(Some(order), How::Direct, 0), Some(made)),
        }
        made
    }

    /// W35 MM-2: the last try for a block of `order` in `zone`, after a run whose scanners met without one: one
    /// more run from the zone's ends that reads no skip hints and takes its targets out of any pageblock but the
    /// reserve's (Linux 7.x `compact_zone_order` at `MIN_COMPACT_PRIORITY`: `whole_zone`, `ignore_skip_hint`,
    /// `ignore_block_suitable`). A migrator that copies needs free frames as targets, and the free scanner takes
    /// them from Movable pageblocks only; where the free frames lie between kernel frames (a heap that grew and
    /// shrank), a zone half free had no target at all, every run ended with its scanners met, and after six of
    /// them compaction was deferred for the next 64 askers. Not for a migrator that drops: it needs no target.
    /// `between`: a background run in slices (None: one direct run). The tries go round the zone: each goes on
    /// where the one before stopped (re-reading the ends each time was most of the free scanner's work under
    /// load). A try stopped by its budget has not failed, and nothing is deferred for it.
    fn compact_last(&self, order: u32, how: How, zone: usize, grace: &mut crate::grace::WaitBudget, between: Option<&mut dyn FnMut()>) -> Last {
        if self.with_migrator(|m| m.drops()) != Some(false) {
            return Last::Failed;
        }
        let mut c = Control::new(Some(order), how, zone);
        c.last = true;
        c.grace = *grace;
        let st = match between {
            None => self.compact_zone(&mut c, DIRECT_SCAN),
            Some(between) => {
                // kcompactd's try that started at the ends only after its scanners met gets a whole pass from
                // there: on a zone a pass covers it then reads all of it in this wake, as a try from the ends does.
                match self.pass(&mut c, between) {
                    Status::Continue if c.wrapped => self.pass(&mut c, between),
                    st => st,
                }
            }
        };
        *grace = c.grace;
        self.count_run(&c, None);
        let made = st == Status::Success || self.zone_has_block(zone, order);
        if self.global_reports {
            cp::LAST_TRIES.inc();
            if made {
                cp::LAST_MADE.inc();
            }
        }
        match st {
            _ if made => Last::Made,
            Status::Continue => Last::Stopped,
            _ => Last::Failed,
        }
    }

    /// One pass of a background run for an order: at most `PASS_SLICES` slices with `between` after each
    /// (kcompactd yields there). `Continue`: the pass's budget is spent, the cached positions say where the next
    /// one goes on.
    fn pass(&self, c: &mut Control, between: &mut dyn FnMut()) -> Status {
        for _ in 0..PASS_SLICES {
            let st = self.compact_zone(c, SLICE);
            if st != Status::Continue {
                return st;
            }
            between();
        }
        Status::Continue
    }

    /// Linux `kcompactd_do_work` for `order`: each zone that is neither deferred nor unsuitable gets one pass
    /// (`pass`), until a block exists. True: a free block of the order exists now.
    pub fn kcompactd_work(&self, order: u32, between: &mut dyn FnMut()) -> bool {
        let order = order.clamp(1, MAX_ORDER);
        self.compact_counts.add(6, 1);
        if self.global_reports {
            cp::DAEMON_WAKE.inc();
        }
        let Some(_inside) = crate::reclaim::enter() else { return false };
        let mut made = false;
        for zone in self.zones_for() {
            if self.managed[zone].load(Relaxed) == 0 {
                continue;
            }
            if self.zone_has_block(zone, order) {
                made = true;
                break;
            }
            if self.deferred(zone, order) || !self.compaction_suitable(zone, order) {
                continue;
            }
            let mut c = Control::new(Some(order), How::Daemon, zone);
            let st = self.pass(&mut c, between);
            self.count_run(&c, None);
            if st == Status::Success || self.zone_has_block(zone, order) {
                self.defer_reset(zone, order);
                made = true;
                break;
            }
            // Linux: frames stranded on per-CPU lists may merge now; then defer as a sync run whose scanners met
            // does (a run stopped by the drop cap was making progress: no deferral).
            self.drain_all();
            made = self.zone_has_block(zone, order);
            if st == Status::Complete {
                let last = if made { Last::Failed } else { self.compact_last(order, How::Daemon, zone, &mut crate::grace::WaitBudget::default(), Some(&mut *between)) };
                match last {
                    Last::Made => {
                        self.defer_reset(zone, order);
                        made = true;
                    }
                    Last::Failed => self.defer(zone, order),
                    Last::Stopped => {}
                }
            }
            if made {
                break;
            }
        }
        made
    }

    /// Linux `compact_node`: every zone compacted whole (`Manual`: /proc/sys/vm/compact_memory; `Proactive`:
    /// kcompactd for the score), in slices with `between` after each. Returns the run's counts.
    pub fn compact_all(&self, how: How, between: &mut dyn FnMut()) -> CompactStats {
        let how = if how.whole() { how } else { How::Manual };
        let mut total = CompactStats::default();
        let Some(_inside) = crate::reclaim::enter() else { return total };
        if how == How::Proactive && self.global_reports {
            cp::PROACTIVE_RUNS.inc();
        }
        for zone in 0..ZONES {
            if self.managed[zone].load(Relaxed) == 0 {
                continue;
            }
            let mut c = Control::new(None, how, zone);
            // A proactive run is one pass: on 24 GiB a run from end to end read 12 million frame states, and with
            // little free memory the score asks for one every period (Mini-PC 12.11.2026: twelve a minute).
            let st = if how == How::Proactive {
                self.pass(&mut c, between)
            } else {
                loop {
                    let st = self.compact_zone(&mut c, SLICE);
                    if st != Status::Continue {
                        break st;
                    }
                    between();
                }
            };
            self.count_run(&c, None);
            let s = c.stats();
            total.migrate_scanned += s.migrate_scanned;
            total.free_scanned += s.free_scanned;
            total.isolated += s.isolated;
            total.dropped += s.dropped;
            total.moved += s.moved;
            total.cleared += s.cleared;
            total.skipped_anon += s.skipped_anon;
            if how == How::Proactive && (st == Status::Success || st == Status::Partial) {
                break;
            }
        }
        total
    }
}
