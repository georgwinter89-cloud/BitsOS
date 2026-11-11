//! MM-9: the driver of page-cache reclaim. Installs vfs's reclaimer into the frame allocator's slow path
//! (direct reclaim, `mm_core::reclaim`) and runs `mm.kswapd`, the Background thread that keeps free memory
//! between the low and the high watermark (linux-6.6/mm/vmscan.c `kswapd` :7713, `balance_pgdat` :7391).
//!
//! W35 MM-1: the thread is woken (Linux `wakeup_kswapd`): a heap block that found no free run notes its order
//! (`mm_core::OrderWant`), so does a huge-page fault without a free 2 MiB block (`want_order`) and every frame
//! allocation that failed (`mm_core::reclaim::FRAME_WANT`, order 0 only rings); every note wakes it at once where
//! the caller may call the scheduler, else at the next slow path that may (`mm_core::reclaim::wake_deferred`).
//! The look every `PERIOD_MS` stays as the backstop, and woken looks come at most every `MIN_GAP_MS` (a fault
//! storm without a free block must not keep it busy). W35 MM-3: the look hands each noted order the machine is not
//! balanced for (`Buddy::order_balanced`, Linux `pgdat_balanced`) to kcompactd (`crate::compact`), which makes the
//! block for the callers that cannot or must not compact themselves. It also turns the allocator's direct-reclaim
//! counts into events (mm-core cannot emit them itself).

use mm_core::reclaim::{self as mr, DirectStats, Reclaim, Reclaimed, Watermarks};
use vfs::reclaim::{self as vr, Mode};

/// How often the thread looks at the free frames without being woken.
pub const PERIOD_MS: u64 = 200;
/// The shortest time between two looks when woken (at most 50 woken looks a second, each bounded).
pub const MIN_GAP_MS: u64 = 20;
/// Pages one background call asks for (4 MiB), so a cache lock is never held long.
pub const STEP: u64 = 1024;

/// Orders a 2 MiB fault found no free block for (`want_order`; the heap keeps its own, `mm_core::HEAP.order_want`).
static FAULT_WANT: mm_core::OrderWant = mm_core::OrderWant::new();

/// A fault found no free block of `order` (THP: 9): kswapd is woken to make one (Linux `wakeup_kswapd` with
/// the order). Lock-free, from any context.
pub(crate) fn want_order(order: u32) {
    FAULT_WANT.note(order);
}

struct Direct;

impl Reclaim for Direct {
    fn reclaim(&self, pages: u64, thorough: bool) -> Reclaimed {
        let o = vr::shrink(pages, Mode::Direct, thorough);
        Reclaimed { freed: o.freed, busy: o.busy > 0 || o.cut }
    }
    fn reclaim_frame(&self, phys: u64, owner: u64) -> bool {
        vr::reclaim_frame(phys, owner)
    }
}

static DIRECT: Direct = Direct;

fn install() -> Result<(), &'static str> {
    if !mr::set_reclaimer(&DIRECT) {
        return Err("no boot frame allocator, or a reclaimer is installed already");
    }
    Ok(())
}
kreg::initcall!(kreg::Level::Core, "mm.reclaim", install);

/// (total, free) frames now; None without a frame allocator. PF-5: frames waiting for a grace period count as
/// free (`look` drains them first), so kswapd does not reclaim more cache for memory that is about to return.
/// The HighAtomic reserve does not count (Linux `__zone_watermark_unusable_free`): with it kswapd would see free
/// memory above the high watermark while nothing an ordinary allocation can take is left.
fn frames() -> Option<(u64, u64)> {
    let s = mm_core::frames()?.stats();
    Some((s.total_bytes / mm_core::PAGE, (s.usable_free_bytes() + s.pending_bytes) / mm_core::PAGE))
}

/// The machine's watermarks now (zeros without a frame allocator).
pub fn watermarks() -> Watermarks {
    frames().map(|(t, _)| mr::watermarks(t)).unwrap_or_default()
}

/// The first pressure episode is told at Info, later ones at Debug (a machine whose cache fills memory meets the
/// watermarks often; B6 without flooding the log).
macro_rules! info_once {
    ($first:expr, $($t:tt)+) => {
        if $first {
            klog::event!(Info, $($t)+)
        } else {
            klog::event!(Debug, $($t)+)
        }
    };
}

/// What the thread remembers between looks.
#[derive(Default)]
pub struct Balancer {
    /// A pressure episode is running (free fell below low, not yet back at high).
    active: bool,
    episodes: u64,
    freed: u64,
    told_stuck: bool,
    direct: DirectStats,
    /// A noted order is short (no free block of it): handed to kcompactd, not yet balanced.
    order_active: bool,
    order_episodes: u64,
}

impl Balancer {
    /// One look: the free count (`balance_count`), then the heap's pending orders (`balance_order`). Returns the
    /// pages the first freed.
    pub fn look(&mut self) -> u64 {
        let freed = self.balance_count();
        self.balance_order();
        freed
    }

    /// Below the low watermark (or after direct reclaim ran while below high), reclaims up to the high one in
    /// steps of `STEP`; a light step that frees nothing is followed by a thorough one, and when that frees
    /// nothing either the rest is mapped, dirty or busy and the look ends.
    fn balance_count(&mut self) -> u64 {
        // Direct reclaim ran since the last look (compared before `report_direct` takes the new snapshot).
        let runs_before = self.direct.runs;
        self.report_direct();
        let pushed = self.direct.runs != runs_before;
        // PF-5: what passed its grace period goes back first; below the low watermark the rest is waited for
        // (bounded) before any page is reclaimed. Heap blocks whose free had to wait go back first too.
        mm_core::grace::drain();
        mm_core::HEAP.drain_scattered();
        let Some((total, mut free)) = frames() else { return 0 };
        let w = mr::watermarks(total);
        if free < w.low && mm_core::grace::pending_bytes() > 0 {
            mm_core::grace::expedite();
            free = frames().map_or(free, |(_, f)| f);
        }
        if free >= w.low && !(pushed && free < w.high) {
            self.end_episode(free);
            return 0;
        }
        if !self.active {
            self.active = true;
            self.episodes += 1;
            self.freed = 0;
            self.told_stuck = false;
            info_once!(self.episodes == 1, "mm", "kswapd.pressure", free_kb = free * 4, low_kb = w.low * 4, high_kb = w.high * 4,
                episode = self.episodes; "free memory below the low watermark: taking clean page-cache pages back");
        }
        let mut freed = 0;
        let mut thorough = false;
        while free < w.high {
            let o = {
                let _inside = mr::enter();
                vr::shrink(STEP.min(w.high - free), Mode::Background, thorough)
            };
            freed += o.freed;
            if o.freed == 0 {
                if thorough {
                    if !self.told_stuck {
                        self.told_stuck = true;
                        klog::event!(Warn, "mm", "kswapd.nothing_reclaimable", free_kb = free * 4, high_kb = w.high * 4,
                            held = o.held, dirty = o.dirty, busy = o.busy; "the rest of the page cache is mapped, dirty or busy");
                    }
                    break;
                }
                thorough = true;
                continue;
            }
            thorough = false;
            // The pages just taken wait for a grace period (lockless readers): wait for it, then count again.
            mm_core::grace::expedite();
            sched::yield_now();
            free = frames().map_or(free, |(_, f)| f);
        }
        self.freed += freed;
        if free >= w.high {
            self.end_episode(free);
        }
        // `self.direct` stays the snapshot of this look's start: runs and failures during the look are seen by
        // the next one.
        freed
    }

    /// The orders heap runs, huge-page faults and failed frame allocations were missing since the last look,
    /// highest first (W35 MM-3, Linux `kswapd_try_to_sleep` -> `wakeup_kcompactd`): once the count is balanced, an
    /// order the machine is not balanced for (`Buddy::order_balanced`, Linux `pgdat_balanced`) goes to kcompactd,
    /// which makes the block by compaction (`crate::compact`). A block of one order serves every lower one, so the
    /// first balanced order ends the look. The next allocation or fault that misses an order notes it again.
    fn balance_order(&mut self) {
        let want = mm_core::HEAP.order_want();
        let mut wanted = want.take() | FAULT_WANT.take() | mm_core::reclaim::FRAME_WANT.take();
        if wanted == 0 {
            self.order_active = false;
            return;
        }
        let Some(b) = mm_core::boot_buddy() else { return };
        let mut short = None;
        while wanted != 0 {
            let order = 31 - wanted.leading_zeros();
            wanted &= !(1 << order);
            if b.order_balanced(order) {
                break;
            }
            mm_core::compact::wake_kcompactd(order);
            short = short.or(Some(order));
        }
        match short {
            Some(order) if !self.order_active => {
                self.order_active = true;
                self.order_episodes += 1;
                info_once!(self.order_episodes == 1, "mm", "kswapd.order_short", order = order, noted = want.noted(), faults = FAULT_WANT.noted(),
                    episode = self.order_episodes; "a heap block, a huge-page fault or a frame allocation found no free block of its order: kcompactd compacts for it");
            }
            Some(_) => {}
            None => self.order_active = false,
        }
    }

    fn end_episode(&mut self, free: u64) {
        if self.active {
            self.active = false;
            info_once!(self.episodes == 1, "mm", "kswapd.balanced", freed = self.freed, free_kb = free * 4, episode = self.episodes);
        }
    }

    /// Direct reclaim that failed since the last look is a warning (an ENOMEM with the cache all mapped or dirty).
    fn report_direct(&mut self) {
        let d = mr::direct_stats();
        if d.failed > self.direct.failed {
            klog::event!(Warn, "mm", "reclaim.direct_failed", count = d.failed - self.direct.failed, runs = d.runs, pages = d.pages;
                "an allocation found no frame even after reclaim: ENOMEM");
        }
        self.direct = d;
    }
}

fn start() -> Result<(), &'static str> {
    let w = watermarks();
    sched::spawn("mm.kswapd", sched::Class::Background, || {
        let mut b = Balancer::default();
        loop {
            sched::sleep_ms(MIN_GAP_MS);
            mm_core::reclaim::kswapd_sleep((PERIOD_MS - MIN_GAP_MS) * 1_000_000);
            // Wakes left for later under a spin lock (kswapd's own, kcompactd's: a THP fault under the space's
            // lock, an order noted under the heap's lock) are rung here at the latest, once per period.
            mm_core::reclaim::wake_deferred();
            mm_core::reclaim::set_kswapd_running(true);
            b.look();
            mm_core::reclaim::set_kswapd_running(false);
        }
    })
    .map_err(|_| "mm.kswapd thread not started")?;
    klog::event!(Info, "mm", "kswapd.started", min_kb = w.min * 4, low_kb = w.low * 4, high_kb = w.high * 4, period_ms = PERIOD_MS);
    Ok(())
}
kreg::initcall!(kreg::Level::Late, "mm.kswapd", start);
