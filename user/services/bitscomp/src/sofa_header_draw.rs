//! GM-6a: the look of the sofa mode's header and footer (DS rule "new look files"; plan/spec/sofa-mode.md §1, §2,
//! board Main.dc.html): the header's glass bar (panel at 72 %, no live blur), the BitsOS mark and word, the nav
//! pills (the active one accent_soft with accent text), the round search button, the status words and the clock,
//! the Menü pill; the footer's hints with their button signs. Physical px = board px x k (sofa.rs `Frame`); the
//! sofa mode is always dark ("Absicht dunkel"). Pure drawings into a `Canvas`: sofa_draw.rs caches them by look.

use crate::canvas::{Box2, Canvas};
use crate::layout::Rect;
use crate::sofa::Frame;
use crate::sofa_draw::{Ink, font};
use crate::sofa_layout::{self as lay, Header, Placed, Text};
use crate::text::{Typeface, Weight};
use crate::theme::alpha;

#[derive(Clone, Debug, PartialEq)]
pub struct HeaderLook {
    pub frame: Frame,
    pub bw: i32,
    pub header: Header,
    pub clock: String,
    pub active: Option<String>,
    pub focus: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FooterLook {
    pub frame: Frame,
    pub bw: i32,
    pub bh: i32,
    pub hints: Vec<Placed>,
    pub hover: Option<String>,
}

/// Board rect `b` in the canvas whose physical origin is `o`.
pub fn local(f: &Frame, o: Rect, b: Rect) -> Box2 {
    let r = f.r(b);
    Box2 { x: (r.x - o.x) as f32, y: (r.y - o.y) as f32, w: r.w as f32, h: r.h as f32 }
}

fn band(b: Box2) -> Rect {
    Rect { x: b.x.round() as i32, y: b.y.round() as i32, w: b.w.round() as i32, h: b.h.round() as i32 }
}

/// The token ring of small elements (§1): 2 px ground, then 3 px accent (box-shadow 0 0 0 2px bg, 0 0 0 5px accent).
pub fn token_ring(c: &mut Canvas, b: Box2, r: f32, k: f32, ink: &Ink) {
    let (g, a) = (2.0 * k, 3.0 * k);
    c.rrect_line(b.inset(-g), r + g, g, ink.bg);
    c.rrect_line(b.inset(-g - a), r + g + a, a, ink.accent);
}

/// The header image: the bar `0,0 bw x 96` of the board; `o` is the image's physical place.
pub fn draw_header(l: &HeaderLook, o: Rect, ink: &Ink) -> Canvas {
    let f = &l.frame;
    let k = f.k as f32;
    let mut c = Canvas::new(o.w, o.h);
    let bar = local(f, o, Rect { x: 0, y: 0, w: l.bw, h: lay::HEADER_H });
    // The glass bar reaches the output's edges (the overscan inset only moves the content).
    c.fill(0, 0, o.w, bar.y as i32 + bar.h as i32, alpha(ink.panel, 0.72));
    c.fill(0, (bar.y + bar.h - k.max(1.0)) as i32, o.w, k.max(1.0).round() as i32, ink.line);
    let h = &l.header;
    // The mark: a ring (r 15, 3 px) and the accent dot (r 5) in a 40 px box.
    let m = local(f, o, h.mark);
    let (cx, cy) = (m.x + m.w / 2.0, m.y + m.h / 2.0);
    c.ring(cx, cy, 15.0 * k - 1.5 * k, 3.0 * k, ink.text);
    c.disc(cx, cy, 5.0 * k, ink.accent);
    c.text_fit(&font(Typeface::Bricolage, 28.0, Weight::SemiBold, k), band(local(f, o, h.word)), "BitsOS", ink.text);
    for it in &h.items {
        let b = local(f, o, it.rect);
        let focused = l.focus.as_deref() == Some(it.id.as_str());
        match it.id.as_str() {
            "search" => {
                c.rrect_line(b, b.h / 2.0, k.max(1.0), ink.line_strong);
                // The magnifier of the board: a circle r 6.5 at (10.5, 10.5) of 24 and the handle to (21, 21).
                let (x0, y0) = (b.x + (b.w - 24.0 * k) / 2.0, b.y + (b.h - 24.0 * k) / 2.0);
                c.ring(x0 + 10.5 * k, y0 + 10.5 * k, 6.5 * k, 2.0 * k, ink.text);
                c.line((x0 + 15.5 * k, y0 + 15.5 * k), (x0 + 21.0 * k, y0 + 21.0 * k), 2.0 * k, ink.text);
            }
            "menu" => {
                c.rrect_line(b, b.h / 2.0, k.max(1.0), ink.line_strong);
                let (x0, y0) = (b.x + 22.0 * k, b.y + (b.h - 22.0 * k) / 2.0);
                for dy in [6.0, 11.0, 16.0] {
                    c.line((x0 + 3.0 * k, y0 + dy * k), (x0 + 19.0 * k, y0 + dy * k), 2.0 * k, ink.text);
                }
                let t = Box2 { x: x0 + 32.0 * k, y: b.y, w: b.w - 54.0 * k, h: b.h };
                c.text_fit(&font(Typeface::Figtree, 18.0, Weight::SemiBold, k), band(t), "Menü", ink.text);
            }
            id => {
                let active = l.active.as_deref() == Some(id);
                let word = crate::sofa_focus::Screen::PILLS.iter().find(|s| id == format!("pill:{}", s.word())).map_or("", |s| s.title());
                if active {
                    c.rrect(b, b.h / 2.0, ink.accent_soft);
                }
                let (w, col) = if active { (Weight::SemiBold, ink.accent) } else { (Weight::Medium, ink.text) };
                c.text_mid(&font(Typeface::Figtree, 20.0, w, k), band(b), word, col);
            }
        }
        if focused {
            token_ring(&mut c, b, b.h / 2.0, k, ink);
        }
    }
    let st = font(Typeface::JetBrainsMono, 16.0, Weight::Regular, k);
    for (r, s) in &h.status {
        c.text_fit(&st, band(local(f, o, *r)), s, ink.muted);
    }
    c.text_fit(&font(Typeface::JetBrainsMono, 24.0, Weight::Medium, k), band(local(f, o, h.clock)), &l.clock, ink.text);
    c
}

/// The footer image: the bar at the board's bottom with its hints (each a button; the hovered one's word in accent).
pub fn draw_footer(l: &FooterLook, o: Rect, ink: &Ink) -> Canvas {
    let f = &l.frame;
    let k = f.k as f32;
    let mut c = Canvas::new(o.w, o.h);
    let bar = local(f, o, Rect { x: 0, y: l.bh - lay::FOOTER_H, w: l.bw, h: lay::FOOTER_H });
    let top = bar.y.max(0.0) as i32;
    c.fill(0, top, o.w, o.h - top, alpha(ink.panel, 0.72));
    c.fill(0, top, o.w, k.max(1.0).round() as i32, ink.line);
    let words = font(Typeface::Figtree, 17.0, Weight::Regular, k);
    for p in &l.hints {
        let h = &p.hint;
        let s = local(f, o, p.sign);
        if h.round {
            let (cx, cy, r) = (s.x + s.w / 2.0, s.y + s.h / 2.0, s.w / 2.0);
            let bold = font(Typeface::Figtree, 17.0, Weight::Bold, k);
            if h.filled {
                c.disc(cx, cy, r, ink.accent);
                c.text_mid(&bold, band(s), h.sign, ink.on_accent);
            } else {
                c.ring(cx, cy, r - k, 2.0 * k, ink.muted);
                c.text_mid(&bold, band(s), h.sign, ink.text);
            }
        } else {
            c.rrect_line(s, 8.0 * k, 2.0 * k, ink.muted);
            c.text_mid(&font(Typeface::Figtree, 14.0, Weight::Bold, k), band(s), h.sign, ink.text);
        }
        let ww = crate::sofa_draw::measure(h.word, Text::Hint);
        let wr = Rect { x: p.sign.x + p.sign.w + 10, y: p.rect.y - 4, w: ww, h: p.rect.h + 8 };
        let hot = l.hover.as_deref() == Some(h.id);
        c.text_fit(&words, band(local(f, o, wr)), h.word, if hot { ink.accent } else { ink.text });
        let rest = Rect { x: wr.x + ww + 10, y: wr.y, w: p.rect.x + p.rect.w - (wr.x + ww + 10) + 8, h: wr.h };
        c.text_fit(&words, band(local(f, o, rest)), h.ways, ink.muted);
    }
    c
}

/// The quick menu's frame (§2.4; GM-6c fills it): the panel at the right, full height, 620 board px, panel at 86 %
/// with the lifted ring's line at its left, its title and the clock, the closing hint at its foot.
pub fn draw_quick(f: &Frame, bw: i32, bh: i32, clock: &str, o: Rect, ink: &Ink) -> Canvas {
    let k = f.k as f32;
    let mut c = Canvas::new(o.w, o.h);
    c.fill(0, 0, o.w, o.h, alpha(ink.panel, 0.86));
    c.fill(0, 0, k.max(1.0).round() as i32, o.h, ink.lifted_ring);
    let x = bw - lay::QUICK_W + 48;
    let w = lay::QUICK_W - 96;
    let title = local(f, o, Rect { x, y: 48, w, h: 52 });
    c.text_fit(&font(Typeface::Bricolage, 40.0, Weight::SemiBold, k), band(title), "Schnellmenü", ink.text);
    c.text_right(&font(Typeface::JetBrainsMono, 26.0, Weight::Medium, k), band(title), clock, ink.muted);
    let foot = local(f, o, Rect { x, y: bh - 40 - 24, w, h: 24 });
    c.text_mid(&font(Typeface::Figtree, 17.0, Weight::Regular, k), band(foot), QUICK_HINT, ink.muted);
    c
}

pub const QUICK_HINT: &str = "Schließen: B · Esc · Klick daneben";
