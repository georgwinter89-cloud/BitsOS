//! The reaper's stall report: one value that must move on (a sync generation that must pass, a page-cache grace
//! epoch that must advance) and has not for the limit. Not "work was pending for the limit": under a stream of
//! exits every pass leaves new work behind while each generation passes within microseconds, and the newest
//! generation, requested a moment ago, always has CPUs that have not reached it yet (a false `reap.stalled`).

/// Watches one value (`what`) while it is stuck.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Watch {
    what: u64,
    /// When `what` was first seen stuck (0: not watching).
    first: u64,
    /// The last report (or `first`).
    told: u64,
}

impl Watch {
    /// The value watched now, if it is stuck.
    pub(crate) fn watched(&self) -> Option<u64> {
        (self.first != 0).then_some(self.what)
    }

    /// `what` is `stuck` at `now`: Some(ns since it was first seen stuck) once the same value has been stuck for
    /// more than `limit` since it was first seen or last reported (a stall goes on: reported every `limit`).
    pub(crate) fn stalled(&mut self, what: u64, stuck: bool, now: u64, limit: u64) -> Option<u64> {
        if !stuck {
            *self = Self::default();
            return None;
        }
        if self.first == 0 || self.what != what {
            let now = now.max(1);
            *self = Self { what, first: now, told: now };
            return None;
        }
        if now.saturating_sub(self.told) > limit {
            self.told = now;
            return Some(now - self.first);
        }
        None
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::Watch;

    const S: u64 = 1_000_000_000;

    #[test]
    fn a_value_that_moves_on_is_never_a_stall() {
        // A new generation every 2 ms for 3 s, each stuck when first seen (gate 9's sc/20-smp under exits).
        let mut w = Watch::default();
        for k in 0..1500u64 {
            assert_eq!(w.stalled(100 + k, true, S + k * 2_000_000, S), None, "generation {}", 100 + k);
        }
    }

    #[test]
    fn the_same_value_stuck_is_reported_every_limit_with_its_age() {
        let mut w = Watch::default();
        assert_eq!(w.stalled(7, true, 10 * S, S), None);
        assert_eq!(w.watched(), Some(7));
        assert_eq!(w.stalled(7, true, 10 * S + S / 2, S), None);
        assert_eq!(w.stalled(7, true, 11 * S + 1, S), Some(S + 1));
        assert_eq!(w.stalled(7, true, 11 * S + 2, S), None, "once per limit");
        assert_eq!(w.stalled(7, true, 12 * S + 3, S), Some(2 * S + 3), "the age counts from the first sight");
        // It passed: over; a later stuck value starts its own watch.
        assert_eq!(w.stalled(7, false, 12 * S + 4, S), None);
        assert_eq!(w.watched(), None);
        assert_eq!(w.stalled(8, true, 12 * S + 5, S), None);
        assert_eq!(w.stalled(8, true, 13 * S + 6, S), Some(S + 1));
        w.clear();
        assert_eq!(w.watched(), None);
    }
}
