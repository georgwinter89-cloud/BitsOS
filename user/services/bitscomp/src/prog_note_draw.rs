//! PE-1: the look of the program-error card. It lies in the notice's layer (`chrome_render::over`): above every
//! window (fullscreen ones too), the bars and the library; at the right under the top bar, so the address bar's
//! notice and the habit hint on the left stay readable. Geometry in logical px, shared by the look, the hit and
//! `ctl hot`. The program's lines are in the mono face, newest at the bottom; as many as fit (at least three).

use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::{Px, font};
use crate::layout::Rect;
use crate::prog_end::{CLOSE, LINES_HEAD, NO_LINES};
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::Theme;

const PAD: i32 = 16;
const TITLE_H: i32 = 26;
const LINE_H: i32 = 20;
const HEAD_H: i32 = 22;
const CODE_H: i32 = 17;
const BTN: (i32, i32) = (130, 34);

/// Where the card's parts lie, and how many of the program's lines it shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Boxes {
    pub card: Rect,
    pub close: Rect,
    pub lines: usize,
}

/// What one picture of the card was made from (a new picture only when it changes).
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    seq: u64,
    card: Rect,
    reason: Vec<String>,
    lines: usize,
    more: Option<String>,
    hover: bool,
    theme: u64,
}

/// The card's width on a screen `full` wide (logical px).
fn width(full: Rect) -> i32 {
    (full.w - 32).clamp(1, 760)
}

/// The reason wrapped to the card's width (`wide`: a function giving a text's logical width).
pub fn reason_lines(reason: &str, w: i32, wide: &dyn Fn(&str) -> i32) -> Vec<String> {
    crate::install_dialog::wrap(reason, (w - 2 * PAD).max(1), wide)
}

/// The boxes for a card with `reasons` reason lines and `n` program lines below `top` (the top bar's lower edge).
pub fn boxes(full: Rect, top: i32, reasons: usize, n: usize) -> Boxes {
    let w = width(full);
    let fixed = 2 * PAD + TITLE_H + reasons as i32 * LINE_H + 8 + HEAD_H + 12 + BTN.1;
    // Half the screen at most for the lines, never fewer than three (or what there is).
    let room = (((full.h / 2) - fixed) / CODE_H).max(3) as usize;
    let lines = n.min(room);
    let h = fixed + lines.max(1) as i32 * CODE_H;
    let card = Rect { x: full.x + full.w - w - 16, y: top + 8, w, h };
    let close = Rect { x: card.x + w - PAD - BTN.0, y: card.y + h - PAD - BTN.1, w: BTN.0, h: BTN.1 };
    Boxes { card, close, lines }
}

#[allow(clippy::too_many_arguments)]
pub fn draw(title: &str, reason: &[String], lines: &[String], b: &Boxes, more: Option<&str>, hover: bool, t: &Theme) -> Canvas {
    let f = b.card;
    let p = Px::new(f, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let all = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, p.v(12.0), t.panel);
    c.rrect_line(all, p.v(12.0), p.v(2.0), t.danger);
    let band = |y: i32, h: i32| p.r(Rect { x: f.x + PAD, y, w: f.w - 2 * PAD, h });
    let mut y = f.y + PAD;
    c.text_fit(&font(t, 15.0, Weight::SemiBold), band(y, TITLE_H), title, t.text);
    y += TITLE_H;
    let rf = font(t, 13.0, Weight::Regular);
    for l in reason {
        c.text_fit(&rf, band(y, LINE_H), l, t.text);
        y += LINE_H;
    }
    y += 8;
    let hf = font(t, 12.0, Weight::Medium);
    c.text_fit(&hf, band(y, HEAD_H), if lines.is_empty() { NO_LINES } else { LINES_HEAD }, t.muted);
    y += HEAD_H;
    let mono = t.font(Family::Mono, 12.0, Weight::Regular);
    for l in &lines[lines.len() - b.lines.min(lines.len())..] {
        c.text_fit(&mono, band(y, CODE_H), l, t.text);
        y += CODE_H;
    }
    if let Some(m) = more {
        let row = Rect { x: f.x + PAD, y: b.close.y, w: b.close.x - f.x - 2 * PAD, h: b.close.h };
        c.text_fit(&rf, p.r(row), m, t.muted);
    }
    crate::install_draw::button(&mut c, &p, t, b.close, CLOSE, false, hover, true);
    c
}

impl App {
    /// Where the shown card lies now (None: no card on the screen).
    pub fn prog_note_boxes(&self) -> Option<Boxes> {
        let n = self.prog_note_shown()?;
        let full = self.area_full();
        let a = self.chrome().address;
        let r = self.prog_note_reason(&n.reason);
        Some(boxes(full, a.y + a.h, r.len(), n.lines.len()))
    }

    fn prog_note_reason(&self, reason: &str) -> Vec<String> {
        let t = &self.theme;
        let f = font(t, 13.0, Weight::Regular);
        reason_lines(reason, width(self.area_full()), &|s| crate::chrome::logical_width(&f, s, t.scale))
    }

    pub fn prog_note_elements<R: CompRenderer>(&mut self, renderer: &mut R) -> Vec<CompElement<R>> {
        let Some(b) = self.prog_note_boxes() else { return Vec::new() };
        let Some(n) = self.prog_note_shown() else { return Vec::new() };
        let reason = self.prog_note_reason(&n.reason);
        let more = crate::prog_end::more(self.runs.notes.len() - 1);
        let look = Look { seq: n.seq, card: b.card, reason, lines: b.lines, more, hover: self.runs.hover, theme: self.theme_gen };
        if self.runs.image.as_ref().is_none_or(|(l, _)| *l != look) {
            let c = draw(&n.title, &look.reason, &n.lines, &b, look.more.as_deref(), look.hover, &self.theme);
            self.runs.image = Some((look, c.into_buffer()));
        }
        let buf = &self.runs.image.as_ref().expect("set above").1;
        let px = crate::header::px_rect(b.card, self.theme.scale);
        crate::deco::image(renderer, buf, b.card, (px.w, px.h), self.theme.scale).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_fits_any_screen_and_shows_at_least_three_lines() {
        for full in [Rect { x: 0, y: 0, w: 2560, h: 1440 }, Rect { x: 0, y: 0, w: 1280, h: 720 }, Rect { x: 0, y: 0, w: 400, h: 300 }] {
            let b = boxes(full, 48, 2, 500);
            assert!(b.card.x >= full.x && b.card.x + b.card.w <= full.x + full.w, "{full:?}");
            assert!(b.close.x >= b.card.x && b.close.x + b.close.w <= b.card.x + b.card.w, "{full:?}");
            assert!(b.close.y + b.close.h <= b.card.y + b.card.h, "{full:?}");
            assert!(b.lines >= 3 && b.lines < 500, "{full:?}: {}", b.lines);
        }
        let big = boxes(Rect { x: 0, y: 0, w: 2560, h: 1440 }, 48, 1, 500);
        assert!(big.card.y + big.card.h <= 48 + 8 + 1440 / 2 + CODE_H, "half the screen at most");
        assert_eq!(boxes(Rect { x: 0, y: 0, w: 2560, h: 1440 }, 48, 1, 2).lines, 2, "a short output shows whole");
        assert_eq!(boxes(Rect { x: 0, y: 0, w: 2560, h: 1440 }, 48, 1, 0).lines, 0);
    }

    #[test]
    fn the_card_draws_its_frame_and_text() {
        let t = crate::theme::test_theme();
        let b = boxes(Rect { x: 0, y: 0, w: 1280, h: 720 }, 40, 1, 2);
        let lines = vec!["fake vkquake gives up".to_string(), "x".repeat(400)];
        let c = draw("„vkQuake“ hat sich gleich nach dem Start beendet.", &["Grund".into()], &lines, &b, Some("Noch 1 weitere Meldung"), true, t);
        let p = Px::new(b.card, t.scale);
        let (w, h) = p.size();
        let danger = crate::canvas::pack(t.danger);
        assert!((0..h).any(|y| c.pixel(1, y) == danger) || (0..w).any(|x| c.pixel(x, 1) == danger), "the frame");
        let text = crate::canvas::pack(t.text);
        assert!((0..w).any(|x| (0..h).any(|y| c.pixel(x, y) == text)), "some text");
    }

    #[test]
    fn a_long_reason_wraps_inside_the_card() {
        let one = |s: &str| s.chars().count() as i32 * 8;
        let r = reason_lines(&"Wort ".repeat(60), 400, &one);
        assert!(r.len() > 1 && r.iter().all(|l| one(l) <= 400 - 2 * PAD), "{r:?}");
    }
}
