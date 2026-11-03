//! Motion (plan/spec/desktop.md §2, WY-5): visible but short, never at the cost of input. With theme.motion
//! `medium` the chrome fades in and the bars glide into place at start (well under 2 s) and the circle menu
//! unfolds (fades in, ~120 ms); `calm` keeps only the fades, `off` nothing. Everything else is still: an idle
//! desktop draws no frames. Only image alphas and offsets change, nothing is redrawn for it.
//! DS-2: Claude's ring moves its dash while it is busy (thinking, working, listening), in steps of `RING_STEP` (at
//! most 10 frames a second, its own small sprite); idle and waiting it stands still and costs nothing.

use std::time::Duration;

use crate::theme::{Motion, Theme};

/// The chrome's start (at most 400 ms, the brief's limit for the desktop's opening at the cheap level, DS-5).
pub const START: Duration = Duration::from_millis(400);
pub const MENU: Duration = Duration::from_millis(120);
/// How far the top and bottom bars glide (logical pixels).
pub const GLIDE: f32 = 12.0;

/// One step of the busy ring: 10 per second at most (scout §3: 5-10 fps, invisible steps at its speed).
pub const RING_STEP: Duration = Duration::from_millis(100);

/// The busy ring's clock. A chrome frame says whether a busy ring is on screen (`shown`); while it is, the clock
/// asks for the next step's frame (`due`, a wake source through `Status::due`), and only that one: no frame comes
/// for it once the ring is idle, waiting or not drawn (a fullscreen window), or motion is off.
#[derive(Default, Debug)]
pub struct RingClock {
    since: Option<Duration>,
    next: Option<Duration>,
}

impl RingClock {
    /// Called by every chrome frame; the dash step to draw (0 when still).
    pub fn shown(&mut self, busy: bool, now: Duration, t: &Theme) -> u32 {
        if !busy || t.motion == Motion::Off {
            self.since = None;
            self.next = None;
            return 0;
        }
        let t0 = *self.since.get_or_insert(now);
        let step = (now.saturating_sub(t0).as_millis() / RING_STEP.as_millis()) as u32;
        self.next = Some(t0 + RING_STEP * (step + 1));
        step
    }

    pub fn due(&self) -> Option<Duration> {
        self.next
    }

    /// True once when the next step is due; the frame it asks for calls `shown` again.
    pub fn tick(&mut self, now: Duration) -> bool {
        if self.next.is_some_and(|n| now >= n) {
            self.next = None;
            return true;
        }
        false
    }
}

#[derive(Default)]
pub struct Anim {
    start: Option<Duration>,
    menu: Option<Duration>,
    /// DS-5: the boot scene lies over the desktop; the chrome's start waits and no frame is asked for it.
    pub hold: bool,
    /// DS-5: the boot scene's fade ends then (frames until the frame after it has taken the scene away).
    pub boot_fade: Option<Duration>,
}

fn ease(k: f32) -> f32 {
    let k = k.clamp(0.0, 1.0);
    1.0 - (1.0 - k) * (1.0 - k) * (1.0 - k)
}

fn progress(since: Option<Duration>, now: Duration, len: Duration) -> f32 {
    since.map_or(1.0, |t0| now.saturating_sub(t0).as_secs_f32() / len.as_secs_f32())
}

impl Anim {
    /// The first chrome frame: the start animation begins.
    pub fn first_frame(&mut self, now: Duration) {
        if !self.hold {
            self.start.get_or_insert(now);
        }
    }

    /// DS-5: the chrome is at rest from now on (the boot scene's fade is the desktop's opening).
    pub fn settle(&mut self, now: Duration) {
        self.start = Some(now.saturating_sub(START));
    }

    pub fn menu_opened(&mut self, now: Duration) {
        self.menu = Some(now);
    }

    /// The chrome's alpha and how far (logical px, towards its edge) a bar still is from its place.
    pub fn chrome(&self, now: Duration, t: &Theme) -> (f32, f32) {
        let len = if t.motion == Motion::Calm { START * 2 / 3 } else { START };
        let k = ease(progress(self.start, now, len));
        match t.motion {
            Motion::Off => (1.0, 0.0),
            Motion::Calm => (k, 0.0),
            Motion::Medium => (k, (1.0 - k) * GLIDE),
        }
    }

    pub fn menu_alpha(&self, now: Duration, t: &Theme) -> f32 {
        if t.motion == Motion::Off { 1.0 } else { ease(progress(self.menu, now, MENU)) }
    }

    /// Whether frames are still needed for motion.
    pub fn running(&self, now: Duration, t: &Theme) -> bool {
        if t.motion == Motion::Off {
            return false;
        }
        let started = !self.hold && self.start.is_none_or(|t0| now < t0 + START);
        let menu = self.menu.is_some_and(|t0| now < t0 + MENU);
        // Until the scene's last frame took it away (`boot_end` clears it), not only until its end time.
        let fade = self.boot_fade.is_some();
        started || menu || fade
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_busy_ring_steps_ten_times_a_second_and_stops_when_idle() {
        let mut t = crate::theme::test_theme().clone();
        let mut r = RingClock::default();
        let t0 = Duration::from_secs(5);
        assert_eq!(r.shown(false, t0, &t), 0);
        assert_eq!(r.due(), None, "idle: no wake");
        assert_eq!(r.shown(true, t0, &t), 0);
        assert_eq!(r.due(), Some(t0 + RING_STEP));
        assert!(!r.tick(t0 + Duration::from_millis(99)));
        assert!(r.tick(t0 + RING_STEP));
        assert_eq!(r.due(), None, "one frame per step, asked again by that frame");
        // A frame comes late: the step follows the clock, not the frame count.
        assert_eq!(r.shown(true, t0 + Duration::from_millis(350), &t), 3);
        assert_eq!(r.due(), Some(t0 + RING_STEP * 4));
        // Over one second at most 10 steps are asked for.
        let mut frames = 0;
        let mut now = t0 + Duration::from_millis(350);
        while now < t0 + Duration::from_millis(1350) {
            now += Duration::from_millis(1);
            if r.tick(now) {
                frames += 1;
                r.shown(true, now, &t);
            }
        }
        assert!(frames <= 10, "{frames} frames in one second");
        // No chrome frame (fullscreen window): after the pending step nothing more is asked.
        assert!(r.tick(now + RING_STEP));
        assert_eq!(r.due(), None);
        // Idle again: still, and the next busy stretch starts at step 0.
        assert_eq!(r.shown(false, now, &t), 0);
        assert_eq!(r.due(), None);
        assert_eq!(r.shown(true, now, &t), 0);
        t.motion = Motion::Off;
        assert_eq!(r.shown(true, now + RING_STEP * 5, &t), 0);
        assert_eq!(r.due(), None, "motion off: the ring stands still");
    }

    /// Fixer DS-2: the clock follows the ring's state through a change sequence. Once the ring is idle or
    /// waiting, the status wake source asks for nothing, the step stays 0 on every frame, and `Anim::running`
    /// never depends on the ring (a busy ring must not make `ctl shot` wait).
    #[test]
    fn a_ring_state_sequence_leaves_no_wake_once_idle() {
        use crate::desk::Ring;
        let t = crate::theme::test_theme().clone();
        let mut st = crate::status::Status::fixed();
        let mut a = Anim::default();
        let t0 = Duration::from_secs(1);
        a.first_frame(t0);
        let mut now = t0 + START + Duration::from_secs(1);
        assert!(!a.running(now, &t), "start fade over");
        // One chrome frame per wake, as the main loop does: tick, then the frame calls `shown`.
        let run = |st: &mut crate::status::Status, ring: Ring, now: &mut Duration, ms: u64| -> (u32, Vec<u32>) {
            let end = *now + Duration::from_millis(ms);
            let mut frames = 0;
            let mut steps = vec![st.ring.shown(crate::ring::moves(ring), *now, &t)];
            while *now < end {
                *now += Duration::from_millis(5);
                if st.due().is_some_and(|d| *now >= d) && st.tick(*now) {
                    frames += 1;
                    steps.push(st.ring.shown(crate::ring::moves(ring), *now, &t));
                }
            }
            (frames, steps)
        };
        for ring in [Ring::Idle, Ring::Thinking, Ring::Idle, Ring::Waiting, Ring::Working, Ring::Waiting] {
            let (frames, steps) = run(&mut st, ring, &mut now, 1000);
            assert!(!a.running(now, &t), "{ring:?}: running() never follows the ring");
            if crate::ring::moves(ring) {
                assert!((5..=10).contains(&frames), "{ring:?}: {frames} frames in one second");
                assert!(steps.windows(2).all(|w| w[1] > w[0]), "{ring:?}: the dash moves on every frame");
            } else {
                assert_eq!(frames, 0, "{ring:?}: a still ring asked for frames");
                assert!(steps.iter().all(|&s| s == 0), "{ring:?}: the step moved while still: {steps:?}");
                assert_eq!(st.due(), None, "{ring:?}: a wake is left once the ring is still");
            }
        }
    }

    /// DS-5: while the boot scene lies over the desktop no frame is asked for the chrome's start; when it fades
    /// the chrome is at rest and frames come until the scene's last frame has taken it away.
    #[test]
    fn the_boot_scene_holds_the_start_and_its_fade_asks_for_frames() {
        let t = crate::theme::test_theme().clone();
        let mut a = Anim { hold: true, ..Anim::default() };
        let t0 = Duration::from_secs(3);
        a.first_frame(t0);
        assert!(!a.running(t0, &t), "the scene is up: idle");
        assert_eq!(a.chrome(t0, &t), (1.0, 0.0), "the desktop under the scene is drawn at rest");
        a.hold = false;
        a.settle(t0 + Duration::from_secs(1));
        a.boot_fade = Some(t0 + Duration::from_secs(1) + Duration::from_millis(300));
        let now = t0 + Duration::from_secs(2);
        assert_eq!(a.chrome(now, &t), (1.0, 0.0));
        assert!(a.running(now, &t), "past its end the fade still needs the frame that ends it");
        a.boot_fade = None;
        assert!(!a.running(now, &t));
        a.first_frame(now + Duration::from_secs(5));
        assert_eq!(a.chrome(now + Duration::from_secs(5), &t), (1.0, 0.0), "the start does not run again");
    }

    #[test]
    fn the_start_fades_in_within_two_seconds_and_off_means_still() {
        let mut t = crate::theme::test_theme().clone();
        let mut a = Anim::default();
        let t0 = Duration::from_secs(10);
        a.first_frame(t0);
        assert_eq!(a.chrome(t0, &t), (0.0, GLIDE));
        assert!(a.running(t0 + Duration::from_millis(300), &t));
        assert_eq!(a.chrome(t0 + Duration::from_secs(2), &t), (1.0, 0.0));
        assert!(!a.running(t0 + Duration::from_secs(2), &t), "under 2 s");
        a.menu_opened(t0);
        assert!(a.menu_alpha(t0 + Duration::from_millis(60), &t) < 1.0);
        assert_eq!(a.menu_alpha(t0 + Duration::from_millis(200), &t), 1.0);
        t.motion = Motion::Off;
        assert_eq!(a.chrome(t0, &t), (1.0, 0.0));
        assert_eq!(a.menu_alpha(t0, &t), 1.0);
        assert!(!a.running(t0, &t));
    }
}
