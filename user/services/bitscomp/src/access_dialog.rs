//! MC-10: the full-access dialog in the installer overlay (same layer, same frame and the same input rules as
//! MC-9b's question): "Vollzugriff für den Gaming-PC (<name>, <fingerprint short>) bis <time>" with the choices
//! 1 h, 8 h, 24 h (preselected) and 7 Tage, the box "auch interne Platte (NVMe) beschreiben" (off), Ablehnen and
//! Erlauben, and "Fernzugriff beenden" while a grant runs. It opens for a client's `request_access` (the installer's
//! `ask` of kind `access`) and with Strg+Alt+V at the real keyboard (Georg's own). Only real input changes or
//! grants anything; Erlauben is armed 2 s after the dialog appears and after every earlier real key (`ACCESS_ARM`), the focus starts on it so
//! Georg answers the desk's request with Enter. An injected press on a button is sent as injected (refused by
//! the installer); injected navigation is dropped.

use std::time::Duration;

use smithay::input::keyboard::Keysym;

use crate::canvas::Canvas;
use crate::chrome_draw::font;
use crate::input::Origin;
/// Erlauben is armed this long after the dialog appeared, and again this long after every real key pressed
/// before that: Enter typed into another window as the dialog comes up (a request the client timed for it) never
/// grants; Georg walking up and pressing Enter does.
pub const ACCESS_ARM: Duration = Duration::from_millis(2000);
use crate::install_draw::{BTN, LINE_H, NOTE_H, PAD, ROW_H, TITLE_H, button, centred, frame_canvas};
use crate::layout::Rect;
use crate::text::Weight;
use crate::theme::Theme;

/// The choices in hours and their labels; `DEFAULT` is preselected.
pub const CHOICES: [(i64, &str); 4] = [(1, "1 h"), (8, "8 h"), (24, "24 h"), (168, "7 Tage")];
pub const DEFAULT: usize = 2;
pub const NVME_TEXT: &str = "auch interne Platte (NVMe) beschreiben";
pub const OWN_TEXT: &str = "Du öffnest den Vollzugriff selbst. Bis zur gewählten Zeit darf der Gaming-PC ohne Rückfrage alles, was der Kernel kann (Programme als root, Dateien, Bildschirm, Tasten, Installieren, Neustart). Beenden: Strg+Alt+V.";
const NOTE: &str = "Nur die echte Tastatur und Maus zählen. Esc heißt Ablehnen.";
/// The fixed texts (the glyph test checks every character).
#[cfg(test)]
pub const TEXTS: [&str; 9] = [NVME_TEXT, OWN_TEXT, NOTE, "Vollzugriff für den Gaming-PC", "Fernzugriff beenden", "Fernzugriff offen bis", "7 Tage", "Ablehnen", "Erlauben"];
/// Text lines the dialog shows at most (the installer's text is a few sentences).
const TEXT_LINES: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AStop {
    Choice(usize),
    Nvme,
    Revoke,
    No,
    Yes,
}

/// What an input did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AStep {
    Stay(bool),
    Grant { hours: i64, nvme: bool, input: Origin },
    No { input: Origin },
    Revoke,
    Ignored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccessDialog {
    /// The installer's question it answers (None: Georg opened it himself).
    pub id: Option<u64>,
    pub client: String,
    pub name: String,
    /// What the client asked for (shown in the text).
    pub hours: i64,
    pub text: String,
    pub choice: usize,
    pub nvme: bool,
    pub focus: AStop,
    /// A grant runs: "Fernzugriff beenden" is offered.
    pub revoke: bool,
    pub opened: Duration,
    /// When a question ends by itself (None: Georg's own dialog stays until he closes it).
    pub deadline: Option<Duration>,
    pub hover: Option<AStop>,
}

impl AccessDialog {
    pub fn new(id: Option<u64>, client: String, name: String, hours: i64, text: String, revoke: bool, now: Duration, timeout: Option<Duration>) -> AccessDialog {
        AccessDialog { id, client, name, hours, text, choice: DEFAULT, nvme: false, focus: AStop::Yes, revoke, opened: now, deadline: timeout.map(|t| now + t), hover: None }
    }

    pub fn armed(&self, now: Duration) -> bool {
        now >= self.opened + ACCESS_ARM
    }

    pub fn hours_chosen(&self) -> i64 {
        CHOICES[self.choice.min(CHOICES.len() - 1)].0
    }

    pub fn stops(&self) -> Vec<AStop> {
        let mut v: Vec<AStop> = (0..CHOICES.len()).map(AStop::Choice).collect();
        v.push(AStop::Nvme);
        if self.revoke {
            v.push(AStop::Revoke);
        }
        v.push(AStop::No);
        v.push(AStop::Yes);
        v
    }

    fn step_focus(&mut self, d: i32) {
        let s = self.stops();
        let i = s.iter().position(|x| *x == self.focus).unwrap_or(0) as i32;
        self.focus = s[(i + d).rem_euclid(s.len() as i32) as usize];
    }

    fn press(&mut self, stop: AStop, input: Origin, now: Duration) -> AStep {
        match (stop, input) {
            (AStop::No, _) => AStep::No { input },
            (AStop::Yes, Origin::Injected) => AStep::Grant { hours: self.hours_chosen(), nvme: self.nvme, input },
            (AStop::Yes, Origin::Physical) if self.armed(now) => AStep::Grant { hours: self.hours_chosen(), nvme: self.nvme, input },
            (AStop::Yes, Origin::Physical) => AStep::Stay(false),
            (_, Origin::Injected) => AStep::Ignored,
            (AStop::Revoke, _) => AStep::Revoke,
            (AStop::Nvme, _) => {
                self.nvme = !self.nvme;
                AStep::Stay(true)
            }
            (AStop::Choice(i), _) => {
                self.choice = i.min(CHOICES.len() - 1);
                self.focus = AStop::Yes;
                AStep::Stay(true)
            }
        }
    }

    pub fn key(&mut self, sym: Keysym, input: Origin, now: Duration) -> AStep {
        let early = input == Origin::Physical && !self.armed(now);
        let step = self.key_step(sym, input, now);
        if early {
            // Typing was going on: the arming starts again from this key.
            self.opened = now;
        }
        step
    }

    fn key_step(&mut self, sym: Keysym, input: Origin, now: Duration) -> AStep {
        match (sym, input) {
            (Keysym::Escape, _) => AStep::No { input },
            (Keysym::Return | Keysym::KP_Enter, _) => self.press(self.focus, input, now),
            (Keysym::space, _) => self.press(self.focus, input, now),
            (_, Origin::Injected) => AStep::Ignored,
            (Keysym::Tab | Keysym::Down | Keysym::KP_Down, _) => {
                self.step_focus(1);
                AStep::Stay(true)
            }
            (Keysym::ISO_Left_Tab | Keysym::Up | Keysym::KP_Up, _) => {
                self.step_focus(-1);
                AStep::Stay(true)
            }
            (Keysym::Left | Keysym::KP_Left, _) => {
                if let AStop::Choice(i) = self.focus {
                    let i = i.saturating_sub(1);
                    self.focus = AStop::Choice(i);
                    self.choice = i;
                } else {
                    self.step_focus(-1);
                }
                AStep::Stay(true)
            }
            (Keysym::Right | Keysym::KP_Right, _) => {
                if let AStop::Choice(i) = self.focus {
                    let i = (i + 1).min(CHOICES.len() - 1);
                    self.focus = AStop::Choice(i);
                    self.choice = i;
                } else {
                    self.step_focus(1);
                }
                AStep::Stay(true)
            }
            _ => AStep::Stay(false),
        }
    }

    /// A left press on `hit` (None: beside every part).
    pub fn click(&mut self, hit: Option<AStop>, input: Origin, now: Duration) -> AStep {
        match hit {
            Some(s) => {
                if input == Origin::Physical {
                    self.focus = s;
                }
                self.press(s, input, now)
            }
            None if input == Origin::Injected => AStep::Ignored,
            None => AStep::Stay(false),
        }
    }
}

/// The dialog's title for the chosen end ("bis 14:00", "bis 03.11. 14:00").
pub fn title(name: &str, fp_short: &str, until: &str) -> String {
    format!("Vollzugriff für den Gaming-PC ({name}, {fp_short}) bis {until}")
}

/// The status bar's chip while a grant runs.
pub fn chip(until: &str) -> String {
    format!("Fernzugriff offen bis {until}")
}

fn width(full: Rect) -> i32 {
    (full.w - 96).clamp(1, 760)
}

/// The frame for `lines` text lines.
fn frame(full: Rect, lines: usize) -> Rect {
    let lines = lines.clamp(1, TEXT_LINES) as i32;
    let h = 2 * PAD + TITLE_H + 12 + lines * LINE_H + 16 + ROW_H + 12 + ROW_H + 12 + NOTE_H + 16 + BTN.1;
    centred(full, width(full), h)
}

/// The dialog's parts: the four choices, the NVMe box, then its buttons from left to right.
pub fn parts(full: Rect, d: &AccessDialog, lines: usize) -> Vec<(AStop, Rect)> {
    let f = frame(full, lines);
    let inner = f.w - 2 * PAD;
    let y0 = f.y + PAD + TITLE_H + 12 + lines.clamp(1, TEXT_LINES) as i32 * LINE_H + 16;
    let cw = ((inner - 3 * 12) / 4).max(1);
    let mut out: Vec<(AStop, Rect)> = (0..CHOICES.len()).map(|i| (AStop::Choice(i), Rect { x: f.x + PAD + i as i32 * (cw + 12), y: y0, w: cw, h: ROW_H })).collect();
    out.push((AStop::Nvme, Rect { x: f.x + PAD, y: y0 + ROW_H + 12, w: inner, h: ROW_H }));
    let by = f.y + f.h - PAD - BTN.1;
    let mut x = f.x + f.w - PAD;
    let mut buttons = Vec::new();
    for s in d.stops().into_iter().rev().filter(|s| matches!(s, AStop::Revoke | AStop::No | AStop::Yes)) {
        let w = if s == AStop::Revoke { 210 } else { BTN.0 }.min((inner - 24) / 3).max(1);
        x -= w;
        buttons.push((s, Rect { x, y: by, w, h: BTN.1 }));
        x -= 12;
    }
    buttons.reverse();
    out.extend(buttons);
    out
}

pub fn hit(full: Rect, d: &AccessDialog, lines: usize, x: f64, y: f64) -> Option<AStop> {
    parts(full, d, lines).into_iter().find(|(_, r)| r.contains(x, y)).map(|(s, _)| s)
}

/// What is drawn (cached by value).
#[derive(Clone, Debug, PartialEq)]
pub struct AccessLook {
    pub frame: Rect,
    pub parts: Vec<(AStop, Rect)>,
    pub title: String,
    pub lines: Vec<String>,
    pub choice: usize,
    pub nvme: bool,
    pub focus: AStop,
    pub hover: Option<AStop>,
    pub armed: bool,
    pub note: String,
    pub theme: u64,
}

impl AccessLook {
    pub fn of(full: Rect, d: &AccessDialog, title: String, lines: Vec<String>, note: Option<String>, armed: bool, theme: u64) -> AccessLook {
        let lines: Vec<String> = lines.into_iter().take(TEXT_LINES).collect();
        let n = lines.len();
        AccessLook { frame: frame(full, n), parts: parts(full, d, n), title, lines, choice: d.choice, nvme: d.nvme, focus: d.focus, hover: d.hover, armed, note: note.unwrap_or_else(|| NOTE.to_string()), theme }
    }
}

pub fn draw(l: &AccessLook, t: &Theme) -> Canvas {
    let f = l.frame;
    let (mut c, p) = frame_canvas(f, t);
    let band = |y: i32, h: i32| Rect { x: f.x + PAD, y, w: f.w - 2 * PAD, h };
    let mut y = f.y + PAD;
    c.text_fit(&font(t, 20.0, Weight::SemiBold), p.r(band(y, TITLE_H)), &l.title, t.text);
    y += TITLE_H + 12;
    let body = font(t, 15.0, Weight::Regular);
    for line in &l.lines {
        c.text_fit(&body, p.r(band(y, LINE_H)), line, t.text);
        y += LINE_H;
    }
    for (stop, r) in &l.parts {
        let (focus, hover) = (l.focus == *stop, l.hover == Some(*stop));
        match stop {
            AStop::Choice(i) => {
                let chosen = *i == l.choice;
                let label = if chosen { format!("· {} ·", CHOICES[*i].1) } else { CHOICES[*i].1.to_string() };
                button(&mut c, &p, t, *r, &label, focus, hover || chosen, true);
            }
            AStop::Nvme => {
                let bx = Rect { x: r.x + 4, y: r.y + (r.h - 22) / 2, w: 22, h: 22 };
                c.rrect_line(p.b(bx), p.v(4.0), p.v(2.0), if focus { t.accent } else { t.muted });
                if l.nvme {
                    c.rrect(p.b(Rect { x: bx.x + 5, y: bx.y + 5, w: 12, h: 12 }), p.v(2.0), t.warm);
                }
                let tr = Rect { x: bx.x + 34, w: r.w - 38, ..*r };
                c.text_mid(&font(t, 15.0, Weight::Medium), p.r(tr), NVME_TEXT, if focus || hover { t.text } else { t.muted });
            }
            AStop::Revoke => button(&mut c, &p, t, *r, "Fernzugriff beenden", focus, hover, true),
            AStop::No => button(&mut c, &p, t, *r, "Ablehnen", focus, hover, true),
            AStop::Yes => button(&mut c, &p, t, *r, "Erlauben", focus, hover, l.armed),
        }
    }
    let ny = l.parts.iter().find(|(s, _)| *s == AStop::Nvme).map_or(f.y, |(_, r)| r.y + r.h + 10);
    c.text_fit(&font(t, 13.0, Weight::Regular), p.r(band(ny, NOTE_H)), &l.note, t.muted);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::from_secs(100);

    fn d(revoke: bool) -> AccessDialog {
        AccessDialog::new(Some(4), "a".repeat(64), "bitsos-desk".into(), 24, "Text".into(), revoke, T0, Some(Duration::from_secs(240)))
    }

    #[test]
    fn enter_grants_24_hours_but_only_armed_and_only_for_real() {
        let mut a = d(false);
        assert_eq!(a.key(Keysym::Return, Origin::Physical, T0), AStep::Stay(false), "not armed yet");
        // Keys keep coming (someone typing elsewhere): each one before the arming starts it again.
        assert_eq!(a.key(Keysym::Return, Origin::Physical, T0 + ACCESS_ARM - Duration::from_millis(10)), AStep::Stay(false));
        assert!(!a.armed(T0 + ACCESS_ARM + Duration::from_millis(10)), "re-armed from the last key");
        let later = T0 + 2 * ACCESS_ARM;
        assert_eq!(a.key(Keysym::Return, Origin::Physical, later), AStep::Grant { hours: 24, nvme: false, input: Origin::Physical });
        // Injected: navigation dropped, a press sent as injected (refused by the installer), Esc a no.
        let mut b = d(false);
        for k in [Keysym::Tab, Keysym::Left, Keysym::Right, Keysym::Down] {
            assert_eq!(b.key(k, Origin::Injected, later), AStep::Ignored);
        }
        assert_eq!((b.focus, b.choice, b.nvme), (AStop::Yes, DEFAULT, false), "injected input moved nothing");
        assert_eq!(b.key(Keysym::Return, Origin::Injected, later), AStep::Grant { hours: 24, nvme: false, input: Origin::Injected });
        assert_eq!(b.key(Keysym::Escape, Origin::Injected, later), AStep::No { input: Origin::Injected });
        assert_eq!(b.click(Some(AStop::Nvme), Origin::Injected, later), AStep::Ignored);
        assert!(!b.nvme, "an injected click never ticks the NVMe box");
    }

    #[test]
    fn choices_the_nvme_box_and_revoke() {
        let later = T0 + ACCESS_ARM;
        let mut a = d(true);
        assert!(a.stops().contains(&AStop::Revoke));
        // Up from Erlauben: Ablehnen, Fernzugriff beenden, the box, 7 Tage.
        for want in [AStop::No, AStop::Revoke, AStop::Nvme, AStop::Choice(3)] {
            a.key(Keysym::Up, Origin::Physical, later);
            assert_eq!(a.focus, want);
        }
        a.key(Keysym::Left, Origin::Physical, later);
        assert_eq!((a.focus, a.choice), (AStop::Choice(2), 2));
        a.key(Keysym::Left, Origin::Physical, later);
        a.key(Keysym::Left, Origin::Physical, later);
        a.key(Keysym::Left, Origin::Physical, later);
        assert_eq!(a.choice, 0, "1 h, and it stops there");
        a.focus = AStop::Nvme;
        assert_eq!(a.key(Keysym::space, Origin::Physical, later), AStep::Stay(true));
        assert!(a.nvme);
        assert_eq!(a.click(Some(AStop::Yes), Origin::Physical, later), AStep::Grant { hours: 1, nvme: true, input: Origin::Physical });
        assert_eq!(a.click(Some(AStop::Revoke), Origin::Physical, later), AStep::Revoke);
        assert_eq!(a.click(Some(AStop::Choice(3)), Origin::Physical, later), AStep::Stay(true));
        assert_eq!((a.hours_chosen(), a.focus), (168, AStop::Yes), "a choice moves the focus to Erlauben");
        assert_eq!(a.key(Keysym::Escape, Origin::Physical, later), AStep::No { input: Origin::Physical });
    }

    #[test]
    fn texts_have_glyphs() {
        let mut all: Vec<String> = TEXTS.iter().map(|s| s.to_string()).collect();
        all.extend(CHOICES.iter().map(|c| format!("· {} ·", c.1)));
        all.push(title("bitsos-desk", "ab12 cd34 ef56 7890", "03.11. 14:00"));
        all.push(chip("14:00"));
        for t in &all {
            assert!(t.chars().all(crate::text::Font::has), "{t}");
        }
    }

    #[test]
    fn the_parts_lie_inside_the_screen() {
        for (w, h) in [(2048, 1152), (1280, 720), (800, 600)] {
            let full = Rect { x: 0, y: 0, w, h };
            for revoke in [false, true] {
                let a = d(revoke);
                for lines in [1, 4, 9, 30] {
                    let p = parts(full, &a, lines);
                    assert_eq!(p.len(), a.stops().len());
                    let f = frame(full, lines);
                    for (s, r) in &p {
                        assert!(r.x >= f.x && r.y >= f.y && r.x + r.w <= f.x + f.w && r.y + r.h <= f.y + f.h, "{s:?} {r:?} in {f:?} on {w}x{h}");
                        assert_eq!(hit(full, &a, lines, r.x as f64 + 2.0, r.y as f64 + 2.0), Some(*s));
                    }
                }
            }
        }
        assert_eq!(title("bitsos-desk", "ab12 cd34 ef56 7890", "14:00"), "Vollzugriff für den Gaming-PC (bitsos-desk, ab12 cd34 ef56 7890) bis 14:00");
        assert_eq!(chip("14:00"), "Fernzugriff offen bis 14:00");
    }
}
