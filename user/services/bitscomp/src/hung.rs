//! B4 (WY-4): whether a client still answers. Every client with a window gets an xdg_wm_base ping once a
//! second; one whose ping stays unanswered for 2 s is "hung": its window keeps its last frame under a grey veil
//! with "antwortet nicht" and a "Beenden" button, and bitscomp stops sending it frame callbacks, input and
//! configures. A late pong makes it answer again. Pure (times are CLOCK_MONOTONIC durations from `pacer::now`), so
//! the host tests drive it with made-up times.

use std::time::Duration;

pub const PING_EVERY: Duration = Duration::from_secs(1);
pub const HUNG_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Send a ping now (its serial is the caller's).
    Ping,
    /// The client just became hung.
    Hung,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tracker {
    /// When the last ping went out.
    sent: Option<Duration>,
    /// The unanswered ping's send time.
    pending: Option<Duration>,
    hung: bool,
}

impl Tracker {
    pub fn hung(&self) -> bool {
        self.hung
    }

    /// While hung: since when it has not answered (the unanswered ping's send time).
    pub fn since(&self) -> Option<Duration> {
        self.pending.filter(|_| self.hung)
    }

    /// One look at the client at `now`; at most one thing to do.
    pub fn tick(&mut self, now: Duration) -> Step {
        if let Some(t0) = self.pending {
            if !self.hung && now.saturating_sub(t0) >= HUNG_AFTER {
                self.hung = true;
                return Step::Hung;
            }
            // One ping at a time (xdg-shell: a new ping before the pong is pointless; Smithay refuses it).
            return Step::Nothing;
        }
        if self.sent.is_none_or(|s| now.saturating_sub(s) >= PING_EVERY) {
            self.sent = Some(now);
            self.pending = Some(now);
            return Step::Ping;
        }
        Step::Nothing
    }

    /// The ping could not be sent (the client is going away): try again at the next period.
    pub fn not_sent(&mut self) {
        self.pending = None;
    }

    /// The pong came; true when the client was hung and answers again.
    pub fn pong(&mut self) -> bool {
        self.pending = None;
        std::mem::replace(&mut self.hung, false)
    }

    /// When `tick` has something to do next (the main loop's wake-up), `None` once hung until a pong.
    pub fn due(&self) -> Option<Duration> {
        match (self.pending, self.hung) {
            (Some(_), true) => None,
            (Some(t0), false) => Some(t0 + HUNG_AFTER),
            (None, _) => Some(self.sent.map_or(Duration::ZERO, |s| s + PING_EVERY)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn answering_client_is_pinged_once_a_second_and_never_hung() {
        let mut t = Tracker::default();
        assert_eq!(t.tick(ms(0)), Step::Ping);
        assert_eq!(t.tick(ms(500)), Step::Nothing);
        assert!(!t.pong());
        assert_eq!(t.due(), Some(ms(1000)));
        assert_eq!(t.tick(ms(999)), Step::Nothing);
        assert_eq!(t.tick(ms(1000)), Step::Ping);
        t.pong();
        for k in 2..100 {
            assert_eq!(t.tick(ms(k * 1000)), Step::Ping);
            assert!(!t.pong());
        }
        assert!(!t.hung());
    }

    #[test]
    fn two_seconds_without_pong_is_hung_once_and_quiet_after() {
        let mut t = Tracker::default();
        assert_eq!(t.tick(ms(0)), Step::Ping);
        // No second ping while one is unanswered.
        assert_eq!(t.tick(ms(1000)), Step::Nothing);
        assert_eq!(t.due(), Some(ms(2000)));
        assert_eq!(t.tick(ms(1999)), Step::Nothing);
        assert_eq!(t.tick(ms(2000)), Step::Hung);
        assert!(t.hung());
        assert_eq!(t.due(), None);
        for k in 3..10 {
            assert_eq!(t.tick(ms(k * 1000)), Step::Nothing);
        }
    }

    #[test]
    fn late_pong_recovers_and_pinging_resumes() {
        let mut t = Tracker::default();
        t.tick(ms(0));
        assert_eq!(t.tick(ms(2500)), Step::Hung);
        assert!(t.pong());
        assert!(!t.hung());
        // The last ping went out at 0, so the next is due at once.
        assert_eq!(t.tick(ms(2600)), Step::Ping);
        assert!(!t.pong());
    }

    #[test]
    fn slow_but_answering_client_is_not_hung() {
        let mut t = Tracker::default();
        t.tick(ms(0));
        assert_eq!(t.tick(ms(1900)), Step::Nothing);
        assert!(!t.pong());
        assert_eq!(t.tick(ms(1950)), Step::Ping);
        assert_eq!(t.tick(ms(3900)), Step::Nothing);
        assert!(!t.hung());
    }

    #[test]
    fn unsent_ping_is_retried_next_period() {
        let mut t = Tracker::default();
        assert_eq!(t.tick(ms(0)), Step::Ping);
        t.not_sent();
        assert_eq!(t.tick(ms(3000)), Step::Ping);
        assert!(!t.hung());
    }
}
