//! WY-5c: a folded side bar comes back for a while when the pointer is pushed against its screen edge for
//! `HOLD` (plan/spec/desktop.md §1): it lies over the tiles and folds again when the pointer leaves it. Pinning
//! (its own button: the list's fold button, Claude's ring) docks it for good (the setting), which ends the reveal.
//! Pure state over `pacer` time, so the timing is testable without a display.

use std::time::Duration;

/// How long the pointer must stay against the edge.
pub const HOLD: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The workspace list.
    Left,
    /// Claude's sidebar.
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reveal {
    /// The edge the pointer is against, since when.
    pushing: Option<(Edge, Duration)>,
    /// The bar shown over the tiles.
    pub shown: Option<Edge>,
}

impl Reveal {
    /// The pointer moved: `at` is the folded edge it is against (if any), `inside` whether it is over the shown
    /// bar. Returns true when the shown bar changed.
    pub fn motion(&mut self, now: Duration, at: Option<Edge>, inside: bool) -> bool {
        match at {
            Some(e) if self.pushing.is_none_or(|(p, _)| p != e) && self.shown != Some(e) => self.pushing = Some((e, now)),
            Some(_) => {}
            None => self.pushing = None,
        }
        if self.shown.is_some() && !inside && at != self.shown {
            self.shown = None;
            return true;
        }
        false
    }

    /// Shows the bar once the pointer was against its edge for `HOLD`; true when it appeared.
    pub fn tick(&mut self, now: Duration) -> bool {
        match self.pushing {
            Some((e, since)) if now >= since + HOLD => {
                self.pushing = None;
                let changed = self.shown != Some(e);
                self.shown = Some(e);
                changed
            }
            _ => false,
        }
    }

    /// When `tick` has something to do.
    pub fn due(&self) -> Option<Duration> {
        self.pushing.map(|(_, since)| since + HOLD)
    }

    /// Ends the reveal (the bar was pinned, folded or the pointer was taken by something else).
    pub fn clear(&mut self) -> bool {
        self.pushing = None;
        self.shown.take().is_some()
    }

    /// Shows `e` at once (`ctl reveal`, for the headless pictures).
    pub fn show(&mut self, e: Edge) {
        self.pushing = None;
        self.shown = Some(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_bar_comes_after_300_ms_against_the_edge() {
        let mut r = Reveal::default();
        assert!(!r.motion(ms(1000), Some(Edge::Left), false));
        assert_eq!(r.due(), Some(ms(1300)));
        assert!(!r.tick(ms(1299)), "not before 300 ms");
        assert_eq!(r.shown, None);
        // Moving along the edge keeps the timer running.
        r.motion(ms(1200), Some(Edge::Left), false);
        assert_eq!(r.due(), Some(ms(1300)));
        assert!(r.tick(ms(1300)));
        assert_eq!(r.shown, Some(Edge::Left));
        assert_eq!(r.due(), None);
    }

    #[test]
    fn leaving_the_edge_early_shows_nothing() {
        let mut r = Reveal::default();
        r.motion(ms(0), Some(Edge::Right), false);
        r.motion(ms(200), None, false);
        assert!(!r.tick(ms(400)));
        assert_eq!((r.shown, r.due()), (None, None));
        // Back at the edge: the 300 ms start again.
        r.motion(ms(500), Some(Edge::Right), false);
        assert!(!r.tick(ms(700)));
        assert!(r.tick(ms(800)));
        assert_eq!(r.shown, Some(Edge::Right));
    }

    #[test]
    fn the_bar_folds_again_when_the_pointer_leaves_it() {
        let mut r = Reveal::default();
        r.show(Edge::Left);
        assert!(!r.motion(ms(0), Some(Edge::Left), true), "still at the edge");
        assert!(!r.motion(ms(10), None, true), "over the bar");
        assert!(r.motion(ms(20), None, false), "left it");
        assert_eq!(r.shown, None);
        // The other edge while one is shown: this one folds, the other starts its timer.
        r.show(Edge::Left);
        assert!(r.motion(ms(30), Some(Edge::Right), false));
        assert_eq!(r.due(), Some(ms(330)));
        assert!(r.tick(ms(330)));
        assert_eq!(r.shown, Some(Edge::Right));
    }

    #[test]
    fn clear_ends_both() {
        let mut r = Reveal::default();
        r.motion(ms(0), Some(Edge::Left), false);
        assert!(!r.clear(), "nothing was shown");
        assert_eq!(r.due(), None);
        r.show(Edge::Right);
        assert!(r.clear());
        assert_eq!(r.shown, None);
    }
}
