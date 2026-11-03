//! WY-13: a guard against a busy main loop. The loop sleeps until the earliest of its wake-up times (`pacer`); a
//! time that stays in the past because its tick does not move it on wakes the loop at once, turn after turn
//! (27.10., generation 32: a full core on a static screen). Each turn is counted (`ctl stats`), and a run of more
//! than `STREAK` turns in a row that sleep until a past time without drawing a frame gets one line a minute,
//! naming the source. A loop woken that often by its fds instead (more than `BUSY_TURNS` turns in a second
//! without a frame, no past time: data a source reports but nobody reads) gets one line a minute too, naming which
//! fd sources fired (`Woke`, counted in their callbacks). Pure over `pacer` times, so the host tests drive it.

use std::time::Duration;

use crate::stats::Stats;

/// Turns in a row before the line.
pub const STREAK: u64 = 100;
/// At most one line per this long.
pub const LOG_EVERY: Duration = Duration::from_secs(60);
/// Turns within one second without a frame that count as busy (an idle loop turns a few times a second, a
/// client talking without frames some dozens).
pub const BUSY_TURNS: u64 = 200;

/// How often each fd source of the main loop fired (their callbacks count); the rest is "other" (input, the
/// control socket, the back end's wake-up, signals, the services).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Woke {
    /// A client socket's own watch (`clients::watch`).
    pub clients: u64,
    /// The display's epoll fd.
    pub display: u64,
    /// The frame and deadline timer (`pacer`).
    pub timer: u64,
}

/// The earliest of the named wake-up times.
pub fn earliest(list: &[(Option<Duration>, &'static str)]) -> Option<(Duration, &'static str)> {
    list.iter().filter_map(|&(t, name)| t.map(|t| (t, name))).min_by_key(|&(t, _)| t)
}

#[derive(Default)]
pub struct Guard {
    streak: u64,
    frames: u64,
    logged: Option<Duration>,
    /// The current one-second window: its start, its turns, whether a frame was drawn in it.
    window: Duration,
    window_turns: u64,
    window_drew: bool,
    busy_logged: Option<Duration>,
    /// `Woke` at the last turn, the window's fires per source, and its turns no counted source woke.
    last: Woke,
    window_woke: Woke,
    window_other: u64,
}

impl Guard {
    /// One loop turn about to sleep until `wake` at `now`, with `frames` drawn and the sources' fires `woke` so
    /// far; a line to log, if any.
    pub fn turn(
        &mut self,
        now: Duration,
        wake: Option<(Duration, &'static str)>,
        frames: u64,
        woke: Woke,
        stats: &mut Stats,
    ) -> Option<String> {
        stats.loop_turns += 1;
        self.count_woke(woke);
        let past = wake.filter(|&(t, _)| t <= now);
        if past.is_some() {
            stats.past_wakes += 1;
        }
        let drew = frames != self.frames;
        self.frames = frames;
        let busy = self.busy_window(now, drew, past.is_some());
        let (_, source) = match past {
            Some(p) if !drew => p,
            _ => {
                self.streak = 0;
                return busy;
            }
        };
        self.streak += 1;
        if self.streak <= STREAK || self.logged.is_some_and(|t| now.saturating_sub(t) < LOG_EVERY) {
            return busy;
        }
        self.logged = Some(now);
        Some(format!(
            "loop: {} turns in a row woke for a time already past without drawing a frame (source: {source}; ctl stats past_wakes={})",
            self.streak, stats.past_wakes
        ))
    }

    fn count_woke(&mut self, woke: Woke) {
        let d = Woke {
            clients: woke.clients.saturating_sub(self.last.clients),
            display: woke.display.saturating_sub(self.last.display),
            timer: woke.timer.saturating_sub(self.last.timer),
        };
        self.last = woke;
        self.window_woke.clients += d.clients;
        self.window_woke.display += d.display;
        self.window_woke.timer += d.timer;
        if d == Woke::default() {
            self.window_other += 1;
        }
    }

    /// Counts the turn into its one-second window; a line when a finished window was busy without frames.
    fn busy_window(&mut self, now: Duration, drew: bool, past: bool) -> Option<String> {
        if now.saturating_sub(self.window) < Duration::from_secs(1) {
            self.window_turns += 1;
            self.window_drew |= drew || past;
            return None;
        }
        let (turns, quiet, woke, other) = (self.window_turns, !self.window_drew, self.window_woke, self.window_other);
        (self.window, self.window_turns, self.window_drew) = (now, 1, drew || past);
        (self.window_woke, self.window_other) = (Woke::default(), 0);
        if turns <= BUSY_TURNS || !quiet || self.busy_logged.is_some_and(|t| now.saturating_sub(t) < LOG_EVERY) {
            return None;
        }
        self.busy_logged = Some(now);
        Some(format!(
            "loop: {turns} turns in one second without a frame or a past wake-up time (a source keeps waking it: client sockets {} display fd {} timer {} other {other})",
            woke.clients, woke.display, woke.timer
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn earliest_names_its_source() {
        assert_eq!(earliest(&[(None, "frame"), (Some(ms(9)), "answer"), (Some(ms(3)), "veil")]), Some((ms(3), "veil")));
        assert_eq!(earliest(&[(None, "frame")]), None);
    }

    #[test]
    fn a_due_left_in_the_past_is_named_once_a_minute() {
        let (mut g, mut s) = (Guard::default(), Stats::default());
        let mut lines = Vec::new();
        for k in 0..1000 {
            let now = ms(1000 + k);
            lines.extend(g.turn(now, Some((ms(500), "answer")), 0, Woke::default(), &mut s));
        }
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("source: answer"), "{}", lines[0]);
        assert_eq!((s.loop_turns, s.past_wakes), (1000, 1000));
        // A minute later the same spin is named again.
        assert!(g.turn(ms(61_100), Some((ms(500), "answer")), 0, Woke::default(), &mut s).is_some());
    }

    /// Every turn woken by a client socket's watch.
    fn w(k: u64) -> Woke {
        Woke { clients: k, ..Woke::default() }
    }

    #[test]
    fn a_few_hundred_turns_a_second_are_named_and_a_quiet_loop_is_not() {
        let (mut g, mut s) = (Guard::default(), Stats::default());
        let mut lines = Vec::new();
        // 300 timer-woken turns a second for 3 s without a frame.
        for k in 0..900u64 {
            let now = Duration::from_micros(3333 * k);
            lines.extend(g.turn(now, Some((now + ms(500), "status")), 0, Woke { timer: k, ..Woke::default() }, &mut s));
        }
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("client sockets 0 display fd 0 timer 30"), "{}", lines[0]);
        // 20 turns a second (an idle loop with a talking client): no line.
        let (mut g, mut s) = (Guard::default(), Stats::default());
        for k in 0..200u64 {
            assert!(g.turn(ms(50 * k), Some((ms(50 * k + 500), "status")), 0, w(k), &mut s).is_none());
        }
    }

    #[test]
    fn woken_by_fds_without_frames_is_named_once_a_minute() {
        let (mut g, mut s) = (Guard::default(), Stats::default());
        let mut lines = Vec::new();
        // 5000 turns a second for 3 s, each sleeping until a time ahead: a source keeps waking the loop.
        for k in 0..15_000u64 {
            let now = Duration::from_micros(200 * k);
            lines.extend(g.turn(now, Some((now + ms(500), "status")), 7, w(k), &mut s));
        }
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("a source keeps waking it: client sockets 5000"), "{}", lines[0]);
        assert_eq!(s.past_wakes, 0);
    }

    #[test]
    fn future_wakes_and_drawn_frames_are_no_spin() {
        let (mut g, mut s) = (Guard::default(), Stats::default());
        for k in 0..500u64 {
            let now = ms(k);
            assert!(g.turn(now, Some((now + ms(16), "frame")), k, Woke::default(), &mut s).is_none());
            // Behind but drawing every turn (a long render): no line either.
            assert!(g.turn(now, Some((now, "frame")), k + 1, Woke::default(), &mut s).is_none());
        }
        assert!(g.turn(ms(600), None, 0, Woke::default(), &mut s).is_none());
        assert_eq!(s.past_wakes, 500);
    }
}
