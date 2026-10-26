//! WY-8 (GX-3 change request 2): which of fbdev's two windows is drawn next and with which rectangles, apart from
//! the ioctls so it is tested on the host. A window drawn now was last drawn two frames ago, so it gets last
//! frame's damage too; both windows are copied whole at the start and after a refused pan. A pan refused twice in
//! a row (Linux fb_pan_display's EINVAL on a bad offset or step, fbmem.c:706; a mode switch) ends the two windows:
//! from then on one window, the one on the screen, as without `--fb-pan`.

use smithay::utils::{Physical, Rectangle};

type Rect = Rectangle<i32, Physical>;

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Both windows are copied whole next; `first`: the first refusal since the start (logged once).
    Retry { first: bool },
    /// The second in a row: one window from now on.
    OneWindow,
}

#[derive(Debug)]
pub struct Flip {
    halves: bool,
    /// The window drawn next and the one on the screen (the last pan that landed; 0 at the start).
    back: usize,
    shown: usize,
    /// How many frames still copy their window whole.
    fresh: u8,
    prev: Vec<Rect>,
    in_a_row: u32,
    ever: bool,
}

impl Flip {
    pub fn new(halves: bool) -> Flip {
        Flip { halves, back: halves as usize, shown: 0, fresh: if halves { 2 } else { 0 }, prev: Vec::new(), in_a_row: 0, ever: false }
    }

    pub fn halves(&self) -> bool {
        self.halves
    }

    pub fn shown(&self) -> usize {
        self.shown
    }

    /// The window to draw into now and the rectangles to copy there (`full` is the whole screen).
    pub fn frame(&mut self, damage: &[Rect], full: Rect) -> (usize, Vec<Rect>) {
        let mut rects = std::mem::take(&mut self.prev);
        rects.extend_from_slice(damage);
        if self.fresh > 0 {
            self.fresh -= 1;
            rects.push(full);
        }
        (if self.halves { self.back } else { self.shown }, rects)
    }

    /// The pan to the window just drawn landed: it is on the screen, the other one is drawn next.
    pub fn landed(&mut self, damage: &[Rect]) {
        self.prev = damage.to_vec();
        self.shown = self.back;
        self.back ^= 1;
        self.in_a_row = 0;
    }

    /// The pan was refused: the screen still shows `shown`.
    pub fn refused(&mut self) -> Refused {
        self.in_a_row += 1;
        self.prev.clear();
        let first = !self.ever;
        self.ever = true;
        if self.in_a_row == 1 {
            self.fresh = 2;
            return Refused::Retry { first };
        }
        self.halves = false;
        self.fresh = 1;
        Refused::OneWindow
    }

    /// After the fall back to one window the screen was panned to window `w` (the first, so the console finds
    /// its picture where it draws); that one is drawn whole next.
    pub fn moved_to(&mut self, w: usize) {
        self.shown = w;
        self.fresh = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32) -> Rect {
        Rectangle::new((x, 0).into(), (1, 1).into())
    }
    fn full() -> Rect {
        Rectangle::from_size((100, 50).into())
    }

    #[test]
    fn both_windows_whole_first_then_two_frames_of_damage() {
        let mut f = Flip::new(true);
        assert_eq!(f.frame(&[r(1)], full()), (1, vec![r(1), full()]), "first frame: window 1, whole");
        f.landed(&[r(1)]);
        assert_eq!(f.shown(), 1);
        assert_eq!(f.frame(&[r(2)], full()), (0, vec![r(1), r(2), full()]), "window 0 is whole once too");
        f.landed(&[r(2)]);
        assert_eq!(f.frame(&[r(3)], full()), (1, vec![r(2), r(3)]), "then the last two frames' damage");
        f.landed(&[r(3)]);
        assert_eq!(f.frame(&[], full()), (0, vec![r(3)]), "an empty frame still catches up the window");
    }

    #[test]
    fn a_refused_pan_copies_both_whole_and_a_second_ends_the_two_windows() {
        let mut f = Flip::new(true);
        f.frame(&[], full());
        f.landed(&[]);
        f.frame(&[], full());
        f.landed(&[]);
        assert_eq!(f.frame(&[r(1)], full()), (1, vec![r(1)]));
        assert_eq!(f.refused(), Refused::Retry { first: true });
        assert_eq!(f.shown(), 0, "the screen still shows window 0");
        assert_eq!(f.frame(&[r(2)], full()), (1, vec![r(2), full()]), "the same window again, whole");
        f.landed(&[r(2)]);
        assert_eq!(f.frame(&[], full()), (0, vec![r(2), full()]), "and the other one whole too");
        assert_eq!(f.refused(), Refused::Retry { first: false }, "a later single refusal is not logged again");
        f.frame(&[], full());
        assert_eq!(f.refused(), Refused::OneWindow);
        assert!(!f.halves());
        assert_eq!(f.frame(&[r(4)], full()), (1, vec![r(4), full()]), "one window: the shown one, whole once");
        assert_eq!(f.frame(&[r(5)], full()), (1, vec![r(5)]), "then only the damage");
        f.moved_to(0);
        assert_eq!(f.frame(&[r(6)], full()), (0, vec![r(6), full()]));
    }

    #[test]
    fn one_window_from_the_start_copies_only_the_damage() {
        let mut f = Flip::new(false);
        assert_eq!(f.frame(&[r(1)], full()), (0, vec![r(1)]));
        assert_eq!(f.frame(&[r(2)], full()), (0, vec![r(2)]));
    }
}
