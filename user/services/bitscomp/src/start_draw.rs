//! Drawing the start page of `start` (Start.dc.html, WY-5) into one image of the work area.

use crate::canvas::Canvas;
use crate::chrome_draw::{Px, font};
use crate::layout::Rect;
use crate::start::{Page, Part};
use crate::status::Block;
use crate::text::Weight;
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq)]
pub struct StartLook {
    pub work: Rect,
    pub page: Page,
    pub theme: u64,
    pub date: String,
    pub greet: String,
    /// "Arbeitsraum 1 · 3 Kacheln, 5 Tabs · gestern 22:14" when a session is offered.
    pub restore: Option<String>,
    /// (name, address) of each favourite tile.
    pub favs: Vec<(String, String)>,
    /// (kind, name, when) of each recent row.
    pub recent: Vec<(String, String, String)>,
    /// (title, big value, small line).
    pub blocks: Vec<(String, String, String)>,
    pub hover: Option<Part>,
}

/// The start page's building blocks from the status values (real ones only).
pub fn blocks(status: &[Block], cpus: usize) -> Vec<(String, String, String)> {
    let get = |l: &str| status.iter().find(|b| b.label == l).map(|b| b.value.clone());
    let mut v = Vec::new();
    if let Some(cpu) = get("CPU") {
        let mut small = if cpus > 0 { format!("{cpus} Kerne") } else { String::new() };
        if let Some(t) = get("Temperatur") {
            small = if small.is_empty() { t } else { format!("{small} · {t}") };
        }
        v.push(("Rechner".to_string(), cpu, small));
    }
    if let Some(n) = get("Netz") {
        v.push(("Netz".to_string(), n, "Verbindung".to_string()));
    }
    v
}

pub fn draw(l: &StartLook, t: &Theme) -> Canvas {
    let p = Px::new(l.work, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.bg);
    let pg = &l.page;
    let label = font(t, 13.0, Weight::SemiBold).spaced(0.14);
    c.text_fit(&label, p.r(pg.date), &l.date, t.accent);
    c.text_fit(&font(t, 40.0, Weight::Light), p.r(pg.greet), &l.greet, t.text);
    let card = |c: &mut Canvas, r: Rect, lit: bool, hover: bool| {
        let b = p.b(r);
        if lit {
            c.glow(b, p.v(16.0), if t.effects_gpu { 6 } else { 2 }, t.glow);
        }
        c.rrect(b, p.v(16.0), if hover { t.panel2 } else { t.panel });
        c.rrect_line(b, p.v(16.0), p.line(), if lit { t.accent } else { t.line });
    };
    let (title, sub) = (font(t, 22.0, Weight::SemiBold), font(t, 14.0, Weight::Regular));
    let text_of = |c: &mut Canvas, r: Rect, a: &str, b: &str, dim: bool| {
        let x = r.x + 28 + 54 + 20;
        let band = Rect { x, w: r.x + r.w - 28 - x, ..r };
        let (t1, t2) = (Rect { y: r.y + 24, h: 30, ..band }, Rect { y: r.y + 58, h: 20, ..band });
        c.text_fit(&title, p.r(t1), a, if dim { t.muted } else { t.text });
        c.text_fit(&sub, p.r(t2), b, t.muted);
    };
    let icon = |r: Rect| p.r(Rect { x: r.x + 28, y: r.y + 24, w: 54, h: 54 });
    let offered = l.restore.is_some();
    card(&mut c, pg.restore, offered, offered && l.hover == Some(Part::Restore));
    let ic = icon(pg.restore);
    let (ix, iy, k) = (ic.x as f32, ic.y as f32, ic.w as f32 / 54.0);
    c.ring(ix + 27.0 * k, iy + 27.0 * k, 24.0 * k, 1.5 * k, crate::theme::alpha(if offered { t.accent } else { t.muted }, 0.5));
    c.poly(&[(ix + 20.0 * k, iy + 17.0 * k), (ix + 38.0 * k, iy + 27.0 * k), (ix + 20.0 * k, iy + 37.0 * k)], if offered { t.accent } else { t.muted });
    match &l.restore {
        Some(s) => text_of(&mut c, pg.restore, "Letzte Sitzung wiederherstellen", s, false),
        None => text_of(&mut c, pg.restore, "Letzte Sitzung wiederherstellen", "Keine gespeicherte Sitzung", true),
    }
    card(&mut c, pg.new, false, l.hover == Some(Part::New));
    let ic = icon(pg.new);
    let (ix, iy, k) = (ic.x as f32, ic.y as f32, ic.w as f32 / 54.0);
    c.ring(ix + 27.0 * k, iy + 27.0 * k, 24.0 * k, 1.5 * k, t.muted);
    c.line((ix + 27.0 * k, iy + 17.0 * k), (ix + 27.0 * k, iy + 37.0 * k), 2.5 * k, t.text);
    c.line((ix + 17.0 * k, iy + 27.0 * k), (ix + 37.0 * k, iy + 27.0 * k), 2.5 * k, t.text);
    text_of(&mut c, pg.new, "Neue Sitzung", "Ein leerer Arbeitsraum mit einem Terminal", false);

    let head = font(t, 13.0, Weight::SemiBold).spaced(0.14);
    c.text_fit(&head, p.r(pg.fav_head), "FAVORITEN", t.muted);
    let (mark, name, small) = (font(t, 15.0, Weight::SemiBold), font(t, 15.0, Weight::Medium), font(t, 12.0, Weight::Regular));
    for (i, (r, (n, a))) in pg.favs.iter().zip(&l.favs).enumerate() {
        let b = p.b(*r);
        c.rrect(b, p.v(12.0), if l.hover == Some(Part::Fav(i)) { t.panel2 } else { t.panel });
        c.rrect_line(b, p.v(12.0), p.line(), if l.hover == Some(Part::Fav(i)) { t.accent } else { t.line });
        let m = Rect { x: r.x + 16, y: r.y + 16, w: 32, h: 32 };
        c.rrect(p.b(m), p.v(8.0), t.chip);
        let letter: String = n.chars().next().map(|ch| ch.to_uppercase().collect()).unwrap_or_default();
        c.text_mid(&mark, p.r(m), &letter, t.accent);
        let inner = Rect { x: r.x + 16, w: r.w - 32, ..*r };
        c.text_fit(&name, p.r(Rect { y: r.y + 56, h: 22, ..inner }), n, t.text);
        c.text_fit(&small, p.r(Rect { y: r.y + 80, h: 16, ..inner }), a, t.muted);
    }
    c.text_fit(&head, p.r(pg.recent_head), "ZULETZT BENUTZT", t.muted);
    if !pg.recents.is_empty() {
        let first = pg.recents[0];
        let last = pg.recents[pg.recents.len().min(l.recent.len()).max(1) - 1];
        let frame = Rect { x: first.x, y: first.y, w: first.w, h: last.y + last.h - first.y };
        let fb = p.b(frame);
        c.rrect(fb, p.v(12.0), t.panel);
        let kind = font(t, 11.0, Weight::SemiBold).spaced(0.08);
        for (i, (r, (k, n, when))) in pg.recents.iter().zip(&l.recent).enumerate() {
            let rr = p.r(*r);
            if l.hover == Some(Part::Recent(i)) {
                c.fill(rr.x, rr.y, rr.w, rr.h, t.panel2);
            }
            if i + 1 < l.recent.len().min(pg.recents.len()) {
                c.fill(rr.x, rr.y + rr.h - 1, rr.w, p.line().round() as i32, t.line);
            }
            let inner = Rect { x: r.x + 16, w: r.w - 32, ..*r };
            c.text_fit(&kind, p.r(Rect { w: 96, ..inner }), k, t.muted);
            let ww = small.width(when);
            let nr = p.r(Rect { x: inner.x + 108, w: inner.w - 108, ..inner });
            c.text_fit(&font(t, 15.0, Weight::Regular), Rect { w: nr.w - ww - p.v(10.0) as i32, ..nr }, n, t.text);
            c.text_right(&small, nr, when, t.muted);
        }
        if l.recent.is_empty() {
            c.text_mid(&small, p.r(first), "Noch nichts benutzt", t.muted);
        }
        c.rrect_line(fb, p.v(12.0), p.line(), t.line);
    }
    if !pg.blocks.is_empty() && !l.blocks.is_empty() {
        c.text_fit(&head, p.r(pg.blocks_head), "BAUSTEINE", t.muted);
        let big = font(t, 26.0, Weight::SemiBold);
        for (r, (ti, v, sm)) in pg.blocks.iter().zip(&l.blocks) {
            let b = p.b(*r);
            c.rrect(b, p.v(12.0), t.panel);
            c.rrect_line(b, p.v(12.0), p.line(), t.line);
            let inner = Rect { x: r.x + 14, w: r.w - 28, ..*r };
            c.text_fit(&small, p.r(Rect { y: r.y + 14, h: 16, ..inner }), ti, t.muted);
            c.text_fit(&big, p.r(Rect { y: r.y + 34, h: 34, ..inner }), v, t.text);
            c.text_fit(&small, p.r(Rect { y: r.y + 70, h: 16, ..inner }), sm, t.muted);
        }
    }
    c
}
