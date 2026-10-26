//! MM-9: the driver of page-cache reclaim. Installs vfs's reclaimer into the frame allocator's slow path
//! (direct reclaim, `mm_core::reclaim`) and runs `mm.kswapd`, the Background thread that keeps free memory
//! between the low and the high watermark (linux-6.6/mm/vmscan.c `kswapd` :7713, `balance_pgdat` :7391).
//!
//! The allocator never wakes the thread (that would put a scheduler call into every context that allocates);
//! the thread looks at the free frames every `PERIOD_MS` instead, and sooner work falls to direct reclaim.
//! It also turns the allocator's direct-reclaim counts into events (mm-core cannot emit them itself).

use mm_core::reclaim::{self as mr, DirectStats, Reclaim, Reclaimed, Watermarks};
use vfs::reclaim::{self as vr, Mode};

/// How often the thread looks at the free frames.
pub const PERIOD_MS: u64 = 200;
/// Pages one background call asks for (4 MiB), so a cache lock is never held long.
pub const STEP: u64 = 1024;

struct Direct;

impl Reclaim for Direct {
    fn reclaim(&self, pages: u64, thorough: bool) -> Reclaimed {
        let o = vr::shrink(pages, Mode::Direct, thorough);
        Reclaimed { freed: o.freed, busy: o.busy > 0 || o.cut }
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

/// (total, free) frames now; None without a frame allocator.
fn frames() -> Option<(u64, u64)> {
    let s = mm_core::frames()?.stats();
    Some((s.total_bytes / mm_core::PAGE, s.free_bytes / mm_core::PAGE))
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
}

impl Balancer {
    /// One look: below the low watermark (or after direct reclaim ran while below high), reclaims up to the high
    /// one in steps of `STEP`; a light step that frees nothing is followed by a thorough one, and when that
    /// frees nothing either the rest is mapped, dirty or busy and the look ends. Returns the pages freed.
    pub fn look(&mut self) -> u64 {
        // Direct reclaim ran since the last look (compared before `report_direct` takes the new snapshot).
        let runs_before = self.direct.runs;
        self.report_direct();
        let pushed = self.direct.runs != runs_before;
        let Some((total, mut free)) = frames() else { return 0 };
        let w = mr::watermarks(total);
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
            let o = vr::shrink(STEP.min(w.high - free), Mode::Background, thorough);
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
            sched::sleep_ms(PERIOD_MS);
            b.look();
        }
    })
    .map_err(|_| "mm.kswapd thread not started")?;
    klog::event!(Info, "mm", "kswapd.started", min_kb = w.min * 4, low_kb = w.low * 4, high_kb = w.high * 4, period_ms = PERIOD_MS);
    Ok(())
}
kreg::initcall!(kreg::Level::Late, "mm.kswapd", start);
