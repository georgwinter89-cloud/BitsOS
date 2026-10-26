//! klog's own health, reported by klogd (RB-klog, B6): klog cannot log its own trouble from inside (the ring
//! is what is in trouble), so klogd looks at the counters every round and turns changes into events.
//!
//! - `W klog records.lost lost= total= timeouts= wait_max_us= irqoff_wait_max_us= hold_max_us=`: records the ring
//!   could not take since the last report (at most one report a second).
//! - `W klog ring.slow_hold hold_max_us= ...`: the ring lock was held longer than `SLOW_HOLD_NS` (each new
//!   maximum once, at most one report a second). Holds are microseconds by rule (one record copied in or a
//!   small batch copied out); a long one names a bug.
//! - `I klog boot.stats ...` once, `BOOT_REPORT_NS` after boot: the Mini-PC check reads it (lost=0, small
//!   waits).

use klog::LockStats;

/// A ring hold longer than this is reported.
pub const SLOW_HOLD_NS: u64 = 200_000;
/// When the boot report comes (ns since boot).
pub const BOOT_REPORT_NS: u64 = 10_000_000_000;
/// Reports of the same kind at most this often.
const REPORT_GAP_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Report {
    Lost { lost: u64, total: u64, stats: LockStats },
    SlowHold { stats: LockStats },
    Boot { lost: u64, stats: LockStats },
}

#[derive(Default)]
pub struct Watch {
    lost_seen: u64,
    lost_at: Option<u64>,
    hold_seen: u64,
    hold_at: Option<u64>,
    boot_done: bool,
}

fn quiet(last: Option<u64>, now: u64) -> bool {
    last.is_some_and(|t| now.saturating_sub(t) < REPORT_GAP_NS)
}

impl Watch {
    /// One look: what to report now (at most one report per call; the others come in the next rounds).
    pub fn step(&mut self, now_ns: u64, lost: u64, stats: LockStats) -> Option<Report> {
        if lost > self.lost_seen && !quiet(self.lost_at, now_ns) {
            let r = Report::Lost { lost: lost - self.lost_seen, total: lost, stats };
            self.lost_seen = lost;
            self.lost_at = Some(now_ns);
            return Some(r);
        }
        if stats.hold_max_ns > SLOW_HOLD_NS && stats.hold_max_ns > self.hold_seen && !quiet(self.hold_at, now_ns) {
            self.hold_seen = stats.hold_max_ns;
            self.hold_at = Some(now_ns);
            return Some(Report::SlowHold { stats });
        }
        if !self.boot_done && now_ns >= BOOT_REPORT_NS {
            self.boot_done = true;
            return Some(Report::Boot { lost, stats });
        }
        None
    }
}

pub fn emit(r: Report) {
    let us = |ns: u64| ns / 1000;
    match r {
        Report::Lost { lost, total, stats: s } => klog::event!(Warn, "klog", "records.lost", lost = lost, total = total,
            timeouts = s.timeouts, wait_max_us = us(s.wait_max_ns), irqoff_wait_max_us = us(s.irqoff_wait_max_ns),
            hold_max_us = us(s.hold_max_ns); "the ring was held too long; readers see the hole"),
        Report::SlowHold { stats: s } => klog::event!(Warn, "klog", "ring.slow_hold", hold_max_us = us(s.hold_max_ns),
            limit_us = us(SLOW_HOLD_NS), wait_max_us = us(s.wait_max_ns), irqoff_wait_max_us = us(s.irqoff_wait_max_ns)),
        Report::Boot { lost, stats: s } => klog::event!(Info, "klog", "boot.stats", lost = lost, timeouts = s.timeouts,
            wait_max_us = us(s.wait_max_ns), irqoff_wait_max_us = us(s.irqoff_wait_max_ns), hold_max_us = us(s.hold_max_ns)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(hold: u64) -> LockStats {
        LockStats { hold_max_ns: hold, ..LockStats::default() }
    }

    #[test]
    fn reports_losses_once_a_second_with_the_right_count() {
        let mut w = Watch::default();
        assert_eq!(w.step(1_000, 0, st(5_000)), None);
        assert_eq!(w.step(2_000, 3, st(5_000)), Some(Report::Lost { lost: 3, total: 3, stats: st(5_000) }));
        assert_eq!(w.step(3_000, 5, st(5_000)), None, "quiet for a second");
        let r = w.step(1_000_003_000, 6, st(5_000));
        assert_eq!(r, Some(Report::Lost { lost: 3, total: 6, stats: st(5_000) }), "the quiet second's losses counted too");
    }

    #[test]
    fn reports_each_new_slow_hold_maximum() {
        let mut w = Watch::default();
        assert_eq!(w.step(1, 0, st(SLOW_HOLD_NS)), None, "at the limit is fine");
        assert!(matches!(w.step(2, 0, st(SLOW_HOLD_NS + 1)), Some(Report::SlowHold { .. })));
        assert_eq!(w.step(REPORT_GAP_NS + 3, 0, st(SLOW_HOLD_NS + 1)), None, "same maximum again");
        assert!(matches!(w.step(REPORT_GAP_NS + 4, 0, st(SLOW_HOLD_NS * 2)), Some(Report::SlowHold { .. })));
    }

    #[test]
    fn boot_report_comes_once_after_ten_seconds() {
        let mut w = Watch::default();
        assert_eq!(w.step(BOOT_REPORT_NS - 1, 0, st(1)), None);
        assert_eq!(w.step(BOOT_REPORT_NS, 0, st(1)), Some(Report::Boot { lost: 0, stats: st(1) }));
        assert_eq!(w.step(BOOT_REPORT_NS * 2, 0, st(1)), None);
    }
}
