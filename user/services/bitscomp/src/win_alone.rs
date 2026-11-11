//! PK-L1b (D7): the Win key pressed and released alone opens the program library, as Windows' start menu does.
//! "Alone" (desk-PK67-plan.md §3 "Keys today"): Super went down, no other key, button, wheel or pad button came
//! before it went up, and less than `ALONE_MS` passed. Any combination (Super+Up, Super+Down, later Win+Space ...)
//! keeps its own meaning. A pure state machine fed with every key press and release and every other input (the
//! times are the keyboard's milliseconds), so the timing is tested on the host.

pub const KEY_LEFTMETA: u32 = 125;
pub const KEY_RIGHTMETA: u32 = 126;
pub const ALONE_MS: u32 = 600;

#[derive(Debug, Default)]
pub struct WinAlone {
    /// The Super key that is down and when it went down.
    down: Option<(u32, u32)>,
    spoiled: bool,
}

pub fn is_super(code: u32) -> bool {
    code == KEY_LEFTMETA || code == KEY_RIGHTMETA
}

impl WinAlone {
    /// A key went down or up (evdev code); true when this release is "Win alone".
    pub fn key(&mut self, code: u32, pressed: bool, now: u32) -> bool {
        match (is_super(code), pressed, self.down) {
            (true, true, None) => {
                self.down = Some((code, now));
                self.spoiled = false;
                false
            }
            // The same Super key again (a repeat) changes nothing.
            (true, true, Some((c, _))) if c == code => false,
            (_, true, Some(_)) => {
                self.spoiled = true;
                false
            }
            (true, false, Some((c, t))) if c == code => {
                self.down = None;
                !self.spoiled && now.wrapping_sub(t) < ALONE_MS
            }
            _ => false,
        }
    }

    /// A button, a wheel step or a pad button while Super may be down.
    pub fn other(&mut self) {
        if self.down.is_some() {
            self.spoiled = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: u32 = 103;
    const DOWN: u32 = 108;
    const SHIFT: u32 = 42;

    #[test]
    fn win_alone_opens_within_600_ms_only() {
        let mut w = WinAlone::default();
        assert!(!w.key(KEY_LEFTMETA, true, 1000));
        assert!(w.key(KEY_LEFTMETA, false, 1599), "599 ms alone opens");
        assert!(!w.key(KEY_LEFTMETA, true, 2000));
        assert!(!w.key(KEY_LEFTMETA, false, 2601), "601 ms does not");
        assert!(!w.key(KEY_RIGHTMETA, true, 3000));
        assert!(!w.key(KEY_RIGHTMETA, false, 3600), "600 ms exactly is not less than 600");
        assert!(!w.key(KEY_RIGHTMETA, true, 4000));
        assert!(w.key(KEY_RIGHTMETA, false, 4010), "the right Win key too");
        // The keyboard's clock wraps after 49 days: still 10 ms.
        assert!(!w.key(KEY_LEFTMETA, true, u32::MAX - 5));
        assert!(w.key(KEY_LEFTMETA, false, 4));
    }

    #[test]
    fn combinations_keep_their_meaning() {
        let mut w = WinAlone::default();
        for other in [UP, DOWN] {
            assert!(!w.key(KEY_LEFTMETA, true, 0));
            assert!(!w.key(other, true, 50));
            assert!(!w.key(other, false, 80));
            assert!(!w.key(KEY_LEFTMETA, false, 100), "Super+{other} does not open");
        }
        // Super, then a click.
        assert!(!w.key(KEY_LEFTMETA, true, 0));
        w.other();
        assert!(!w.key(KEY_LEFTMETA, false, 100), "Super then a click does not open");
        // Super held across a pad button (or a wheel step).
        assert!(!w.key(KEY_LEFTMETA, true, 0));
        w.other();
        assert!(!w.key(KEY_LEFTMETA, false, 300), "Super across a pad button does not open");
        // Both Win keys.
        assert!(!w.key(KEY_LEFTMETA, true, 0));
        assert!(!w.key(KEY_RIGHTMETA, true, 10));
        assert!(!w.key(KEY_RIGHTMETA, false, 20));
        assert!(!w.key(KEY_LEFTMETA, false, 30));
        // A click before Super went down spoils nothing; a repeat of Super neither.
        w.other();
        assert!(!w.key(KEY_LEFTMETA, true, 0));
        assert!(!w.key(KEY_LEFTMETA, true, 30));
        assert!(w.key(KEY_LEFTMETA, false, 60));
        // Shift held before Super and let go during it: no other key came, it opens.
        assert!(!w.key(SHIFT, true, 0));
        assert!(!w.key(KEY_LEFTMETA, true, 10));
        assert!(!w.key(SHIFT, false, 20));
        assert!(w.key(KEY_LEFTMETA, false, 40));
        // A release without its press (Super went down before bitscomp started) opens nothing.
        assert!(!w.key(KEY_LEFTMETA, false, 50));
    }
}
