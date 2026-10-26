//! Motion (plan/spec/desktop.md §2, WY-5): visible but short, never at the cost of input. With theme.motion
//! `medium` the chrome fades in and the bars glide into place at start (well under 2 s) and the circle menu
//! unfolds (fades in, ~120 ms); `calm` keeps only the fades, `off` nothing. Everything else is still: an idle
//! desktop draws no frames. Only image alphas and offsets change, nothing is redrawn for it.

use std::time::Duration;

use crate::theme::{Motion, Theme};

pub const START: Duration = Duration::from_millis(600);
pub const MENU: Duration = Duration::from_millis(120);
/// How far the top and bottom bars glide (logical pixels).
pub const GLIDE: f32 = 12.0;

#[derive(Default)]
pub struct Anim {
    start: Option<Duration>,
    menu: Option<Duration>,
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
        self.start.get_or_insert(now);
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
        let started = self.start.is_none_or(|t0| now < t0 + START);
        let menu = self.menu.is_some_and(|t0| now < t0 + MENU);
        started || menu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
