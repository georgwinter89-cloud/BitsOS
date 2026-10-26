//! When `serve` takes its automatic rounds (VS-2b): on the wall clock's multiples of `snap.interval_min`
//! (:00, :05, ...), the first one at start. The loop wakes at least once a minute to read the settings again
//! (a new interval or `snap.enabled` takes effect within a minute), but a wake-up is not a round: a round runs
//! only when its time has come. VS-2 ran a round at every wake-up, so one per minute (desk-minipc-w11.md S3).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Longest sleep: settings and the data state are read again at least this often.
pub const WAKE_S: u64 = 60;

/// Seconds until the next multiple of `interval_s` on the wall clock (at least 1).
pub fn until_next(now_s: u64, interval_s: u64) -> u64 {
    let i = interval_s.max(60);
    (i - now_s % i).max(1)
}

/// The time source of the loop (a fake one in the tests).
pub trait Clock {
    /// Seconds on the clock rounds align to.
    fn now_s(&mut self) -> u64;
    /// Sleeps; false ends the loop (tests only).
    fn sleep_s(&mut self, s: u64) -> bool;
}

/// UTC seconds; before the wall clock is set (0) the seconds since start, so rounds still come.
pub struct RealClock(Instant);

impl RealClock {
    pub fn new() -> RealClock {
        RealClock(Instant::now())
    }
}

impl Default for RealClock {
    fn default() -> Self {
        RealClock::new()
    }
}

impl Clock for RealClock {
    fn now_s(&mut self) -> u64 {
        match SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()) {
            Ok(s) if s > 0 => s,
            _ => self.0.elapsed().as_secs(),
        }
    }
    fn sleep_s(&mut self, s: u64) -> bool {
        std::thread::sleep(Duration::from_secs(s));
        true
    }
}

/// What the loop finds at a wake-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// No rounds now (disabled, /data not ready or never): look again after this many seconds.
    Wait(u64),
    /// Rounds are on, every this many seconds.
    Rounds(u64),
}

/// The next round's time.
#[derive(Debug, Default)]
pub struct Sched {
    next_s: Option<u64>,
    interval_s: u64,
}

impl Sched {
    /// Whether a round is due at `now_s`. The time is set anew when the interval changed or the clock went
    /// back by more than an interval (set while asleep).
    pub fn due(&mut self, now_s: u64, interval_s: u64) -> bool {
        let i = interval_s.max(60);
        match self.next_s {
            None => true,
            Some(n) => {
                if i != self.interval_s || n > now_s.saturating_add(i) {
                    self.next_s = Some(now_s.saturating_add(until_next(now_s, i)));
                    self.interval_s = i;
                }
                self.next_s.is_some_and(|n| now_s >= n)
            }
        }
    }

    /// A round ended at `now_s`: the next one at the next multiple of the interval, at least half an interval
    /// later (the first round at start is off the grid; the next grid point may be seconds away).
    pub fn done(&mut self, now_s: u64, interval_s: u64) {
        let i = interval_s.max(60);
        let mut wait = until_next(now_s, i);
        if wait < i / 2 {
            wait = wait.saturating_add(i);
        }
        self.interval_s = i;
        self.next_s = Some(now_s.saturating_add(wait));
    }

    /// How long to sleep from `now_s`: until the next round, at most `WAKE_S`.
    pub fn sleep_for(&self, now_s: u64) -> u64 {
        self.next_s.map_or(1, |n| n.saturating_sub(now_s)).clamp(1, WAKE_S)
    }
}

/// The scheduler loop: `look` reads settings and the data state at every wake-up, `round` takes one round.
/// Returns when the clock says stop (never with `RealClock`).
pub fn run(clock: &mut impl Clock, mut look: impl FnMut() -> Look, mut round: impl FnMut()) {
    let mut s = Sched::default();
    loop {
        let wait = match look() {
            Look::Wait(w) => w,
            Look::Rounds(i) => {
                if s.due(clock.now_s(), i) {
                    round();
                    s.done(clock.now_s(), i);
                }
                s.sleep_for(clock.now_s())
            }
        };
        if !clock.sleep_s(wait.clamp(1, WAKE_S)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Fake time: sleeping moves it on; `jumps` set the clock at a given time (as an NTP step would).
    struct Fake<'a> {
        t: &'a Cell<u64>,
        end: u64,
        jumps: Vec<(u64, u64)>,
        sleeps: &'a RefCell<Vec<u64>>,
    }

    impl Clock for Fake<'_> {
        fn now_s(&mut self) -> u64 {
            self.t.get()
        }
        fn sleep_s(&mut self, s: u64) -> bool {
            self.sleeps.borrow_mut().push(s);
            let mut t = self.t.get() + s;
            if let Some(i) = self.jumps.iter().position(|&(at, _)| t >= at) {
                t = self.jumps.remove(i).1;
            }
            self.t.set(t);
            t < self.end
        }
    }

    /// Runs the loop from `start` to `end` with `look` (given the time) and a round taking `round_s`; the round
    /// start times.
    fn drive(start: u64, end: u64, jumps: Vec<(u64, u64)>, round_s: u64, look: impl Fn(u64) -> Look) -> (Vec<u64>, Vec<u64>) {
        let t = Cell::new(start);
        let sleeps = RefCell::new(Vec::new());
        let rounds = RefCell::new(Vec::new());
        let mut c = Fake { t: &t, end, jumps, sleeps: &sleeps };
        run(&mut c, || look(t.get()), || {
            rounds.borrow_mut().push(t.get());
            t.set(t.get() + round_s);
        });
        (rounds.into_inner(), sleeps.into_inner())
    }

    const T0: u64 = 1_800_000_000; // a multiple of 300 s

    #[test]
    fn rounds_come_every_interval_not_every_wakeup() {
        // Started 17 s after a grid point, 5 minutes, two hours: the round at start, then on the grid.
        let (r, sleeps) = drive(T0 + 17, T0 + 7200, vec![], 2, |_| Look::Rounds(300));
        assert_eq!(r[0], T0 + 17);
        assert_eq!(&r[1..4], &[T0 + 300, T0 + 600, T0 + 900]);
        assert_eq!(r.len(), 24, "{r:?}");
        assert!(r.windows(2).skip(1).all(|w| w[1] - w[0] == 300), "{r:?}");
        assert!(sleeps.iter().all(|&s| (1..=WAKE_S).contains(&s)), "settings read at least once a minute");
    }

    #[test]
    fn a_start_just_before_a_grid_point_skips_it() {
        let (r, _) = drive(T0 + 290, T0 + 1000, vec![], 1, |_| Look::Rounds(300));
        assert_eq!(r, vec![T0 + 290, T0 + 600, T0 + 900]);
    }

    #[test]
    fn a_new_interval_takes_effect_within_a_minute() {
        // 5 minutes for the first 20 minutes, then 15.
        let (r, _) = drive(T0, T0 + 3600, vec![], 1, |t| Look::Rounds(if t < T0 + 1200 { 300 } else { 900 }));
        assert_eq!(r, vec![T0, T0 + 300, T0 + 600, T0 + 900, T0 + 1800, T0 + 2700]);
    }

    #[test]
    fn disabled_or_not_ready_means_no_rounds() {
        // Pending for 25 s (data state), then on; off between minutes 10 and 20; then on again (the round that
        // fell due while off comes at once).
        let look = |t: u64| match t - T0 {
            0..25 => Look::Wait(10),
            600..1200 => Look::Wait(60),
            _ => Look::Rounds(300),
        };
        let (r, _) = drive(T0, T0 + 1500, vec![], 1, look);
        assert_eq!(r, vec![T0 + 30, T0 + 300, T0 + 1200]);
    }

    #[test]
    fn the_clock_set_back_or_forward() {
        // Set back an hour at T0+400: the next round on the new clock's grid, not an hour later.
        let (r, _) = drive(T0, T0 + 1000, vec![(T0 + 400, T0 - 3600 + 400)], 1, |_| Look::Rounds(300));
        assert_eq!(&r[..2], &[T0, T0 + 300]);
        assert_eq!(r[2], T0 - 3600 + 600, "{r:?}");
        // Set forward two hours: one round at once, not one per missed interval.
        let (r, _) = drive(T0, T0 + 9000, vec![(T0 + 400, T0 + 7600)], 1, |_| Look::Rounds(300));
        assert_eq!(r, vec![T0, T0 + 300, T0 + 7600, T0 + 7800, T0 + 8100, T0 + 8400, T0 + 8700]);
    }

    #[test]
    fn a_slow_round_is_not_repeated() {
        // A round taking 70 s (over the 60 s wake-up) ends past nothing due: the next on the grid.
        let (r, _) = drive(T0, T0 + 1000, vec![], 70, |_| Look::Rounds(300));
        assert_eq!(r, vec![T0, T0 + 300, T0 + 600, T0 + 900]);
    }

    #[test]
    fn ticks_on_the_clock() {
        assert_eq!(until_next(1000 * 300, 300), 300);
        assert_eq!(until_next(1000 * 300 + 299, 300), 1);
        assert_eq!(until_next(7, 30), 53, "intervals below a minute count as one");
    }
}
