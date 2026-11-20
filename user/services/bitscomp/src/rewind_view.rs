//! REWIND: the rewind view's rectangles (logical px) from the Verlauf board (1920x1080, board px through
//! `Theme::bl`): the header (Zur Bühne, the title, the date), the upper panel with the chosen time at its left,
//! the three ways back under it and the right side for the chosen way, the lower panel with the time line (chips
//! newest first, two arrows), the foot (the message line and "Zum Jetzt"). Pure: the look, the clicks and
//! `ctl hot` read the same rectangles.

use crate::layout::Rect;
use crate::rewind::{Hot, Tab};
use crate::text::{Family, Weight};
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq)]
pub struct Geo {
    pub full: Rect,
    pub back: Rect,
    pub title: Rect,
    pub date: Rect,
    pub upper: Rect,
    /// "STAND UM", the big time, its line under it, the kind chip.
    pub label: Rect,
    pub time: Rect,
    pub ago: Rect,
    pub chip: Rect,
    /// The three ways back, in `Tab::WAYS` order.
    pub ways: [Rect; 3],
    /// The chosen way's side.
    pub pane: Rect,
    pub lower: Rect,
    pub lower_label: Rect,
    pub lower_day: Rect,
    pub older: Rect,
    pub newer: Rect,
    /// The time line's chips (as many as fit), left = newest of those shown.
    pub chips: Vec<Rect>,
    pub foot: Rect,
    pub now: Rect,
}

/// The pane's parts for a list (folders, programs) and its action.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneGeo {
    /// "Ordner höher" (folders only) and the line beside it (the path, or the list's label).
    pub up: Rect,
    pub head: Rect,
    pub rows: Vec<Rect>,
    /// The line left of the action button (what is chosen).
    pub says: Rect,
    pub act: Rect,
    /// The second button of "Alles" after it was set (Rückgängig), left of `act` (Jetzt neu starten).
    pub act2: Rect,
}

pub const BACK: &str = "Zur Bühne";
pub const NOW: &str = "Zum Jetzt";

fn button_w(t: &Theme, text: &str) -> i32 {
    let f = t.bfont(Family::Ui, 14.0, Weight::SemiBold);
    (f.width(text) as f64 / t.scale).ceil() as i32 + t.bl(36.0)
}

pub fn geo(full: Rect, t: &Theme) -> Geo {
    let b = |v: f32| t.bl(v);
    let (x0, y0) = (full.x + b(48.0), full.y);
    let wide = (full.w - b(96.0)).max(1);
    let back = Rect { x: x0, y: y0 + b(20.0), w: button_w(t, BACK) + b(24.0), h: b(44.0) };
    let title = Rect { x: back.x + back.w + b(16.0), y: back.y, w: b(400.0), h: back.h };
    let date = Rect { x: x0 + wide - b(400.0), y: back.y, w: b(400.0), h: back.h };
    let foot = Rect { x: x0, y: full.y + full.h - b(108.0), w: wide, h: b(64.0) };
    let nw = button_w(t, NOW);
    let now = Rect { x: foot.x + foot.w - nw, y: foot.y + (foot.h - b(44.0)) / 2, w: nw, h: b(44.0) };
    let lower = Rect { x: x0, y: foot.y - b(24.0) - b(220.0), w: wide, h: b(220.0) };
    let upper_y = y0 + b(84.0);
    let upper = Rect { x: x0, y: upper_y, w: wide, h: (lower.y - b(24.0) - upper_y).max(b(200.0)) };
    let (lx, ly) = (upper.x + b(56.0), upper.y + b(48.0));
    let label = Rect { x: lx, y: ly, w: b(700.0), h: b(14.0) };
    let time = Rect { x: lx, y: ly + b(20.0), w: b(700.0), h: b(120.0) };
    let ago = Rect { x: lx, y: time.y + time.h + b(4.0), w: b(700.0), h: b(20.0) };
    let chip = Rect { x: lx, y: ago.y + ago.h + b(12.0), w: b(700.0), h: b(32.0) };
    let pane_x = upper.x + b(840.0);
    let pane = Rect { x: pane_x, y: upper.y + b(48.0), w: (upper.x + upper.w - b(56.0) - pane_x).max(1), h: (upper.h - b(96.0)).max(1) };
    let ways_top = chip.y + chip.h + b(28.0);
    let way_w = (pane.x - b(40.0) - lx).clamp(1, b(700.0));
    let way_h = ((upper.y + upper.h - b(32.0) - ways_top - b(20.0)) / 3).clamp(b(40.0), b(84.0));
    let ways = [0, 1, 2].map(|i| Rect { x: lx, y: ways_top + i * (way_h + b(10.0)), w: way_w, h: way_h });
    let lower_label = Rect { x: lower.x + b(24.0), y: lower.y + b(24.0), w: b(180.0), h: b(14.0) };
    let lower_day = Rect { x: lower_label.x, y: lower_label.y + b(22.0), w: b(180.0), h: b(24.0) };
    let older = Rect { x: lower_label.x, y: lower_day.y + b(40.0), w: b(44.0), h: b(44.0) };
    let newer = Rect { x: older.x + b(52.0), ..older };
    let (cx, cw, gap) = (lower.x + b(232.0), b(156.0), b(12.0));
    let room = lower.x + lower.w - b(24.0) - cx;
    let n = ((room + gap) / (cw + gap)).max(1);
    let chips = (0..n).map(|i| Rect { x: cx + i * (cw + gap), y: lower.y + b(40.0), w: cw, h: b(140.0) }).collect();
    Geo { full, back, title, date, upper, label, time, ago, chip, ways, pane, lower, lower_label, lower_day, older, newer, chips, foot, now }
}

/// The pane's list rows and buttons; `act` is the action button's text (its width), `act2` the second one's.
pub fn pane(g: &Geo, t: &Theme, act: &str, act2: &str) -> PaneGeo {
    let b = |v: f32| t.bl(v);
    let p = g.pane;
    let up = Rect { x: p.x + b(24.0), y: p.y + b(20.0), w: button_w(t, "Ordner höher") + b(20.0), h: b(44.0) };
    let head = Rect { x: up.x + up.w + b(16.0), y: up.y, w: (p.x + p.w - b(24.0) - (up.x + up.w + b(16.0))).max(1), h: up.h };
    let aw = button_w(t, act).min(p.w - b(48.0)).max(1);
    let act_r = Rect { x: p.x + p.w - b(24.0) - aw, y: p.y + p.h - b(20.0) - b(44.0), w: aw, h: b(44.0) };
    let a2w = if act2.is_empty() { 0 } else { button_w(t, act2) };
    let act2_r = Rect { x: act_r.x - b(12.0) - a2w, w: a2w, ..act_r };
    let says = Rect { x: p.x + b(24.0), y: act_r.y, w: ((if a2w > 0 { act2_r.x } else { act_r.x }) - b(16.0) - (p.x + b(24.0))).max(1), h: act_r.h };
    let (top, end, rh, gap) = (up.y + up.h + b(16.0), act_r.y - b(16.0), b(48.0), b(6.0));
    let mut rows = Vec::new();
    let mut y = top;
    while y + rh <= end {
        rows.push(Rect { x: p.x + b(24.0), y, w: (p.w - b(48.0)).max(1), h: rh });
        y += rh + gap;
    }
    PaneGeo { up, head, rows, says, act: act_r, act2: act2_r }
}

/// The text lines of the "Alles" side (explanation, then what is set), each `line` high from the pane's top.
pub fn text_lines(g: &Geo, t: &Theme, n: usize) -> Vec<Rect> {
    let b = |v: f32| t.bl(v);
    (0..n).map(|i| Rect { x: g.pane.x + b(28.0), y: g.pane.y + b(28.0) + i as i32 * b(30.0), w: (g.pane.w - b(56.0)).max(1), h: b(26.0) }).collect()
}

/// The parts at (x, y) in order of `parts` (the first that holds it).
pub fn hit(parts: &[(Hot, Rect)], x: f64, y: f64) -> Option<Hot> {
    parts.iter().find(|(_, r)| r.contains(x, y)).map(|(h, _)| *h)
}

/// The tab of way `i` (0..3).
pub fn way(i: usize) -> Tab {
    Tab::WAYS[i.min(2)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_board_fits_the_mini_pc_and_a_small_output() {
        for (w, h, rows) in [(2048, 1152, 6), (1600, 900, 2)] {
            let t = Theme::default();
            let full = Rect { x: 0, y: 0, w, h };
            let g = geo(full, &t);
            let inside = |r: Rect| r.x >= 0 && r.y >= 0 && r.x + r.w <= w && r.y + r.h <= h;
            for r in [g.back, g.date, g.upper, g.pane, g.lower, g.older, g.newer, g.foot, g.now] {
                assert!(inside(r), "{r:?} at {w}x{h}");
            }
            assert!(g.upper.y + g.upper.h < g.lower.y && g.lower.y + g.lower.h < g.foot.y, "panels apart at {w}x{h}");
            assert!(g.chips.len() >= 4 && g.chips.iter().all(|c| inside(*c)), "{} chips at {w}x{h}", g.chips.len());
            assert!(g.ways.iter().all(|r| inside(*r) && r.x + r.w < g.pane.x), "the ways left of the pane");
            let p = pane(&g, &t, "„brief.txt“ zurückholen", "");
            assert!(p.rows.len() >= rows, "{} rows at {w}x{h}", p.rows.len());
            assert!(p.rows.iter().all(|r| r.y + r.h < p.act.y) && inside(p.act), "rows above the button");
        }
    }

    #[test]
    fn hits_take_the_first_part() {
        let a = Rect { x: 0, y: 0, w: 10, h: 10 };
        assert_eq!(hit(&[(Hot::DialogOk, a), (Hot::Back, a)], 5.0, 5.0), Some(Hot::DialogOk));
        assert_eq!(hit(&[(Hot::Back, a)], 20.0, 5.0), None);
        assert_eq!(way(1), Tab::Program);
    }
}
