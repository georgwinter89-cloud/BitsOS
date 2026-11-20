//! WY-8 (GX-3 change request 2), WY-17: which of fbdev's two windows the renderer draws next and how old it is, apart
//! from the ioctls so it is tested on the host. A window drawn now was last drawn two frames ago; both are drawn whole
//! at the start and when the kernel says something else wrote into them.

#[derive(Debug)]
pub struct Flip {
    /// The window drawn next and the one on the screen (the last pan that landed; 0 at the start).
    back: usize,
    shown: usize,
    /// How many frames still draw their window whole.
    fresh: u8,
}

impl Flip {
    pub fn new() -> Flip {
        Flip { back: 1, shown: 0, fresh: 2 }
    }

    pub fn shown(&self) -> usize {
        self.shown
    }

    /// The window the frame being drawn goes to (`aim`).
    pub fn back(&self) -> usize {
        self.back
    }

    /// The kernel named the window: it may not be the one this count would pick, the engine alternates its buffers for
    /// every frame it ever flipped. A window comes back two frames old; whole while `fresh` (at the start) and when
    /// something else wrote into the buffers (`whole`).
    pub fn aim(&mut self, window: usize, whole: bool) -> (usize, usize) {
        self.back = window;
        self.shown = window ^ 1;
        let fresh = self.fresh > 0;
        self.fresh = self.fresh.saturating_sub(1);
        (window, if fresh || whole { 0 } else { 2 })
    }

    /// The pan to the window just drawn landed: it is on the screen, the other one is drawn next.
    pub fn landed(&mut self) {
        self.shown = self.back;
        self.back ^= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_the_renderer_draws_itself_is_two_frames_old() {
        let mut f = Flip::new();
        assert_eq!(f.aim(1, false), (1, 0), "both whole at the start");
        f.landed();
        assert_eq!(f.aim(0, false), (0, 0));
        f.landed();
        assert_eq!(f.aim(1, false), (1, 2));
        assert_eq!(f.shown(), 0);
        f.landed();
        assert_eq!(f.aim(0, true), (0, 0), "the kernel says something else wrote there");
        f.landed();
        assert_eq!(f.aim(1, false), (1, 2));
        assert_eq!(f.back(), 1);
        assert_eq!(f.aim(1, false), (1, 2), "nothing drawn, nothing flipped: the same window, as old as it was");
    }

    #[test]
    fn the_kernel_names_the_window_not_the_count() {
        let mut f = Flip::new();
        // The engine showed its second buffer when the renderer started: the first frame goes to the first.
        assert_eq!(f.aim(0, false), (0, 0));
        assert_eq!(f.shown(), 1);
    }
}
