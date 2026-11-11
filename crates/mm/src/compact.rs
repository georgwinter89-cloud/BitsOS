//! W35 MM-3: `mm.kcompactd`, the Background thread that compacts memory (Linux 7.x mm/compaction.c `kcompactd`,
//! `kcompactd_do_work`, `proactive_compact_node`), and /proc/sys/vm/compact_memory's run (`compact_nodes`).
//!
//! Woken by kswapd for an order still short once the free count is balanced, by a huge-page fault that may not
//! stall (`thp::fault_block`) and by a madvised one whose direct step failed (`mm_core::compact::wake_kcompactd`),
//! it makes a free block of each asked order, highest first, in slices with a yield between. Without a wake it
//! looks every `PERIOD_MS` (Linux `HPAGE_FRAG_CHECK_INTERVAL_MSEC`) whether the fragmentation score is above the
//! high mark of `vm.compaction_proactiveness` and compacts every zone down to the low mark; a run that did not
//! lower the score waits 64 periods (Linux `COMPACT_MAX_DEFER_SHIFT`). Proactiveness 0: no proactive work, the wait
//! is only for wakes (bounded by `IDLE_MS`, a backstop). Before it sleeps, skip hints a direct run asked to clear are
//! cleared (Linux does this when kswapd sleeps).

use mm_core::buddy::How;
use mm_core::compact as cp;

/// Linux `HPAGE_FRAG_CHECK_INTERVAL_MSEC`.
pub const PERIOD_MS: u64 = 500;
/// The wait without proactive work (Linux waits without a timeout; a Background thread may, the bound is a
/// backstop for a lost wake). A wake left for later under a spin lock does not wait for it: kswapd rings deferred
/// wakes every period (`reclaim::wake_deferred` in its loop).
pub const IDLE_MS: u64 = 60_000;
/// The shortest time between two wakes (a storm of huge-page faults without a block must not keep it busy; each
/// wake is bounded anyway, and a deferred order costs only a few atomics).
pub const MIN_GAP_MS: u64 = 500;
/// Periods a proactive run that did not lower the score waits (Linux `default_timeout << COMPACT_MAX_DEFER_SHIFT`).
pub const BACKOFF: u64 = 1 << 6;

macro_rules! info_once {
    ($first:expr, $($t:tt)+) => {
        if $first {
            klog::event!(Info, $($t)+)
        } else {
            klog::event!(Debug, $($t)+)
        }
    };
}

/// What the thread remembers between wakes.
#[derive(Default)]
struct Daemon {
    /// An asked order could not be made (told once per episode).
    stuck: bool,
    stuck_episodes: u64,
    made: u64,
    proactive_active: bool,
    proactive_episodes: u64,
    backoff: bool,
}

impl Daemon {
    /// One wake: the asked orders, else (woken without one, or the period ran out) the proactive check.
    fn work(&mut self) {
        let Some(b) = mm_core::boot_buddy() else { return };
        let mut orders = cp::take_orders();
        if orders != 0 {
            let mut failed = None;
            while orders != 0 {
                let order = 31 - orders.leading_zeros();
                orders &= !(1 << order);
                let had = b.has_free_block(order);
                if b.kcompactd_work(order, &mut sched::yield_now) {
                    if !had {
                        self.made += 1;
                        mm_core::reclaim::KSWAPD_CLEARED.inc();
                    }
                    // A block of this order serves every lower one.
                    break;
                }
                failed = failed.or(Some(order));
            }
            match failed {
                Some(order) if !self.stuck => {
                    self.stuck = true;
                    self.stuck_episodes += 1;
                    let s = b.compact_stats();
                    info_once!(self.stuck_episodes == 1, "mm", "kcompactd.order_stuck", order = order, made = self.made, skipped_anon = s.skipped_anon,
                        dropped = s.dropped, episode = self.stuck_episodes; "no block of this order could be made: its frames are in use, mapped, dirty, anonymous or busy");
                }
                Some(_) => {}
                None => {
                    if self.stuck {
                        self.stuck = false;
                        info_once!(self.stuck_episodes == 1, "mm", "kcompactd.order_made", made = self.made, episode = self.stuck_episodes);
                    }
                }
            }
        } else if b.should_proactive_compact() {
            let before = b.fragmentation_score();
            if !self.proactive_active {
                self.proactive_active = true;
                self.proactive_episodes += 1;
                info_once!(self.proactive_episodes == 1, "mm", "kcompactd.proactive", score = before, high = cp::score_wmark(false),
                    low = cp::score_wmark(true), episode = self.proactive_episodes; "free memory is in pieces: compacting in the background");
            }
            let s = b.compact_all(How::Proactive, &mut sched::yield_now);
            let after = b.fragmentation_score();
            self.backoff = after >= before;
            if after <= cp::score_wmark(true) || self.backoff {
                self.proactive_active = false;
                info_once!(self.proactive_episodes == 1, "mm", "kcompactd.proactive_done", score = after, cleared = s.cleared, dropped = s.dropped,
                    moved = s.moved, backoff = self.backoff, episode = self.proactive_episodes);
            }
        } else {
            self.backoff = false;
        }
        b.reset_skip_if_flushed();
    }

    fn timeout_ms(&self) -> u64 {
        if cp::proactiveness() == 0 {
            IDLE_MS
        } else if self.backoff {
            PERIOD_MS * BACKOFF
        } else {
            PERIOD_MS
        }
    }
}

/// /proc/sys/vm/compact_memory (Linux `compact_nodes`): every zone compacted whole, in the writer's context (a
/// dropping migrator clears only pageblocks that hold free frames already). Returns the run's counts.
pub fn compact_memory() -> mm_core::CompactStats {
    let Some(b) = mm_core::boot_buddy() else { return Default::default() };
    let before = b.fragmentation_score();
    let s = b.compact_all(How::Manual, &mut sched::yield_now);
    klog::event!(Info, "mm", "compact.memory", score_before = before, score_after = b.fragmentation_score(), cleared = s.cleared,
        dropped = s.dropped, moved = s.moved, migrate_scanned = s.migrate_scanned, skipped_anon = s.skipped_anon);
    s
}

fn start() -> Result<(), &'static str> {
    sched::spawn("mm.kcompactd", sched::Class::Background, || {
        let mut d = Daemon::default();
        loop {
            sched::sleep_ms(MIN_GAP_MS);
            cp::kcompactd_sleep((d.timeout_ms() - MIN_GAP_MS) * 1_000_000);
            d.work();
        }
    })
    .map_err(|_| "mm.kcompactd thread not started")?;
    klog::event!(Info, "mm", "kcompactd.started", period_ms = PERIOD_MS, proactiveness = cp::proactiveness(), extfrag_threshold = cp::extfrag_threshold());
    Ok(())
}
kreg::initcall!(kreg::Level::Late, "mm.kcompactd", start);
