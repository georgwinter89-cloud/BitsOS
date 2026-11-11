//! MM-9: the frame allocator's side of page-cache reclaim (Linux mm/vmscan.c and mm/page_alloc.c, small).
//!
//! The page cache lives above mm-core (crate vfs), so the allocator knows it only as a registered `Reclaim`.
//! When an allocation of a reclaim-allowed kind found no frame (both attempts and the per-CPU drain failed:
//! the slow path, the fast path is unchanged), the buddy asks it for clean, unmapped cache pages and tries
//! again (`__alloc_pages_direct_reclaim`, linux-6.6/mm/page_alloc.c:3639): a light round first, then a thorough
//! one, no more (bounded, B4: the reclaimer never waits for a lock and never allocates). The kswapd-like
//! thread (crate mm, `mm.kswapd`) keeps free memory between the watermarks computed here in the background.

use crate::{Kind, PAGE};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// The page cache's side (crate vfs implements it, crate mm installs it).
pub trait Reclaim: Sync {
    /// Takes back up to `pages` clean, unmapped page-cache pages. Must not wait for a lock (bounded try-locks
    /// only) or allocate frames: the allocator may be called with any lock of its caller held. `thorough`: look
    /// at every cached page (the rounds before the allocation would fail).
    fn reclaim(&self, pages: u64, thorough: bool) -> Reclaimed;
    /// FS-STALL: takes back the page that frame `phys` holds, named by the frame's owner word (`Buddy::owner`),
    /// when it is clean and nobody holds it; the same rules. True: it went (its frame comes back after a grace
    /// period). Default: never.
    fn reclaim_frame(&self, phys: u64, owner: u64) -> bool {
        let _ = (phys, owner);
        false
    }
}

/// What one reclaim round did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    pub freed: u64,
    /// A cache was passed over because someone held its lock, or the round's scan budget ran out: nothing freed
    /// does not mean nothing reclaimable.
    pub busy: bool,
}

/// Direct reclaim rounds at most: one light, then thorough ones while caches were busy or a budget ran out.
pub const ROUNDS: u32 = 4;

/// `SWAP_CLUSTER_MAX`: the pages one direct reclaim round asks for at least.
pub const CLUSTER: u64 = 32;

/// Whether an allocation of `kind` may reclaim directly. Only program memory and the page cache itself: they are
/// allocated in thread context (faults, file reads and writes), never under the heap's lock (heap refills,
/// `Kind::Heap`), a page-table lock (`PageTable`), a driver's DMA setup (`Dma`, `Driver`) or the scheduler's
/// spawn path (`Stack`); those rely on the background thread keeping the watermark gap free (as Linux's
/// atomic allocations rely on the min reserve).
pub fn direct_allowed(kind: Kind) -> bool {
    matches!(kind, Kind::User | Kind::PageCache)
}

/// Pages a direct reclaim round asks for an allocation of 2^`order` frames (a block needs its neighbours free
/// too, so twice the block, at least a cluster).
pub fn direct_target(order: u32) -> u64 {
    CLUSTER.max(2u64 << order.min(20))
}

/// Free-frame watermarks of the whole machine in frames (`__setup_per_zone_wmarks`, one zone's worth).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Watermarks {
    pub min: u64,
    pub low: u64,
    pub high: u64,
}

/// Linux's watermarks for `total` managed frames: min = `min_free_kbytes` = sqrt(16 x RAM in KiB), clamped to
/// 128 KiB..256 MiB (linux-6.6/mm/page_alloc.c:5748); the kswapd gaps low - min and high - low are each the
/// larger of min/4 and 0.1 % of RAM (`watermark_scale_factor` 10, :5682). 32 GB: min 22.6 MiB, low 55 MiB,
/// high 87 MiB; 512 MiB: min 2.8 MiB, low 3.5 MiB, high 4.2 MiB.
pub fn watermarks(total: u64) -> Watermarks {
    let kib = total.saturating_mul(PAGE / 1024);
    let min_kib = isqrt(kib.saturating_mul(16)).clamp(128, 262_144);
    let min = (min_kib / (PAGE / 1024)).min(total);
    let gap = (min / 4).max(total.saturating_mul(10) / 10_000);
    Watermarks { min, low: min.saturating_add(gap), high: min.saturating_add(gap.saturating_mul(2)) }
}

/// `si_mem_available` (linux-6.6/mm/show_mem.c:32) in bytes: free memory minus the reserve (the high
/// watermark, `calculate_totalreserve_pages`), plus the file pages of the page cache minus what has to stay (half
/// of it, at most the low watermark). `file`: page-cache bytes that are not shared memory (tmpfs never goes:
/// no swap). V2 has no reclaimable slab to add.
pub fn mem_available(total_bytes: u64, free_bytes: u64, file_bytes: u64) -> u64 {
    let w = watermarks(total_bytes / PAGE);
    let (free, file) = (free_bytes / PAGE, file_bytes / PAGE);
    let keep = (file / 2).min(w.low);
    (free + file).saturating_sub(w.high.min(total_bytes / PAGE) + keep) * PAGE
}

fn isqrt(n: u64) -> u64 {
    n.isqrt()
}

/// Direct reclaim counts of one allocator (`Buddy::direct_reclaim`).
#[derive(Default)]
pub struct DirectCounters {
    /// Allocations that went into direct reclaim.
    pub runs: AtomicU64,
    /// Pages the reclaimer freed for them.
    pub pages: AtomicU64,
    /// Of `runs`, those that got their frames afterwards.
    pub rescued: AtomicU64,
    /// Of `runs`, those that failed all the same (ENOMEM while nothing reclaimable was left, or only busy pages).
    pub failed: AtomicU64,
    /// Program or cache allocations that took frames of the reserve (nothing was reclaimable).
    pub dips: AtomicU64,
}

/// A copy of `DirectCounters`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectStats {
    pub runs: u64,
    pub pages: u64,
    pub rescued: u64,
    pub failed: u64,
    pub dips: u64,
}

impl DirectCounters {
    pub const fn new() -> DirectCounters {
        DirectCounters { runs: AtomicU64::new(0), pages: AtomicU64::new(0), rescued: AtomicU64::new(0), failed: AtomicU64::new(0), dips: AtomicU64::new(0) }
    }
    pub fn get(&self) -> DirectStats {
        DirectStats {
            runs: self.runs.load(Relaxed),
            pages: self.pages.load(Relaxed),
            rescued: self.rescued.load(Relaxed),
            failed: self.failed.load(Relaxed),
            dips: self.dips.load(Relaxed),
        }
    }
}

klog::counter!(DIRECT_RUNS, "mm.reclaim.direct_runs");
klog::counter!(DIRECT_PAGES, "mm.reclaim.direct_pages");
klog::counter!(DIRECT_RESCUED, "mm.reclaim.direct_rescued");
klog::counter!(DIRECT_FAILED, "mm.reclaim.direct_failed");
// FS-STALL: the kernel heap's own reclaim (`Buddy::reclaim_for_heap`): calls, pages freed, allocations it saved.
klog::counter!(pub HEAP_RUNS, "mm.reclaim.heap_runs");
klog::counter!(pub HEAP_PAGES, "mm.reclaim.heap_pages");
klog::counter!(pub HEAP_RESCUED, "mm.reclaim.heap_rescued");
// W35 MM-3: blocks the heap's direct compaction made (the name is from the cache-dropping search it replaced).
klog::counter!(pub HEAP_CLEARED, "mm.reclaim.heap_cleared_blocks");
// Heap allocations whose run was missing (the order noted for kswapd), and the blocks kcompactd made for the
// orders kswapd handed it (W35 MM-3; before, kswapd cleared them of cache itself).
klog::counter!(pub HEAP_ORDER_WANTED, "mm.reclaim.heap_order_wanted");
klog::counter!(pub KSWAPD_CLEARED, "mm.reclaim.kswapd_cleared_blocks");

/// W35 MM-1: orders of frame allocations that failed (the boot allocator's slow path, `Buddy::want_kswapd`), next
/// to the heap's and the faults' (`KernelHeap::order_want`, crate mm's `want_order`): kswapd takes all three.
pub static FRAME_WANT: crate::OrderWant = crate::OrderWant::new();

/// kswapd's doorbell (Linux `wakeup_kswapd` -> `wake_up_interruptible(&pgdat->kswapd_wait)`): `wake_kswapd` rings it,
/// the thread waits on it between looks (`kswapd_sleep`; its period stays as the backstop).
static KSWAPD_WAIT: ksync::WaitQueue = ksync::WaitQueue::new();
/// A wake is due (the condition the thread waits for).
static KSWAPD_KICK: AtomicBool = AtomicBool::new(false);
/// A wake asked for where the scheduler may not be called (a spin lock held, an interrupt handler): the next caller
/// of `wake_deferred` from a context that allows it rings the bell.
static KSWAPD_DEFERRED: AtomicBool = AtomicBool::new(false);

/// Wakes kswapd: at once where the caller's context may call the scheduler (`context_allows`: no spin lock, no
/// interrupt handler; 87fbfd71's rule that the allocator never calls the scheduler from a context that may not
/// sleep), else deferred to the next `wake_deferred` or the thread's period. Lock-free until the wake itself.
pub fn wake_kswapd() {
    KSWAPD_KICK.store(true, Relaxed);
    if context_allows() {
        KSWAPD_WAIT.wake_one();
    } else {
        KSWAPD_DEFERRED.store(true, Relaxed);
    }
}

/// kswapd is looking (between its wake and its next sleep; Linux `kswapd_is_running`): proactive compaction waits.
static KSWAPD_RUNNING: AtomicBool = AtomicBool::new(false);

/// Set by the kswapd thread around each look.
pub fn set_kswapd_running(on: bool) {
    KSWAPD_RUNNING.store(on, Relaxed);
}

/// Whether kswapd is looking now (`set_kswapd_running`).
pub fn kswapd_running() -> bool {
    KSWAPD_RUNNING.load(Relaxed)
}

/// A wake is pending that kswapd has not taken yet (`Buddy::want_kswapd` then skips its zone-lock check).
pub fn kswapd_kicked() -> bool {
    KSWAPD_KICK.load(Relaxed)
}

/// Rings a wake `wake_kswapd` had to leave for later, where the context allows it (the slow paths that may reclaim).
pub fn wake_deferred() {
    if KSWAPD_DEFERRED.load(Relaxed) && context_allows() && KSWAPD_DEFERRED.swap(false, Relaxed) {
        KSWAPD_WAIT.wake_one();
    }
    crate::compact::wake_deferred();
}

/// kswapd's wait between looks: until `wake_kswapd` or `timeout_ns`; true if woken.
pub fn kswapd_sleep(timeout_ns: u64) -> bool {
    let woke = KSWAPD_WAIT.wait_until_timeout(|| KSWAPD_KICK.load(Relaxed), timeout_ns);
    KSWAPD_KICK.store(false, Relaxed);
    woke
}

/// Bumps the global klog counters (the boot allocator only).
pub(crate) fn count_global(pages: u64, outcome: Option<bool>) {
    match outcome {
        None => {
            DIRECT_RUNS.inc();
        }
        Some(true) => DIRECT_RESCUED.inc(),
        Some(false) => DIRECT_FAILED.inc(),
    }
    if pages > 0 {
        DIRECT_PAGES.add(pages);
    }
}

/// Threads inside page-cache reclaim now (ksync thread ids; 0 = a free slot). FS-STALL: a heap allocation made
/// inside reclaim (the grace queue growing for the pages it takes) must not reclaim again, or reclaim recurses.
static INSIDE: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// FS-STALL: notes frame `phys`'s owner (the page cache: its cache and page, `vfs::reclaim`); 0 clears it.
pub fn set_frame_owner(phys: u64, v: u64) {
    if let Some(b) = crate::boot_buddy() {
        b.set_owner(phys, v);
    }
}

/// Whether the caller's context lets it reclaim for the heap (FS-STALL review R1/R2): no spin lock held (reclaim
/// frees into the grace queue under `mm.grace` and takes cache index locks: a holder of one of those would spin on
/// itself), not in an interrupt handler or with interrupts off (an NMI too), not inside a grace section. The heap
/// then answers null as before; kswapd and the reserve are for those callers.
pub fn context_allows() -> bool {
    if ksync::preempt_count() != 0 || hal::percpu::in_hard_irq() || crate::grace::in_section() {
        return false;
    }
    #[cfg(target_os = "none")]
    if !hal::cpu::interrupts_enabled() {
        return false;
    }
    true
}

/// Marks the calling thread as inside reclaim until the guard drops; None when it is inside already (or no
/// thread id, or every slot is taken: then the caller does not reclaim).
pub fn enter() -> Option<Inside> {
    let me = ksync::thread_id();
    if me == 0 || INSIDE.iter().any(|s| s.load(Relaxed) == me) {
        return None;
    }
    INSIDE.iter().position(|s| s.compare_exchange(0, me, Relaxed, Relaxed).is_ok()).map(Inside)
}

pub struct Inside(usize);

/// Threads whose heap allocation leaves the frames reserve alone for now (`keep_reserve`).
static KEEP: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// A slot whose thread's allocation is suspended by a nested one (`Keep::Paused`).
const PAUSED: u64 = 1 << 63;

fn inside(me: u64) -> bool {
    INSIDE.iter().any(|s| s.load(Relaxed) == me)
}

/// The heap's tries before the last one, where it may reclaim: it does not take the reserve (the frames below
/// the min watermark are for callers under spin locks, which cannot reclaim). None where reclaim is not allowed
/// or the thread is reclaiming (the heap takes the reserve then, as Linux's PF_MEMALLOC does: what frees memory
/// must get the little it needs for that), or with no slot.
///
/// A heap allocation made while the same thread's outer one holds a slot (the grace queue growing inside the
/// outer one's reclaim round, the vmap bookkeeping of a scattered block, an interrupt handler on top of it)
/// pauses that slot for its own duration: the outer allocation's patience is not the inner one's. Mini-PC
/// 11.11.2026, generation 114: the inner ones were refused the reserve, reclaim freed nothing, and with the
/// patient bound everything stood for seconds.
pub fn keep_reserve() -> Option<Keep> {
    let me = ksync::thread_id();
    if me == 0 {
        return None;
    }
    if let Some(i) = KEEP.iter().position(|s| s.compare_exchange(me, me | PAUSED, Relaxed, Relaxed).is_ok()) {
        return Some(Keep::Paused(i));
    }
    if !context_allows() || inside(me) {
        return None;
    }
    KEEP.iter().position(|s| s.compare_exchange(0, me, Relaxed, Relaxed).is_ok()).map(Keep::Held)
}

/// Whether the calling thread's heap allocation keeps the reserve: asked only when a zone is below it.
pub fn keeps_reserve() -> bool {
    // An interrupt handler runs with the interrupted thread's id: that thread's slot is not the handler's, which
    // cannot reclaim and is what the reserves are for.
    if hal::percpu::in_hard_irq() {
        return false;
    }
    let me = ksync::thread_id();
    me != 0 && KEEP.iter().any(|s| s.load(Relaxed) == me)
}

/// Whether the calling thread is inside page-cache reclaim (`enter`): its allocations are not patient.
pub fn reclaiming() -> bool {
    let me = ksync::thread_id();
    me != 0 && inside(me)
}

pub enum Keep {
    /// This allocation keeps the reserve until the guard drops.
    Held(usize),
    /// The thread's outer allocation keeps it again when the guard drops.
    Paused(usize),
}

impl Keep {
    /// Whether this allocation itself keeps the reserve (and so has a last try without it).
    pub fn held(&self) -> bool {
        matches!(self, Keep::Held(_))
    }
}

impl Drop for Keep {
    fn drop(&mut self) {
        match *self {
            Keep::Held(i) => KEEP[i].store(0, Relaxed),
            Keep::Paused(i) => {
                let _ = KEEP[i].fetch_and(!PAUSED, Relaxed);
            }
        }
    }
}

/// Threads whose heap allocations stay short (`hurry`).
static HURRY: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

/// Marks the calling thread as one that must not stall until the guard drops: its heap allocations that find no
/// memory answer null after the short bound instead of the patient one (`KernelHeap::regain`). For threads that
/// others wait on and that handle a refusal themselves (the network poll round: a stalled round stops receive
/// processing for every connection). None with no thread id or no slot: the thread is then patient like any other.
pub fn hurry() -> Option<Hurry> {
    let me = ksync::thread_id();
    if me == 0 || hurried() {
        return None;
    }
    HURRY.iter().position(|s| s.compare_exchange(0, me, Relaxed, Relaxed).is_ok()).map(Hurry)
}

/// Whether the calling thread is inside `hurry`: asked only on the heap's slow path.
pub fn hurried() -> bool {
    let me = ksync::thread_id();
    me != 0 && HURRY.iter().any(|s| s.load(Relaxed) == me)
}

pub struct Hurry(usize);

impl Drop for Hurry {
    fn drop(&mut self) {
        HURRY[self.0].store(0, Relaxed);
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        INSIDE[self.0].store(0, Relaxed);
    }
}

/// Installs the page cache's reclaimer into the boot allocator (crate mm's core initcall). False: no boot
/// allocator, or one was installed already.
pub fn set_reclaimer(r: &'static dyn Reclaim) -> bool {
    crate::boot_buddy().is_some_and(|b| b.set_reclaimer(r))
}

/// The boot allocator's direct reclaim counts (zeros without one).
pub fn direct_stats() -> DirectStats {
    crate::boot_buddy().map(|b| b.direct_reclaim()).unwrap_or_default()
}
