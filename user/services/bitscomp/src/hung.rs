//! B4 (WY-4): whether a client still answers. Every client with a window gets an xdg_wm_base ping once a
//! second; one whose ping stays unanswered for 2 s is "hung": its window keeps its last frame under a grey veil
//! with "antwortet nicht" and a "Beenden" button, and bitscomp stops sending it frame callbacks, input and
//! configures. A late pong makes it answer again. WY-13: a hung client is still probed, so one lost pong (or a
//! ping or pong left unread in a socket by a missed wake-up) never keeps a live client hung for good. The first
//! probes come once a second, later ones ever further apart (up to `PROBE_MAX`): a stopped program reads nothing,
//! and every probe it does not read waits in its socket and then in bitscomp's bounded output queue.
//! Pure (times are CLOCK_MONOTONIC durations from `pacer::now`), so the host tests drive it with made-up times.

use std::time::Duration;

pub const PING_EVERY: Duration = Duration::from_secs(1);
pub const HUNG_AFTER: Duration = Duration::from_secs(2);
/// Probes of one hang a second apart; after them the gap doubles.
pub const PROBES_FAST: u32 = 4;
/// The longest gap between two probes.
pub const PROBE_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Send a ping now (its serial is the caller's); while hung this is a probe.
    Ping,
    /// The client just became hung.
    Hung,
}

/// What `ShellClient::send_ping` found for a `Step::Ping`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// A new ping went out (Smithay had none open: the old one was answered or its xdg_wm_base is gone).
    New,
    /// Smithay still has a ping open on that xdg_wm_base (its pong was lost or is not read yet).
    Open,
    /// The xdg_wm_base is dead (the client is going away).
    Dead,
}

/// What the caller does after a send (`Tracker::sent`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct After {
    /// Send the open ping once more with its serial (a hung client's probe; Smithay refuses a new one).
    pub resend: bool,
    /// Worth one line: this hang's probe found something else than the last one (B6: changes only).
    pub note: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tracker {
    /// When the last ping or probe went out.
    sent: Option<Duration>,
    /// The unanswered ping's send time.
    pending: Option<Duration>,
    /// Since when it has not answered (the send time of the ping that made it hung); kept across probes, so
    /// the veil's seconds do not start again at every probe.
    hung_since: Option<Duration>,
    /// While hung: when the last probe went out (or it became hung), and the probes of this hang so far.
    probed: Duration,
    probes: u32,
    /// What the last logged probe of this hang found: true = Smithay still had the ping open.
    noted: Option<bool>,
}

/// The gap before probe `n` (counted from 0) of a hang: `PROBES_FAST` of `PING_EVERY`, then doubling up to
/// `PROBE_MAX`.
pub fn probe_gap(n: u32) -> Duration {
    let Some(k) = n.checked_sub(PROBES_FAST - 1) else { return PING_EVERY };
    PING_EVERY.checked_mul(1u32.checked_shl(k).unwrap_or(u32::MAX)).map_or(PROBE_MAX, |g| g.min(PROBE_MAX))
}

impl Tracker {
    pub fn hung(&self) -> bool {
        self.hung_since.is_some()
    }

    /// While hung: since when it has not answered.
    pub fn since(&self) -> Option<Duration> {
        self.hung_since
    }

    /// One look at the client at `now`; at most one thing to do.
    pub fn tick(&mut self, now: Duration) -> Step {
        if self.hung() {
            // WY-13: keep probing; `probed` moves on with every probe, so `due` never stays in the past.
            if now.saturating_sub(self.probed) >= probe_gap(self.probes) {
                self.probed = now;
                self.probes = self.probes.saturating_add(1);
                self.sent = Some(now);
                return Step::Ping;
            }
            return Step::Nothing;
        }
        if let Some(t0) = self.pending {
            if now.saturating_sub(t0) >= HUNG_AFTER {
                self.hung_since = Some(t0);
                self.noted = None;
                self.probes = 0;
                // The first probe one period from now (a `due` left at the old ping's time would wake at once).
                self.probed = now;
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

    /// What sending the ping of `Step::Ping` found, and what to do about it.
    pub fn sent(&mut self, s: Sent) -> After {
        let open = match s {
            Sent::Dead => {
                self.not_sent();
                return After::default();
            }
            Sent::New => false,
            Sent::Open => true,
        };
        if !self.hung() {
            return After::default();
        }
        After { resend: open, note: self.noted.replace(open) != Some(open) }
    }

    /// The ping could not be sent (the client is going away): try again at the next period.
    pub fn not_sent(&mut self) {
        self.pending = None;
    }

    /// The pong came; true when the client was hung and answers again.
    pub fn pong(&mut self) -> bool {
        self.pending = None;
        self.noted = None;
        self.probes = 0;
        self.hung_since.take().is_some()
    }

    /// When `tick` has something to do next (the main loop's wake-up); always ahead of the last `tick`.
    pub fn due(&self) -> Option<Duration> {
        let next_ping = self.sent.map_or(Duration::ZERO, |s| s + PING_EVERY);
        match (self.pending, self.hung()) {
            (_, true) => Some(self.probed + probe_gap(self.probes)),
            (Some(t0), false) => Some(t0 + HUNG_AFTER),
            (None, false) => Some(next_ping),
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
    fn two_seconds_without_pong_is_hung_once_then_probed_every_second() {
        let mut t = Tracker::default();
        assert_eq!(t.tick(ms(0)), Step::Ping);
        // No second ping while one is unanswered.
        assert_eq!(t.tick(ms(1000)), Step::Nothing);
        assert_eq!(t.due(), Some(ms(2000)));
        assert_eq!(t.tick(ms(1999)), Step::Nothing);
        assert_eq!(t.tick(ms(2000)), Step::Hung);
        assert!(t.hung());
        assert_eq!(t.since(), Some(ms(0)));
        // WY-13: hung once, then the first probes a second apart, each one moving the wake-up on.
        assert_eq!(t.due(), Some(ms(3000)));
        assert_eq!(t.tick(ms(2999)), Step::Nothing);
        assert_eq!(t.tick(ms(3001)), Step::Ping);
        for k in 4..(3 + PROBES_FAST as u64) {
            assert_eq!(t.due(), Some(ms((k - 1) * 1000 + 1001)));
            assert_eq!(t.tick(ms(k * 1000)), Step::Nothing);
            assert_eq!(t.tick(ms(k * 1000 + 1)), Step::Ping);
            assert!(t.hung());
            // The veil's seconds keep counting from the first unanswered ping.
            assert_eq!(t.since(), Some(ms(0)));
        }
        // Then the gap grows.
        assert_eq!(t.due(), Some(ms((2 + PROBES_FAST as u64) * 1000 + 1 + 2000)));
    }

    #[test]
    fn a_long_hang_is_probed_a_bounded_number_of_times() {
        // A stopped program reads none of them: ten minutes give a few dozen probes, never one a second.
        let mut t = Tracker::default();
        t.tick(ms(0));
        assert_eq!(t.tick(ms(2000)), Step::Hung);
        let (mut now, mut last, mut probes) = (ms(2000), ms(2000), 0);
        while now < ms(602_000) {
            now = t.due().unwrap();
            assert_eq!(t.tick(now), Step::Ping);
            let gap = now - last;
            assert!(gap >= PING_EVERY && gap <= PROBE_MAX, "gap {gap:?}");
            (last, probes) = (now, probes + 1);
        }
        assert!((PROBES_FAST..=40).contains(&probes), "{probes} probes in ten minutes");
        assert_eq!(probe_gap(1000), PROBE_MAX);
        assert_eq!(probe_gap(u32::MAX), PROBE_MAX);
        // The pong starts the next hang's probes at a second again.
        assert!(t.pong());
        t.tick(now + ms(1000));
        assert_eq!(t.tick(now + ms(3000)), Step::Hung);
        assert_eq!(t.due(), Some(now + ms(4000)));
    }

    #[test]
    fn a_tick_at_due_always_moves_due_on() {
        // The main loop sleeps until `due`: a due that a tick at that time leaves in place is a busy loop.
        let mut t = Tracker::default();
        let mut now = ms(0);
        for turn in 0..50 {
            let due = t.due().expect("always a wake-up");
            now = now.max(due);
            t.tick(now);
            if turn % 7 == 3 {
                t.not_sent();
            }
            assert!(t.due().unwrap() > now, "turn {turn}: due {:?} not after {now:?}", t.due());
        }
        assert!(t.hung());
    }

    #[test]
    fn lost_pong_then_the_next_probes_pong_recovers() {
        let mut t = Tracker::default();
        assert_eq!(t.tick(ms(0)), Step::Ping);
        // That pong is lost.
        assert_eq!(t.tick(ms(2000)), Step::Hung);
        assert_eq!(t.tick(ms(3000)), Step::Ping);
        // Smithay still has the first ping open: the probe sends it again.
        assert_eq!(t.sent(Sent::Open), After { resend: true, note: true });
        assert!(t.hung());
        assert!(t.pong(), "the probe's pong makes it answer again");
        assert!(!t.hung());
        assert_eq!(t.since(), None);
        assert_eq!(t.tick(ms(4000)), Step::Ping);
        assert!(!t.pong());
    }

    #[test]
    fn probe_answered_with_ping_already_pending_stays_hung() {
        let mut t = Tracker::default();
        t.tick(ms(0));
        assert_eq!(t.tick(ms(2000)), Step::Hung);
        assert_eq!(t.tick(ms(2500)), Step::Nothing);
        assert_eq!(t.tick(ms(3000)), Step::Ping);
        assert_eq!(t.sent(Sent::Open), After { resend: true, note: true }, "the first 'still open' of a hang");
        for _ in 0..6 {
            let now = t.due().unwrap();
            assert_eq!(t.tick(now), Step::Ping);
            assert_eq!(t.sent(Sent::Open), After { resend: true, note: false }, "the same finding again: no line");
            assert!(t.hung());
        }
        // A probe that really went out (the old ping is gone) is a change: one line, nothing to re-send; still
        // hung until its pong.
        let now = t.due().unwrap();
        assert_eq!(t.tick(now), Step::Ping);
        assert_eq!(t.sent(Sent::New), After { resend: false, note: true });
        assert!(t.hung());
        assert!(t.pong());
        // Not hung: a ping's result is no news and nothing is re-sent.
        assert_eq!(t.tick(now + ms(1000)), Step::Ping);
        assert_eq!(t.sent(Sent::Open), After::default());
        assert_eq!(t.sent(Sent::New), After::default());
    }

    #[test]
    fn dead_xdg_wm_base_is_not_sent_and_never_re_sent() {
        let mut t = Tracker::default();
        assert_eq!(t.tick(ms(0)), Step::Ping);
        assert_eq!(t.sent(Sent::Dead), After::default());
        assert!(!t.hung());
        assert_eq!(t.tick(ms(3000)), Step::Ping, "an unsent ping makes nobody hung");
        assert_eq!(t.tick(ms(5000)), Step::Hung);
        assert_eq!(t.tick(ms(6000)), Step::Ping);
        assert_eq!(t.sent(Sent::Dead), After::default());
        assert!(t.hung());
    }

    #[test]
    fn unsent_probe_while_hung_keeps_probing() {
        let mut t = Tracker::default();
        t.tick(ms(0));
        assert_eq!(t.tick(ms(2000)), Step::Hung);
        assert_eq!(t.tick(ms(3000)), Step::Ping);
        t.not_sent();
        assert!(t.hung());
        assert_eq!(t.since(), Some(ms(0)));
        assert_eq!(t.due(), Some(ms(4000)));
        assert_eq!(t.tick(ms(4000)), Step::Ping);
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
