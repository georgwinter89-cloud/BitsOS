//! MC-9b: the two dialogs of the installer overlay as plain models (keys and clicks in, outcomes out), kept apart
//! from drawing so the rules can be tested: the installer's question ("Erlauben"/"Ablehnen", MC-9a's `ask`) and
//! the grant dialog ("Claude darf ... bis ...", Strg+Alt+G). The rules (DECISIONS 24.10., V2-LANES MC-9):
//! - only real keyboard/mouse input (`Origin::Physical`) can say yes; injected input never moves the focus or
//!   scrolls, and when it presses a button the answer goes out marked `injected` (the installer refuses it);
//! - focus starts on "Ablehnen", Esc and the end of the dialog time are no;
//! - "Erlauben" is armed only `ARM` after the dialog appeared (a click meant for what was there before never
//!   confirms) and only once the whole text was on screen.

use std::time::Duration;

use smithay::input::keyboard::Keysym;

use crate::input::Origin;

/// How long "Erlauben" stays unarmed after a dialog appears.
pub const ARM: Duration = Duration::from_millis(1000);

/// What an input did to a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to send; `true` when the look changed.
    Stay(bool),
    /// Answer the question: yes or no, and how it was given.
    Answer { yes: bool, input: Origin },
    /// Input that does not count (injected) and was dropped: logged by the caller.
    Ignored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    No,
    Yes,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AskDialog {
    pub id: u64,
    pub kind: String,
    pub title: String,
    pub text: String,
    /// When it appeared and when it ends by itself (pacer clock).
    pub opened: Duration,
    pub deadline: Duration,
    pub on_yes: bool,
    /// The first text line shown, and the most lines that fit (set by the layout).
    pub scroll: usize,
    pub lines: usize,
    pub visible: usize,
    pub seen_end: bool,
    pub hover: Option<Btn>,
}

impl AskDialog {
    pub fn new(id: u64, kind: String, title: String, text: String, now: Duration, timeout: Duration) -> AskDialog {
        AskDialog { id, kind, title, text, opened: now, deadline: now + timeout, on_yes: false, scroll: 0, lines: 0, visible: 0, seen_end: false, hover: None }
    }

    /// The layout's result: `lines` wrapped lines of which `visible` fit.
    pub fn fit(&mut self, lines: usize, visible: usize) {
        self.lines = lines;
        self.visible = visible.max(1);
        self.scroll = self.scroll.min(lines.saturating_sub(self.visible));
        self.seen_end |= self.scroll + self.visible >= lines;
    }

    pub fn armed(&self, now: Duration) -> bool {
        self.seen_end && now >= self.opened + ARM
    }

    pub fn secs_left(&self, now: Duration) -> u64 {
        self.deadline.saturating_sub(now).as_millis().div_ceil(1000) as u64
    }

    fn scroll_by(&mut self, d: i64) -> bool {
        let max = self.lines.saturating_sub(self.visible) as i64;
        let to = (self.scroll as i64 + d).clamp(0, max) as usize;
        let moved = to != self.scroll;
        self.scroll = to;
        self.seen_end |= self.scroll + self.visible >= self.lines;
        moved
    }

    fn press(&mut self, b: Btn, input: Origin, now: Duration) -> Outcome {
        match (b, input) {
            (Btn::No, _) => Outcome::Answer { yes: false, input },
            (Btn::Yes, Origin::Injected) => Outcome::Answer { yes: true, input },
            (Btn::Yes, Origin::Physical) if self.armed(now) => Outcome::Answer { yes: true, input },
            (Btn::Yes, Origin::Physical) => Outcome::Stay(false),
        }
    }

    pub fn key(&mut self, sym: Keysym, input: Origin, now: Duration) -> Outcome {
        let focused = if self.on_yes { Btn::Yes } else { Btn::No };
        match (sym, input) {
            (Keysym::Escape, _) => Outcome::Answer { yes: false, input },
            (Keysym::Return | Keysym::KP_Enter | Keysym::space, _) => self.press(focused, input, now),
            (_, Origin::Injected) => Outcome::Ignored,
            (Keysym::Tab | Keysym::ISO_Left_Tab | Keysym::Left | Keysym::Right | Keysym::KP_Left | Keysym::KP_Right, _) => {
                self.on_yes = !self.on_yes;
                Outcome::Stay(true)
            }
            (Keysym::Down | Keysym::KP_Down, _) => Outcome::Stay(self.scroll_by(1)),
            (Keysym::Up | Keysym::KP_Up, _) => Outcome::Stay(self.scroll_by(-1)),
            (Keysym::Next | Keysym::KP_Next, _) => Outcome::Stay(self.scroll_by(self.visible.max(2) as i64 - 1)),
            (Keysym::Prior | Keysym::KP_Prior, _) => Outcome::Stay(self.scroll_by(1 - self.visible.max(2) as i64)),
            _ => Outcome::Stay(false),
        }
    }

    /// A press of the left button on `hit` (None: beside the buttons).
    pub fn click(&mut self, hit: Option<Btn>, input: Origin, now: Duration) -> Outcome {
        match hit {
            Some(b) => self.press(b, input, now),
            None if input == Origin::Injected => Outcome::Ignored,
            None => Outcome::Stay(false),
        }
    }

    pub fn wheel(&mut self, steps: f64, input: Origin) -> Outcome {
        if input == Origin::Injected {
            return Outcome::Ignored;
        }
        Outcome::Stay(self.scroll_by((steps * 3.0).round() as i64))
    }
}

/// The kinds a grant can be for (mcp-wire installer `Kind::grantable`: not nvme, not other): wire name, what
/// Claude may then do, the short name in the bar.
pub const KINDS: [(&str, &str, &str); 5] = [
    ("install", "Kernel und Generationen installieren", "installieren"),
    ("pin", "die startende Generation festlegen", "festlegen"),
    ("reload", "den Kernel neu laden (Reload)", "Reload"),
    ("reboot", "den Rechner neu starten", "Neustart"),
    ("settings", "geschützte Einstellungen ändern", "Einstellungen"),
];

pub fn kind_short(kind: &str) -> &str {
    KINDS.iter().find(|k| k.0 == kind).map_or(kind, |k| k.2)
}

/// How long a grant runs: seconds from now, or until the next full hour `h` o'clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dur {
    Secs(i64),
    Until(i32),
}

pub const DURS: [Dur; 6] = [Dur::Secs(1800), Dur::Secs(3600), Dur::Secs(7200), Dur::Secs(4 * 3600), Dur::Secs(8 * 3600), Dur::Until(8)];

pub fn dur_label(d: Dur) -> String {
    match d {
        Dur::Secs(s) if s % 3600 == 0 && s == 3600 => "1 Stunde".into(),
        Dur::Secs(s) if s % 3600 == 0 => format!("{} Stunden", s / 3600),
        Dur::Secs(s) if s % 60 == 0 => format!("{} Minuten", s / 60),
        Dur::Secs(s) => format!("{s} Sekunden"),
        Dur::Until(h) => format!("bis {h} Uhr"),
    }
}

/// The end of `d` from `now` (Unix s), whose local time of day is `since_midnight` seconds; at most 24 h ahead.
pub fn until_of(d: Dur, now: i64, since_midnight: i64) -> i64 {
    match d {
        Dur::Secs(s) => now.saturating_add(s.clamp(1, 86_400)),
        Dur::Until(h) => {
            let mut left = h as i64 * 3600 - since_midnight.clamp(0, 86_399);
            if left <= 0 {
                left += 86_400;
            }
            now.saturating_add(left)
        }
    }
}

/// The grant dialog's focus stops, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Kind,
    Until,
    Cancel,
    Revoke,
    Ok,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantStep {
    Stay(bool),
    Close,
    Grant,
    RevokeAll,
    Ignored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GrantDialog {
    pub kind: usize,
    pub dur: usize,
    /// The duration choices (a headless test adds a few seconds in front: `ctl install test-seconds N`).
    pub durs: Vec<Dur>,
    pub focus: Stop,
    /// Grants run now: "Alle zurücknehmen" is offered.
    pub revoke: bool,
    pub hover: Option<Stop>,
}

impl GrantDialog {
    pub fn new(durs: Vec<Dur>, revoke: bool) -> GrantDialog {
        let dur = durs.iter().position(|d| *d == Dur::Secs(3600)).unwrap_or(0);
        GrantDialog { kind: 0, dur, durs, focus: Stop::Kind, revoke, hover: None }
    }

    pub fn stops(&self) -> Vec<Stop> {
        let mut v = vec![Stop::Kind, Stop::Until, Stop::Cancel];
        if self.revoke {
            v.push(Stop::Revoke);
        }
        v.push(Stop::Ok);
        v
    }

    fn step_focus(&mut self, d: i32) {
        let s = self.stops();
        let i = s.iter().position(|x| *x == self.focus).unwrap_or(0) as i32;
        self.focus = s[(i + d).rem_euclid(s.len() as i32) as usize];
    }

    fn change(&mut self, stop: Stop, d: i32) -> bool {
        let (v, n) = match stop {
            Stop::Kind => (&mut self.kind, KINDS.len()),
            Stop::Until => (&mut self.dur, self.durs.len()),
            _ => return false,
        };
        *v = (*v as i32 + d).rem_euclid(n.max(1) as i32) as usize;
        true
    }

    fn activate(&mut self, stop: Stop) -> GrantStep {
        match stop {
            Stop::Kind | Stop::Until => {
                self.step_focus(1);
                GrantStep::Stay(true)
            }
            Stop::Cancel => GrantStep::Close,
            Stop::Revoke => GrantStep::RevokeAll,
            Stop::Ok => GrantStep::Grant,
        }
    }

    /// Only the real keyboard works this dialog: injected keys are dropped, Esc of them too.
    pub fn key(&mut self, sym: Keysym, input: Origin) -> GrantStep {
        if input == Origin::Injected {
            return GrantStep::Ignored;
        }
        let on_row = matches!(self.focus, Stop::Kind | Stop::Until);
        match sym {
            Keysym::Escape => GrantStep::Close,
            Keysym::Return | Keysym::KP_Enter | Keysym::space => self.activate(self.focus),
            Keysym::Tab | Keysym::Down | Keysym::KP_Down => {
                self.step_focus(1);
                GrantStep::Stay(true)
            }
            Keysym::ISO_Left_Tab | Keysym::Up | Keysym::KP_Up => {
                self.step_focus(-1);
                GrantStep::Stay(true)
            }
            Keysym::Left | Keysym::KP_Left if on_row => GrantStep::Stay(self.change(self.focus, -1)),
            Keysym::Right | Keysym::KP_Right if on_row => GrantStep::Stay(self.change(self.focus, 1)),
            Keysym::Left | Keysym::KP_Left if self.focus != Stop::Cancel => {
                self.step_focus(-1);
                GrantStep::Stay(true)
            }
            Keysym::Right | Keysym::KP_Right if self.focus != Stop::Ok => {
                self.step_focus(1);
                GrantStep::Stay(true)
            }
            _ => GrantStep::Stay(false),
        }
    }

    /// A press on `hit` (a value row cycles its value; a button acts).
    pub fn click(&mut self, hit: Option<Stop>, input: Origin) -> GrantStep {
        if input == Origin::Injected {
            return GrantStep::Ignored;
        }
        let Some(h) = hit else { return GrantStep::Stay(false) };
        self.focus = h;
        match h {
            Stop::Kind | Stop::Until => GrantStep::Stay(self.change(h, 1)),
            b => self.activate(b),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        KINDS[self.kind.min(KINDS.len() - 1)].0
    }

    /// "Claude darf bis 08:00 Uhr ohne Rückfrage den Rechner neu starten."
    pub fn sentence(&self, until_hm: &str) -> String {
        format!("Claude darf bis {until_hm} Uhr ohne Rückfrage {}.", KINDS[self.kind.min(KINDS.len() - 1)].1)
    }
}

/// The bar's marker for the grants running: "Claude darf: installieren bis 08:00 · Reload bis 07:30".
pub fn marker(grants: &[(String, String)]) -> Option<String> {
    if grants.is_empty() {
        return None;
    }
    let parts: Vec<String> = grants.iter().map(|(k, hm)| format!("{} bis {hm}", kind_short(k))).collect();
    Some(format!("Claude darf: {}", parts.join(" · ")))
}

/// `text` in lines of at most `max` (the font's units): paragraphs at '\n', words that do not fit broken by
/// characters (a long argument has no spaces).
pub fn wrap(text: &str, max: i32, width: impl Fn(&str) -> i32) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let joined = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if width(&joined) <= max {
                line = joined;
                continue;
            }
            if width(word) <= max {
                out.push(std::mem::replace(&mut line, word.to_string()));
                continue;
            }
            // Longer than a whole line: broken by characters, starting right after what the line holds.
            if !line.is_empty() {
                line.push(' ');
            }
            for c in word.chars() {
                line.push(c);
                if width(&line) > max && line.chars().count() > 1 {
                    line.pop();
                    out.push(std::mem::replace(&mut line, c.to_string()));
                }
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::from_secs(100);

    fn ask() -> AskDialog {
        let mut d = AskDialog::new(7, "reboot".into(), "Rechner neu starten".into(), "x".into(), T0, Duration::from_secs(60));
        d.fit(3, 10);
        d
    }

    #[test]
    fn only_a_real_armed_yes_confirms() {
        let late = T0 + ARM;
        let mut d = ask();
        assert!(!d.on_yes, "focus starts on Ablehnen");
        assert_eq!(d.clone().key(Keysym::Return, Origin::Physical, late), Outcome::Answer { yes: false, input: Origin::Physical });
        assert_eq!(d.clone().key(Keysym::Escape, Origin::Physical, late), Outcome::Answer { yes: false, input: Origin::Physical });
        // Injected keys neither move the focus nor scroll.
        assert_eq!(d.key(Keysym::Tab, Origin::Injected, late), Outcome::Ignored);
        assert!(!d.on_yes);
        assert_eq!(d.key(Keysym::Tab, Origin::Physical, late), Outcome::Stay(true));
        assert!(d.on_yes);
        // An injected Enter on Erlauben goes out marked: the installer refuses it.
        assert_eq!(d.clone().key(Keysym::Return, Origin::Injected, late), Outcome::Answer { yes: true, input: Origin::Injected });
        // Not armed yet: a real Enter right after the dialog appeared does nothing.
        assert_eq!(d.clone().key(Keysym::Return, Origin::Physical, T0 + ARM / 2), Outcome::Stay(false));
        assert_eq!(d.clone().key(Keysym::space, Origin::Physical, late), Outcome::Answer { yes: true, input: Origin::Physical });
        assert_eq!(d.clone().key(Keysym::Escape, Origin::Physical, late), Outcome::Answer { yes: false, input: Origin::Physical });
        assert_eq!(d.key(Keysym::a, Origin::Physical, late), Outcome::Stay(false));
        // The mouse: injected clicks on Erlauben are marked, a real one before the arm time does nothing.
        assert_eq!(d.clone().click(Some(Btn::Yes), Origin::Injected, late), Outcome::Answer { yes: true, input: Origin::Injected });
        assert_eq!(d.clone().click(Some(Btn::Yes), Origin::Physical, T0), Outcome::Stay(false));
        assert_eq!(d.clone().click(Some(Btn::Yes), Origin::Physical, late), Outcome::Answer { yes: true, input: Origin::Physical });
        assert_eq!(d.clone().click(Some(Btn::No), Origin::Physical, T0), Outcome::Answer { yes: false, input: Origin::Physical });
        assert_eq!(d.clone().click(None, Origin::Injected, late), Outcome::Ignored);
        assert_eq!(d.secs_left(T0), 60);
        assert_eq!(d.secs_left(T0 + Duration::from_millis(59_001)), 1);
        assert_eq!(d.secs_left(T0 + Duration::from_secs(61)), 0);
    }

    #[test]
    fn a_long_text_must_be_read_to_its_end_before_yes() {
        let late = T0 + ARM * 3;
        let mut d = AskDialog::new(1, "install".into(), "t".into(), "x".into(), T0, Duration::from_secs(60));
        d.fit(30, 10);
        d.on_yes = true;
        assert!(!d.armed(late));
        assert_eq!(d.clone().key(Keysym::Return, Origin::Physical, late), Outcome::Stay(false));
        assert_eq!(d.wheel(10.0, Origin::Injected), Outcome::Ignored, "an injected wheel does not scroll");
        assert_eq!(d.key(Keysym::Next, Origin::Injected, late), Outcome::Ignored);
        assert_eq!(d.key(Keysym::Next, Origin::Physical, late), Outcome::Stay(true));
        assert_eq!(d.scroll, 9);
        assert!(!d.armed(late));
        assert_eq!(d.wheel(5.0, Origin::Physical), Outcome::Stay(true));
        assert_eq!(d.scroll, 20, "clamped to the last page");
        assert!(d.armed(late));
        assert_eq!(d.key(Keysym::Up, Origin::Physical, late), Outcome::Stay(true));
        assert!(d.armed(late), "once seen stays seen");
        // A new layout with fewer lines keeps the scroll in range.
        d.fit(5, 10);
        assert_eq!(d.scroll, 0);
    }

    #[test]
    fn the_grant_dialog_takes_only_real_keys_and_enter_never_grants_by_itself() {
        let mut g = GrantDialog::new(DURS.to_vec(), false);
        assert_eq!((g.focus, g.kind_name(), g.durs[g.dur]), (Stop::Kind, "install", Dur::Secs(3600)));
        assert_eq!(g.key(Keysym::Escape, Origin::Injected), GrantStep::Ignored);
        assert_eq!(g.key(Keysym::Return, Origin::Injected), GrantStep::Ignored);
        assert_eq!(g.click(Some(Stop::Ok), Origin::Injected), GrantStep::Ignored);
        // Enter walks down the rows and stops on Abbrechen.
        for _ in 0..5 {
            if g.key(Keysym::Return, Origin::Physical) != GrantStep::Stay(true) {
                break;
            }
        }
        assert_eq!(g.focus, Stop::Cancel);
        assert_eq!(g.clone().key(Keysym::Return, Origin::Physical), GrantStep::Close);
        assert_eq!(g.key(Keysym::Right, Origin::Physical), GrantStep::Stay(true));
        assert_eq!(g.focus, Stop::Ok);
        assert_eq!(g.clone().key(Keysym::Return, Origin::Physical), GrantStep::Grant);
        // Values: Left/Right on a row, a click cycles.
        g.focus = Stop::Kind;
        assert_eq!(g.key(Keysym::Left, Origin::Physical), GrantStep::Stay(true));
        assert_eq!(g.kind_name(), "settings");
        assert_eq!(g.click(Some(Stop::Until), Origin::Physical), GrantStep::Stay(true));
        assert_eq!(g.durs[g.dur], Dur::Secs(7200));
        assert_eq!(g.sentence("08:00"), "Claude darf bis 08:00 Uhr ohne Rückfrage geschützte Einstellungen ändern.");
        let mut r = GrantDialog::new(DURS.to_vec(), true);
        assert!(r.stops().contains(&Stop::Revoke));
        assert_eq!(r.click(Some(Stop::Revoke), Origin::Physical), GrantStep::RevokeAll);
        assert_eq!(r.key(Keysym::Escape, Origin::Physical), GrantStep::Close);
    }

    #[test]
    fn grant_ends_are_at_most_a_day_ahead() {
        let now = 1_800_000_000;
        assert_eq!(until_of(Dur::Secs(3600), now, 0), now + 3600);
        assert_eq!(until_of(Dur::Secs(i64::MAX), now, 0), now + 86_400);
        // 22:30 local: 8 o'clock is 9.5 h ahead; 07:59:30 local: 30 s; 08:00:00 local: tomorrow's.
        assert_eq!(until_of(Dur::Until(8), now, 22 * 3600 + 1800), now + 9 * 3600 + 1800);
        assert_eq!(until_of(Dur::Until(8), now, 8 * 3600 - 30), now + 30);
        assert_eq!(until_of(Dur::Until(8), now, 8 * 3600), now + 86_400);
        assert_eq!(until_of(Dur::Until(8), i64::MAX, 0), i64::MAX);
        let labels: Vec<String> = DURS.iter().map(|d| dur_label(*d)).collect();
        assert_eq!(labels, ["30 Minuten", "1 Stunde", "2 Stunden", "4 Stunden", "8 Stunden", "bis 8 Uhr"]);
        assert_eq!(dur_label(Dur::Secs(3)), "3 Sekunden");
    }

    #[test]
    fn texts_have_glyphs_and_wrap_inside_the_box() {
        let g = GrantDialog::new(DURS.to_vec(), true);
        let mut all: Vec<String> = KINDS.iter().flat_map(|k| [k.1.to_string(), k.2.to_string()]).collect();
        all.extend(DURS.iter().map(|d| dur_label(*d)));
        all.push(g.sentence("08:00"));
        all.push(marker(&[("install".into(), "08:00".into()), ("reload".into(), "07:30".into())]).unwrap());
        all.extend(crate::install_draw::TEXTS.iter().map(|s| s.to_string()));
        for t in &all {
            assert!(t.chars().all(crate::text::Font::has), "{t}");
        }
        assert_eq!(marker(&[]), None);
        assert_eq!(marker(&[("install".into(), "08:00".into())]).as_deref(), Some("Claude darf: installieren bis 08:00"));
        // One unit per character: lines of at most 10, a word longer than a line is broken.
        let w = |s: &str| s.chars().count() as i32;
        let lines = wrap("eins zwei drei\n\nabcdefghijklmnopqrstuvwxyz ende", 10, w);
        assert_eq!(lines, ["eins zwei", "drei", "", "abcdefghij", "klmnopqrst", "uvwxyz", "ende"]);
        assert!(lines.iter().all(|l| w(l) <= 10));
        assert_eq!(wrap("", 10, w), [""]);
        assert_eq!(wrap("url: aaaaaaaaaaaaaaa", 10, w), ["url: aaaaa", "aaaaaaaaaa"]);
        // Narrower than one character: one per line, no endless loop.
        assert_eq!(wrap("abc", 0, w), ["a", "b", "c"]);
    }
}
