//! SYS-A: the program "System"'s look, after the design (plan/spec/system-einstellungen, "Absicht dunkel"): the
//! page's ground, cards with a thin ring, the mono uppercase captions, the big Bricolage numbers, pill tabs (the
//! chosen one on the accent's soft tone). Theme tokens only, so light mode follows. Redrawn when its look changes
//! (once a second while values move); the task manager and the device list are in sys_draw2.

use crate::canvas::Canvas;
use crate::chrome_draw::{Px, font, wrap};
use crate::layout::Rect;
use crate::page_confirm::Dialog;
use crate::page_draw::FieldLook;
use crate::sys_chart::{self, colour};
use crate::sys_layout::Layout;
use crate::sys_model::{Item, TabModel, Tone};
use crate::sys_ring::Range;
use crate::sys_view::{SysPart, Tab};
use crate::text::{Family, Font, Weight};
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq)]
pub struct SysLook {
    pub layout: Layout,
    pub model: TabModel,
    pub theme: u64,
    pub address: String,
    pub tab: Tab,
    pub range: Range,
    pub hover: Option<SysPart>,
    /// The dialog's hovered button.
    pub dhover: Option<crate::page::Part>,
    pub field: Option<FieldLook>,
    pub msg: Option<(String, bool)>,
    pub dialog: Option<Dialog>,
}

pub struct Fonts {
    pub label: Font,
    pub body: Font,
    pub small: Font,
    pub mono: Font,
    pub big: Font,
    pub mid: Font,
    pub ctl: Font,
}

impl Fonts {
    pub fn new(t: &Theme) -> Fonts {
        Fonts {
            label: t.font(Family::Mono, 11.0, Weight::Medium).spaced(0.08),
            body: font(t, 14.0, Weight::Regular),
            small: font(t, 13.0, Weight::Regular),
            mono: t.font(Family::Mono, 13.0, Weight::Regular),
            big: t.font(Family::Display, 40.0, Weight::Medium),
            mid: t.font(Family::Display, 26.0, Weight::Medium),
            ctl: font(t, 14.0, Weight::Medium),
        }
    }
}

/// A pill (tab, filter, range): the chosen one on the accent's soft tone with an accent ring.
pub fn pill(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, text: &str, on: bool, hover: bool, f: &Font) {
    let b = p.b(r);
    let rad = p.v(r.h as f32 / 2.0);
    if on {
        c.rrect(b, rad, t.accent_soft);
        c.rrect_line(b, rad, p.line(), crate::theme::alpha(t.accent, 0.45));
    } else if hover {
        c.rrect(b, rad, t.chip);
    }
    c.text_mid(f, p.r(r), text, if on { t.accent } else if hover { t.text } else { t.muted });
}

pub fn card_bg(c: &mut Canvas, p: &Px, t: &Theme, r: Rect) {
    let b = p.b(r);
    c.rrect(b, p.v(24.0), t.panel);
    c.rrect_line(b, p.v(24.0), p.line(), t.line);
}

/// A button: outlined, or filled in the accent (`primary`); a disabled one is muted.
pub fn button(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, text: &str, primary: bool, live: bool, hover: bool, f: &Font) {
    let b = p.b(r);
    let rad = p.v(12.0);
    if primary && live {
        c.rrect(b, rad, if hover { crate::theme::alpha(t.accent, 0.85) } else { t.accent });
        c.text_mid(f, p.r(r), text, t.on_accent);
    } else {
        c.rrect(b, rad, if hover && live { t.chip } else { t.panel });
        c.rrect_line(b, rad, p.line(), if live { t.line_strong } else { t.line });
        c.text_mid(f, p.r(r), text, if live { t.text } else { t.muted });
    }
}

/// The page's mark: a ring with the accent's dot.
fn mark(c: &mut Canvas, p: &Px, t: &Theme, r: Rect) {
    let b = p.r(r);
    let (cx, cy) = (b.x as f32 + b.w as f32 / 2.0, b.y as f32 + b.h as f32 / 2.0);
    c.ring(cx, cy, p.v(13.0), p.v(3.0), t.text);
    c.disc(cx, cy, p.v(4.0), t.accent);
}

fn legend(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, items: &[(Tone, String)], f: &Font, right: bool) {
    let b = p.r(r);
    let dot = p.v(4.0);
    let gap = p.v(16.0) as i32;
    let ws: Vec<i32> = items.iter().map(|(_, s)| f.width(s) + (dot * 3.0) as i32).collect();
    let total: i32 = ws.iter().sum::<i32>() + gap * (items.len() as i32 - 1).max(0);
    let mut x = if right { b.x + b.w - total } else { b.x };
    for ((k, s), w) in items.iter().zip(ws) {
        let cy = b.y as f32 + b.h as f32 / 2.0;
        if *k != Tone::Muted {
            c.disc(x as f32 + dot, cy, dot, colour(t, *k));
        }
        let tx = if *k == Tone::Muted { x } else { x + (dot * 3.0) as i32 };
        c.text(f, tx, b.y + (b.h - f.height()) / 2, s, t.muted);
        x += w + gap;
    }
}

/// A bar of segments (shares of 1) on its track.
fn bar(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, segs: &[(f64, Tone)]) {
    let b = p.b(r);
    let rad = b.h / 2.0;
    c.rrect(b, rad, t.panel2);
    let mut x = b.x;
    for (share, k) in segs {
        let w = (b.w * *share as f32).min(b.x + b.w - x);
        if w <= 0.5 {
            continue;
        }
        c.rrect(crate::canvas::Box2 { x, y: b.y, w, h: b.h }, rad.min(w / 2.0), colour(t, *k));
        x += w;
    }
}

pub fn draw_items(c: &mut Canvas, p: &Px, t: &Theme, f: &Fonts, items: &[Item], rects: &[Rect], hover: Option<SysPart>, clip: Rect) {
    for (it, r) in items.iter().zip(rects) {
        if r.y + r.h < clip.y || r.y > clip.y + clip.h {
            continue;
        }
        let r = *r;
        match it {
            Item::Label(s) => {
                c.text_fit(&f.label, p.r(r), &s.to_uppercase(), t.muted);
            }
            Item::LabelRight(s, leg) => {
                let lw = f.label.width(&s.to_uppercase());
                c.text_fit(&f.label, p.r(r), &s.to_uppercase(), t.muted);
                let room = Rect { x: r.x + (lw as f64 / t.scale) as i32 + 16, w: r.w - (lw as f64 / t.scale) as i32 - 16, ..r };
                legend(c, p, t, room, leg, &f.small, true);
            }
            Item::Big(v, sub) => {
                let b = p.r(r);
                let w = c.text_fit(&f.big, b, v, t.text);
                if !sub.is_empty() {
                    let sb = Rect { x: b.x + w + p.v(12.0) as i32, y: b.y + (f.big.height() - f.small.height()) / 2 + p.v(6.0) as i32, w: (b.w - w - p.v(12.0) as i32).max(0), h: f.small.height() };
                    c.text_fit(&f.small, sb, sub, t.muted);
                }
            }
            Item::Mid(v) => {
                c.text_fit(&f.mid, p.r(r), v, t.text);
            }
            Item::Sub(s) => {
                let b = p.r(r);
                let lines = wrap(&f.small, s, b.w);
                let lh = (b.h / lines.len().max(1) as i32).max(1);
                for (k, l) in lines.iter().take(3).enumerate() {
                    c.text_fit(&f.small, Rect { y: b.y + k as i32 * lh, h: lh, ..b }, l, t.muted);
                }
            }
            Item::Row(k, v) | Item::RowTone(k, v, _) => {
                let b = p.r(r);
                let col = if let Item::RowTone(_, _, tone) = it { colour(t, *tone) } else { t.text };
                let vw = f.body.width(v).min(b.w * 3 / 5);
                c.text_right(&f.body, Rect { x: b.x + b.w - vw, w: vw, ..b }, v, col);
                c.text_fit(&f.small, Rect { w: b.w - vw - p.v(12.0) as i32, ..b }, k, t.muted);
            }
            Item::Bar(segs) => bar(c, p, t, r, segs),
            Item::Legend(items) => legend(c, p, t, r, items, &f.small, false),
            Item::Chart(ch) => sys_chart::draw(c, p, t, r, ch),
            Item::Threads(v) => sys_chart::threads(c, p, t, r, v, &f.mono),
            Item::Grid(cells) => {
                let cw = (r.w - 14) / 2;
                for (k, (sub, v)) in cells.iter().enumerate() {
                    let (row, col) = (k / 2, k % 2);
                    let x = r.x + col as i32 * (cw + 14);
                    let y = r.y + row as i32 * 52;
                    c.text_fit(&f.small, p.r(Rect { x, y, w: cw, h: 18 }), sub, t.muted);
                    c.text_fit(&f.mid, p.r(Rect { x, y: y + 18, w: cw, h: 32 }), v, t.text);
                }
            }
            Item::Link(s, part) => {
                let on = hover == Some(*part);
                c.text_fit(&f.ctl, p.r(r), &format!("{s} →"), if on { t.text } else { t.accent });
            }
            Item::Buttons(bs) => {
                let n = bs.len().max(1) as i32;
                let bw = (r.w - (n - 1) * 10) / n;
                for (k, (label, part, on)) in bs.iter().enumerate() {
                    let br = Rect { x: r.x + k as i32 * (bw + 10), w: bw, ..r };
                    button(c, p, t, br, label, false, *on, hover == Some(*part), &f.ctl);
                }
            }
            Item::Rule => {
                let b = p.r(r);
                c.fill(b.x, b.y, b.w, p.line().round() as i32, t.line);
            }
            Item::Fill => {}
        }
    }
}

/// What the frame (ground and cards) was drawn from: it is kept while only values change.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameKey {
    content: Rect,
    cards: Vec<Rect>,
    theme: u64,
    scale: u64,
}

fn card_rects(l: &Layout) -> Vec<Rect> {
    let mut v = l.cards.clone();
    if let Some(p) = &l.procs {
        v.extend([p.table, p.detail]);
    }
    if let Some(d) = &l.devs {
        v.extend([d.list, d.detail, d.facts]);
    }
    v.retain(|r| r.y + r.h >= l.body.y && r.y <= l.body.y + l.body.h);
    v
}

fn frame_key(l: &SysLook, t: &Theme) -> FrameKey {
    FrameKey { content: l.layout.content, cards: card_rects(&l.layout), theme: l.theme, scale: t.scale.to_bits() }
}

/// The ground and the cards: the rounded cards are most of a redraw's cost, and they move only with the layout.
fn frame(l: &SysLook, t: &Theme) -> Canvas {
    let p = Px::new(l.layout.content, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.bg);
    for r in card_rects(&l.layout) {
        card_bg(&mut c, &p, t, r);
    }
    c
}

/// The page over a kept frame (a copy of it), drawn anew only when the layout or the theme changed.
pub fn draw_cached(l: &SysLook, t: &Theme, keep: &mut Option<(FrameKey, Canvas)>) -> Canvas {
    let key = frame_key(l, t);
    if keep.as_ref().is_none_or(|(k, _)| *k != key) {
        *keep = Some((key, frame(l, t)));
    }
    let mut c = keep.as_ref().map(|(_, c)| c.clone()).unwrap_or_else(|| frame(l, t));
    over(&mut c, l, t);
    c
}

pub fn draw(l: &SysLook, t: &Theme) -> Canvas {
    let mut c = frame(l, t);
    over(&mut c, l, t);
    c
}

fn over(c: &mut Canvas, l: &SysLook, t: &Theme) {
    let lay = &l.layout;
    let p = Px::new(lay.content, t.scale);
    let f = Fonts::new(t);
    // The body first; the head is drawn over what scrolled under it.
    for (k, r) in lay.cards.iter().enumerate() {
        if r.y + r.h < lay.body.y || r.y > lay.body.y + lay.body.h {
            continue;
        }
        if let (Some(items), Some(rects)) = (l.model.rows.iter().flat_map(|row| row.cards.iter()).nth(k).map(|cd| &cd.items), lay.items.get(k)) {
            draw_items(c, &p, t, &f, items, rects, l.hover, lay.body);
        }
    }
    if let (Some(pr), Some(pm)) = (&lay.procs, &l.model.procs) {
        crate::sys_draw2::procs(c, &p, t, &f, pr, pm, l.hover, l.field.as_ref());
    }
    if let (Some(dl), Some(dm)) = (&lay.devs, &l.model.devices) {
        crate::sys_draw2::devices(c, &p, t, &f, dl, dm, l.hover);
    }
    let hb = p.r(lay.head);
    c.fill(hb.x, hb.y, hb.w, hb.h, t.bg);
    mark(c, &p, t, lay.mark);
    c.text_fit(&t.font(Family::Display, 30.0, Weight::SemiBold), p.r(lay.title), "System", t.text);
    c.text_fit(&f.mono, p.r(lay.addr), &l.address, t.muted);
    if let (Some(r), Some((s, tone))) = (lay.status, &l.model.status) {
        let b = p.b(r);
        c.rrect(b, b.h / 2.0, t.panel);
        c.rrect_line(b, b.h / 2.0, p.line(), t.line);
        let pr = p.r(r);
        c.disc(pr.x as f32 + p.v(18.0), pr.y as f32 + pr.h as f32 / 2.0, p.v(4.5), colour(t, *tone));
        c.text_fit(&f.body, Rect { x: pr.x + p.v(30.0) as i32, w: pr.w - p.v(36.0) as i32, ..pr }, s, t.text);
    }
    c.text_right(&f.small, p.r(lay.sub), &l.model.sub, t.muted);
    for (tab, r) in &lay.tabs {
        pill(c, &p, t, *r, tab.name(), *tab == l.tab, l.hover == Some(SysPart::Tab(*tab)), &f.ctl);
    }
    if let Some(b) = lay.range_box {
        let bb = p.b(b);
        c.rrect(bb, bb.h / 2.0, t.panel2);
        for (r, pr) in &lay.ranges {
            pill(c, &p, t, *pr, r.name(), *r == l.range, l.hover == Some(SysPart::Range(*r)), &f.ctl);
        }
    }
    if let Some((m, err)) = &l.msg {
        c.text_fit(&f.small, p.r(lay.msg), m, if *err { t.danger } else { t.accent });
    }
    if let Some(d) = &l.dialog {
        crate::page_confirm::draw(c, &p, t, lay.content, d, l.dhover);
    }
}
