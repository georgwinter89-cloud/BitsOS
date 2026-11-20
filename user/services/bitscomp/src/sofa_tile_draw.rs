//! GM-6b: the sofa mode's tiles and focus ring (DS rule "new look files"; plan/spec/sofa-mode.md §1, boards Main and
//! Bibliothek): a game tile in its kontext colour (or panel with the 1 px line), its name bottom-left (Bricolage, 600
//! until GM-6f's 700) over its muted line, the one focus ring of big elements (3 board px accent at 4 px distance and
//! the HUD corner brackets top-left and bottom-right), the dashed "Steam öffnen" tile. Pure drawings into a Canvas.

use crate::canvas::{Box2, Canvas};
use crate::layout::Rect;
use crate::sofa::Frame;
use crate::sofa_draw::{Ink, font};
use crate::sofa_header_draw::local;
use crate::sofa_model::Tile;
use crate::text::{Font, Typeface, Weight};
use crate::theme::{DARK, Rgba, alpha, hex};

/// The tiles' colours: kontext 1-3 and the accent (§1), indexed by `sofa_model::colour`.
pub fn kontext() -> [Rgba; 4] {
    let h = |s: &str| hex(s).unwrap_or([0.5, 0.5, 0.5, 1.0]);
    [h(DARK.kontext[0]), h(DARK.kontext[1]), h(DARK.kontext[2]), h(DARK.accent)]
}

pub fn on_kontext() -> Rgba {
    hex(DARK.on_kontext).unwrap_or([0.07, 0.1, 0.09, 1.0])
}

pub fn warnung() -> Rgba {
    hex(DARK.warnung).unwrap_or([1.0, 0.48, 0.42, 1.0])
}

pub fn band(b: Box2) -> Rect {
    Rect { x: b.x.round() as i32, y: b.y.round() as i32, w: b.w.round() as i32, h: b.h.round() as i32 }
}

/// The focus ring of a big element (§1): the outline and the two corner brackets; `r` is the element's radius.
pub fn focus_ring(c: &mut Canvas, b: Box2, r: f32, k: f32, ink: &Ink) {
    let (w, d) = (3.0 * k, 4.0 * k);
    c.rrect_line(b.inset(-d - w), r + d + w, w, ink.accent);
    let (o, arm, br) = (10.0 * k, 28.0 * k, 16.0 * k);
    let half = w / 2.0;
    // Top left: the bracket's outer edge 10 px outside the tile.
    let (x, y) = (b.x - o + half, b.y - o + half);
    let q = std::f32::consts::FRAC_PI_2 * (br - half);
    c.line((x + br, y), (x + arm - half, y), w, ink.accent);
    c.line((x, y + br), (x, y + arm - half), w, ink.accent);
    c.arc(x + br, y + br, br - half, w, Some((q, 3.0 * q, 3.0 * q)), ink.accent);
    let (x, y) = (b.x + b.w + o - half, b.y + b.h + o - half);
    c.line((x - br, y), (x - arm + half, y), w, ink.accent);
    c.line((x, y - br), (x, y - arm + half), w, ink.accent);
    c.arc(x - br, y - br, br - half, w, Some((q, 3.0 * q, q)), ink.accent);
}

/// `s` in at most two lines of `width` (greedy by words; the second cut with an ellipsis).
pub fn two_lines(f: &Font, s: &str, width: i32) -> Vec<String> {
    if f.width(s) <= width {
        return vec![s.to_string()];
    }
    let mut first = String::new();
    let mut rest = s;
    for (i, _) in s.match_indices(' ') {
        if f.width(&s[..i]) <= width {
            first = s[..i].to_string();
            rest = s[i + 1..].trim_start();
        } else {
            break;
        }
    }
    if first.is_empty() {
        return vec![f.fit(s, width)];
    }
    vec![first, f.fit(rest, width)]
}

/// The size of a tile's texts (board px): the start row's (30/16, padding 24) or the library grid's (28/15, 22).
#[derive(Clone, Copy)]
pub struct Sizes {
    pub name: f32,
    pub meta: f32,
    pub pad: i32,
}

pub const START: Sizes = Sizes { name: 30.0, meta: 16.0, pad: 24 };
pub const GRID: Sizes = Sizes { name: 28.0, meta: 15.0, pad: 22 };

/// One tile at board rect `r` (already lifted when it is the focused one).
pub fn tile(c: &mut Canvas, f: &Frame, o: Rect, r: Rect, t: &Tile, z: Sizes, ink: &Ink) {
    let k = f.k as f32;
    let b = local(f, o, r);
    let rad = 20.0 * k;
    let (fg, sub) = match t.colour {
        Some(i) => {
            c.rrect(b, rad, kontext()[i.min(3)]);
            (on_kontext(), on_kontext())
        }
        None => {
            c.rrect(b, rad, ink.panel);
            c.rrect_line(b, rad, k.max(1.0), ink.line);
            (ink.text, ink.muted)
        }
    };
    let (fg, sub) = if t.greyed { (alpha(fg, 0.45), alpha(sub, 0.45)) } else { (fg, sub) };
    let name = font(Typeface::Bricolage, z.name, Weight::SemiBold, k);
    let meta = font(Typeface::Figtree, z.meta, Weight::SemiBold, k);
    let inner = r.w - 2 * z.pad;
    let lines = two_lines(&name, &t.name, (inner as f32 * k) as i32);
    let meta_h = (z.meta * 1.45).round() as i32;
    let name_h = (z.name * 1.07).round() as i32;
    let my = r.y + r.h - z.pad - meta_h;
    c.text_fit(&meta, band(local(f, o, Rect { x: r.x + z.pad, y: my, w: inner, h: meta_h })), &t.meta, sub);
    let mut y = my - 4 - name_h * lines.len() as i32;
    for l in &lines {
        c.text_fit(&name, band(local(f, o, Rect { x: r.x + z.pad, y, w: inner, h: name_h })), l, fg);
        y += name_h;
    }
}

/// A dashed rounded border (CSS `2px dashed`): dashes of 3 widths along the straight edges, the corners whole.
fn dashed(c: &mut Canvas, b: Box2, r: f32, w: f32, col: Rgba) {
    let (dash, gap) = (3.0 * w, 3.0 * w);
    let h = w / 2.0;
    let edge = |c: &mut Canvas, a: (f32, f32), z: (f32, f32)| {
        let len = (z.0 - a.0).hypot(z.1 - a.1);
        let n = ((len + gap) / (dash + gap)).floor().max(1.0);
        let step = len / n;
        for i in 0..n as i32 {
            let t0 = (i as f32 * step + (step - dash).max(0.0) / 2.0) / len;
            let t1 = (t0 * len + dash.min(step)) / len;
            let p = |t: f32| (a.0 + (z.0 - a.0) * t, a.1 + (z.1 - a.1) * t);
            c.line(p(t0), p(t1), w, col);
        }
    };
    let (x0, y0, x1, y1) = (b.x + h, b.y + h, b.x + b.w - h, b.y + b.h - h);
    edge(c, (x0 + r, y0), (x1 - r, y0));
    edge(c, (x0 + r, y1), (x1 - r, y1));
    edge(c, (x0, y0 + r), (x0, y1 - r));
    edge(c, (x1, y0 + r), (x1, y1 - r));
    let rr = r - h;
    let q = std::f32::consts::FRAC_PI_2 * rr;
    for (cx, cy, start) in [(x0 + r, y0 + r, 3.0 * q), (x1 - r, y0 + r, 0.0), (x1 - r, y1 - r, q), (x0 + r, y1 - r, 2.0 * q)] {
        c.arc(cx, cy, rr, w, Some((q, 3.0 * q, start)), col);
    }
}

/// The dashed "Steam öffnen" tile (2.1): 2 px dashed line_strong, the plus in accent, the word under it.
pub fn steam_tile(c: &mut Canvas, f: &Frame, o: Rect, r: Rect, ink: &Ink) {
    let k = f.k as f32;
    let b = local(f, o, r);
    dashed(c, b, 20.0 * k, 2.0 * k, ink.line_strong);
    let (cx, top) = (b.x + b.w / 2.0, b.y + (b.h - (40.0 + 10.0 + 26.0) * k) / 2.0);
    let cy = top + 20.0 * k;
    c.line((cx - 12.0 * k, cy), (cx + 12.0 * k, cy), 3.0 * k, ink.accent);
    c.line((cx, cy - 12.0 * k), (cx, cy + 12.0 * k), 3.0 * k, ink.accent);
    let t = Box2 { x: b.x, y: top + 50.0 * k, w: b.w, h: 26.0 * k };
    c.text_mid(&font(Typeface::Figtree, 20.0, Weight::SemiBold, k), band(t), STEAM_OPEN, ink.text);
}

pub const STEAM_OPEN: &str = "Steam öffnen";

/// The board's label style (`.lbl`): JetBrains Mono 13 500, upper case, .08 em apart.
pub fn label(k: f32) -> Font {
    font(Typeface::JetBrainsMono, 13.0, Weight::Medium, k).spaced(0.08)
}
