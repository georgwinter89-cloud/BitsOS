//! W35 MM-3: the frame allocator's side of memory compaction (Linux 7.x mm/compaction.c): the `Migrate` interface
//! the scanners call for each frame they isolate, the sysctl knobs, the counters of /proc/vmstat and kcompactd's
//! doorbell. The scanners themselves are `buddy/compact.rs`; the kcompactd thread is crate mm's `compact`.
//!
//! Page migration (MM-2) comes after this item (Georg 6.11.: lane BL before MM-2), so until a migrator is installed
//! (`Buddy::set_migrator`) the only way to move a frame is `DropClean`: a clean, unmapped page-cache page is taken
//! back through the page cache's `Reclaim::reclaim_frame` (it is read again when needed) and an anonymous frame
//! stays, counted (`mm.compact.skipped_anon`). MM-2 puts the real copy behind the same trait; the scanners do not
//! change.

use crate::reclaim::{context_allows, Reclaim};
use crate::Kind;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

/// What became of one frame the migrate scanner handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    /// Copied into a frame from `dest`; the old frame is the migrator's to free (it goes back after a grace period
    /// where lockless readers may still hold it).
    Moved,
    /// Given up by its owner (clean cache); its frame comes back after a grace period.
    Dropped,
    /// Still in use (dirty, mapped, held, locked by someone else): the frame stays where it is.
    Stays,
    /// A kind this migrator cannot move (anonymous memory before MM-2): stays, counted by the scanner.
    Unsupported,
}

/// Moves one order-0 frame of a movable kind (`Kind::PageCache` or `Kind::User`) out of the way (Linux
/// `migrate_pages` with `compaction_alloc` as the target source). Called without any allocator lock held, from a
/// context that may reclaim (`reclaim::context_allows`), inside `reclaim::enter`. Must not wait for a lock
/// (try-locks only, as `Reclaim`).
pub trait Migrate: Sync {
    /// `phys`: the frame; `owner`: its owner word (`Buddy::owner`); `dest`: a free frame of `kind` for the copy,
    /// isolated by the free scanner (None: none left).
    fn migrate(&self, phys: u64, kind: Kind, owner: u64, dest: &mut dyn FnMut() -> Option<u64>) -> Moved;
    /// Whether moving a frame loses its contents (`DropClean`): the scanners then clear only blocks that end up
    /// free and cap what one background run may drop.
    fn drops(&self) -> bool {
        false
    }
}

/// The migrator until MM-2: drop-if-clean through the page cache's reclaimer (vfs `reclaim_frame`: clean, not
/// dirty, nobody else holds the page; mapped and held pages stay).
pub struct DropClean<'a>(pub &'a dyn Reclaim);

impl Migrate for DropClean<'_> {
    fn migrate(&self, phys: u64, kind: Kind, owner: u64, _dest: &mut dyn FnMut() -> Option<u64>) -> Moved {
        match kind {
            Kind::PageCache if owner != 0 => {
                if self.0.reclaim_frame(phys, owner) {
                    Moved::Dropped
                } else {
                    Moved::Stays
                }
            }
            Kind::PageCache => Moved::Stays,
            _ => Moved::Unsupported,
        }
    }
    fn drops(&self) -> bool {
        true
    }
}

// ---- sysctl knobs (Linux defaults: mm/compaction.c :1936-1937, sysctl_compact_unevictable_allowed) ----

/// `vm.compaction_proactiveness` (0..=100, Linux default 20, Georg 6.11. decision 2): 0 turns proactive work off.
static PROACTIVENESS: AtomicU32 = AtomicU32::new(20);
/// `vm.extfrag_threshold` (0..=1000, default 500): below it a costly order's failure is lack of memory, not
/// fragmentation, and compaction is not tried (`compaction_suitable`).
static EXTFRAG_THRESHOLD: AtomicU32 = AtomicU32::new(500);
/// `vm.compact_unevictable_allowed` (0..=1, default 1). Kept as written: no migrator moves mlocked pages yet
/// (cache pages mapped by anyone stay, anonymous frames are not moved before MM-2).
static UNEVICTABLE_ALLOWED: AtomicU32 = AtomicU32::new(1);

pub fn proactiveness() -> u32 {
    PROACTIVENESS.load(Relaxed)
}

/// False: outside 0..=100 (EINVAL).
pub fn set_proactiveness(v: u64) -> bool {
    set(&PROACTIVENESS, v, 100, "compaction_proactiveness")
}

pub fn extfrag_threshold() -> u32 {
    EXTFRAG_THRESHOLD.load(Relaxed)
}

pub fn set_extfrag_threshold(v: u64) -> bool {
    set(&EXTFRAG_THRESHOLD, v, 1000, "extfrag_threshold")
}

pub fn unevictable_allowed() -> u32 {
    UNEVICTABLE_ALLOWED.load(Relaxed)
}

pub fn set_unevictable_allowed(v: u64) -> bool {
    set(&UNEVICTABLE_ALLOWED, v, 1, "compact_unevictable_allowed")
}

fn set(at: &AtomicU32, v: u64, max: u64, name: &'static str) -> bool {
    if v > max {
        return false;
    }
    let old = at.swap(v as u32, Relaxed);
    if old as u64 != v {
        klog::event!(Info, "mm", "compact.knob", knob = name, value = v, was = old);
    }
    // Proactive work switched on again: kcompactd's long sleep ends now.
    if name == "compaction_proactiveness" && old == 0 && v > 0 {
        ring();
    }
    true
}

/// Linux `fragmentation_score_wmark`: proactive compaction starts above the high mark and runs down to the low one.
pub fn score_wmark(low: bool) -> u32 {
    let wmark_low = 100u32.saturating_sub(proactiveness());
    let leeway = 10.min(wmark_low / 2);
    if low { wmark_low } else { (wmark_low + leeway).min(100) }
}

// ---- counters (/proc/vmstat compact_*, Linux 7.x include/linux/vm_event_item.h) ----
// Bumped once per compaction run with the run's totals (never per frame on the allocation path).

klog::counter!(pub MIGRATE_SCANNED, "mm.compact.migrate_scanned");
klog::counter!(pub FREE_SCANNED, "mm.compact.free_scanned");
klog::counter!(pub ISOLATED, "mm.compact.isolated");
klog::counter!(pub STALL, "mm.compact.stall");
klog::counter!(pub FAIL, "mm.compact.fail");
klog::counter!(pub SUCCESS, "mm.compact.success");
klog::counter!(pub DAEMON_WAKE, "mm.compact.daemon_wake");
klog::counter!(pub DAEMON_MIGRATE_SCANNED, "mm.compact.daemon_migrate_scanned");
klog::counter!(pub DAEMON_FREE_SCANNED, "mm.compact.daemon_free_scanned");
// Anonymous frames the migrate scanner found in a block it would have cleared (none can move before MM-2).
klog::counter!(pub SKIPPED_ANON, "mm.compact.skipped_anon");
// Page-cache frames dropped instead of moved (`DropClean`).
klog::counter!(pub DROPPED, "mm.compact.dropped");
// Proactive runs (score above the high mark).
klog::counter!(pub PROACTIVE_RUNS, "mm.compact.proactive_runs");

// ---- kcompactd's doorbell (Linux `wakeup_kcompactd`, `kcompactd_work_requested`) ----

static WAIT: ksync::WaitQueue = ksync::WaitQueue::new();
/// Orders asked for (bit k: order k; Linux keeps only the highest, `kcompactd_max_order`: a block of it serves
/// every lower one, but a missing small one must not wait behind a big one that cannot be made).
static ORDERS: AtomicU32 = AtomicU32::new(0);
static KICK: AtomicBool = AtomicBool::new(false);
/// A wake left for later where the scheduler may not be called (as kswapd's, `reclaim::wake_deferred`).
static DEFERRED: AtomicBool = AtomicBool::new(false);
static WOKEN: AtomicU32 = AtomicU32::new(0);

/// Asks kcompactd for a free block of `order` (kswapd after balancing the count, a THP fault that may not stall,
/// a failed high-order allocation). Lock-free; from any context (the wake itself is deferred under a spin lock).
pub fn wake_kcompactd(order: u32) {
    if order == 0 || order > crate::MAX_ORDER {
        return;
    }
    ORDERS.fetch_or(1 << order, Relaxed);
    WOKEN.fetch_add(1, Relaxed);
    ring();
}

fn ring() {
    KICK.store(true, Relaxed);
    if context_allows() {
        WAIT.wake_one();
    } else {
        DEFERRED.store(true, Relaxed);
    }
}

/// A direct run's scanners met and asked for the skip hints to be cleared: kcompactd clears them on its next
/// pass (lock-free; deferred under a spin lock as every wake).
pub(crate) fn ring_for_flush() {
    ring();
}

/// Rings a wake `wake_kcompactd` had to leave for later (called by `reclaim::wake_deferred`).
pub(crate) fn wake_deferred() {
    if DEFERRED.load(Relaxed) && context_allows() && DEFERRED.swap(false, Relaxed) {
        WAIT.wake_one();
    }
}

/// kcompactd's wait: until a wake or `timeout_ns`; true if woken.
pub fn kcompactd_sleep(timeout_ns: u64) -> bool {
    let woke = WAIT.wait_until_timeout(|| KICK.load(Relaxed), timeout_ns);
    KICK.store(false, Relaxed);
    woke
}

/// The orders asked for since the last take (bit k: order k), cleared.
pub fn take_orders() -> u32 {
    ORDERS.swap(0, Relaxed)
}

/// The orders asked for and not taken yet.
pub fn pending_orders() -> u32 {
    ORDERS.load(Relaxed)
}

/// Wakes asked for since boot (`wake_kcompactd`).
pub fn wakes_asked() -> u64 {
    u64::from(WOKEN.load(Relaxed))
}
