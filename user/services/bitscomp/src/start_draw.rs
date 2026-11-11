//! Drawing the start page of `start` (Start.dc.html, WY-5) into one image of the work area. DS-2: no ground of
//! its own (the wallpaper shows through), the cards are glass with the design's shadows, the lit card glows softly,
//! and the two big cards carry the HUD brackets. Three images, so that a hover or the status values never draw the whole
//! page again (about 50 ms on the CPU, Georg 5.11.): the page itself, the building blocks, and the one part under the pointer.

use crate::canvas::Canvas;
use crate::chrome_draw::{Px, font};
use crate::deco::hud::{self, Glass};
use crate::layout::Rect;
use crate::start::{Page, Part};
use crate::status::Block;
use crate::text::Weight;
use crate::theme::{Theme, alpha};

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

/// The page's look with neither the pointer nor the blocks' values: what `draw` depends on.
pub fn base_look(l: &StartLook) -> StartLook {
    let blocks = l.blocks.iter().map(|_| (String::new(), String::new(), String::new())).collect();
    StartLook { hover: None, blocks, ..l.clone() }
}

/// What `draw_blocks` depends on.
pub fn blocks_look(l: &StartLook) -> StartLook {
    StartLook { hover: None, ..l.clone() }
}

/// What `draw_hover` depends on.
pub fn hover_look(l: &StartLook) -> StartLook {
    StartLook { blocks: base_look(l).blocks, ..l.clone() }
}

/// Where the blocks' image lies; None: no blocks shown.
pub fn blocks_rect(l: &StartLook) -> Option<Rect> {
    let n = l.blocks.len().min(l.page.blocks.len());
    let last = *l.page.blocks[..n].last()?;
    let head = l.page.blocks_head;
    let (x, y) = (head.x.min(l.page.blocks[0].x), head.y);
    Some(Rect { x, y, w: last.x + last.w - x, h: last.y + last.h - y })
}

/// Where the hovered part's image lies; None: the pointer is on no part (or an offered card is not offered).
pub fn hover_rect(l: &StartLook) -> Option<Rect> {
    let pg = &l.page;
    match l.hover? {
        Part::Restore if l.restore.is_some() => Some(pg.restore),
        Part::Restore => None,
        Part::New => Some(pg.new),
        Part::Fav(i) if i < l.favs.len() => pg.favs.get(i).copied(),
        Part::Recent(i) if i < l.recent.len() => pg.recents.get(i).copied(),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Layer {
    Base,
    Blocks,
    Hover,
}

/// The page without the blocks' cards and the pointer.
pub fn draw(l: &StartLook, t: &Theme) -> Canvas {
    paint(l, t, l.work, Layer::Base)
}

/// The blocks and their heading, an image of `at`.
pub fn draw_blocks(l: &StartLook, t: &Theme, at: Rect) -> Canvas {
    paint(l, t, at, Layer::Blocks)
}

/// The hovered part, an image of `at`, over the page's own picture of it.
pub fn draw_hover(l: &StartLook, t: &Theme, at: Rect) -> Canvas {
    paint(l, t, at, Layer::Hover)
}

fn paint(l: &StartLook, t: &Theme, at: Rect, layer: Layer) -> Canvas {
    let p = Px::new(at, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let pg = &l.page;
    let base = layer == Layer::Base;
    let shows = |part: Part| base || layer == Layer::Hover && l.hover == Some(part);
    // The shadows first, so none falls on a card drawn before it: lifted under the two big cards, raised under
    // the smaller ones (CSS paints each only outside its own box).
    if base {
        for r in [pg.restore, pg.new] {
            hud::shadow(&mut c, p.b(r), p.v(16.0), &t.shadow_lifted, t);
        }
    }
    let recent_frame = (!pg.recents.is_empty()).then(|| {
        let first = pg.recents[0];
        let last = pg.recents[pg.recents.len().min(l.recent.len()).max(1) - 1];
        Rect { x: first.x, y: first.y, w: first.w, h: last.y + last.h - first.y }
    });
    let blocks_shown = !pg.blocks.is_empty() && !l.blocks.is_empty();
    let small_cards = pg.favs.iter().take(l.favs.len()).chain(recent_frame.iter());
    if base {
        for r in small_cards.chain(pg.blocks.iter().take(if blocks_shown { l.blocks.len() } else { 0 })) {
            hud::shadow(&mut c, p.b(*r), p.v(12.0), &t.shadow_raised, t);
        }
    }
    let fill = |hover: bool| if hover { alpha(t.panel2, 0.85) } else { hud::glass(t, Glass::Card) };
    let label = font(t, 13.0, Weight::SemiBold).spaced(0.14);
    if base {
        c.text_fit(&label, p.r(pg.date), &l.date, t.accent);
        c.text_fit(&font(t, 40.0, Weight::Light), p.r(pg.greet), &l.greet, t.text);
    }
    let card = |c: &mut Canvas, r: Rect, lit: bool, hover: bool| {
        let b = p.b(r);
        if lit {
            hud::glow_soft(c, b, p.v(16.0), t);
            c.glow(b, p.v(16.0), if t.effects_gpu { 6 } else { 2 }, t.glow);
        }
        c.rrect(b, p.v(16.0), fill(hover));
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
    if shows(Part::Restore) {
        card(&mut c, pg.restore, offered, offered && l.hover == Some(Part::Restore));
        let ic = icon(pg.restore);
        let (ix, iy, k) = (ic.x as f32, ic.y as f32, ic.w as f32 / 54.0);
        c.ring(ix + 27.0 * k, iy + 27.0 * k, 24.0 * k, 1.5 * k, crate::theme::alpha(if offered { t.accent } else { t.muted }, 0.5));
        c.poly(&[(ix + 20.0 * k, iy + 17.0 * k), (ix + 38.0 * k, iy + 27.0 * k), (ix + 20.0 * k, iy + 37.0 * k)], if offered { t.accent } else { t.muted });
        match &l.restore {
            Some(s) => text_of(&mut c, pg.restore, "Letzte Sitzung wiederherstellen", s, false),
            None => text_of(&mut c, pg.restore, "Letzte Sitzung wiederherstellen", "Keine gespeicherte Sitzung", true),
        }
    }
    if shows(Part::New) {
        card(&mut c, pg.new, false, l.hover == Some(Part::New));
        let ic = icon(pg.new);
        let (ix, iy, k) = (ic.x as f32, ic.y as f32, ic.w as f32 / 54.0);
        c.ring(ix + 27.0 * k, iy + 27.0 * k, 24.0 * k, 1.5 * k, t.muted);
        c.line((ix + 27.0 * k, iy + 17.0 * k), (ix + 27.0 * k, iy + 37.0 * k), 2.5 * k, t.text);
        c.line((ix + 17.0 * k, iy + 27.0 * k), (ix + 37.0 * k, iy + 27.0 * k), 2.5 * k, t.text);
        text_of(&mut c, pg.new, "Neue Sitzung", "Ein leerer Arbeitsraum mit einem Terminal", false);
    }
    // The HUD brackets around the pair: the start page's stage.
    let (a, b) = (p.b(pg.restore), p.b(pg.new));
    let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
    let stage = crate::canvas::Box2 { x: x0, y: y0, w: (a.x + a.w).max(b.x + b.w) - x0, h: (a.y + a.h).max(b.y + b.h) - y0 };
    if base {
        hud::brackets(&mut c, stage, t);
    }

    let head = font(t, 13.0, Weight::SemiBold).spaced(0.14);
    if base {
        c.text_fit(&head, p.r(pg.fav_head), "FAVORITEN", t.muted);
    }
    let (mark, name, small) = (font(t, 15.0, Weight::SemiBold), font(t, 15.0, Weight::Medium), font(t, 12.0, Weight::Regular));
    for (i, (r, (n, a))) in pg.favs.iter().zip(&l.favs).enumerate() {
        if !shows(Part::Fav(i)) {
            continue;
        }
        let b = p.b(*r);
        c.rrect(b, p.v(12.0), fill(l.hover == Some(Part::Fav(i))));
        c.rrect_line(b, p.v(12.0), p.line(), if l.hover == Some(Part::Fav(i)) { t.accent } else { t.line });
        let m = Rect { x: r.x + 16, y: r.y + 16, w: 32, h: 32 };
        c.rrect(p.b(m), p.v(8.0), t.chip);
        let letter: String = n.chars().next().map(|ch| ch.to_uppercase().collect()).unwrap_or_default();
        c.text_mid(&mark, p.r(m), &letter, t.accent);
        let inner = Rect { x: r.x + 16, w: r.w - 32, ..*r };
        c.text_fit(&name, p.r(Rect { y: r.y + 56, h: 22, ..inner }), n, t.text);
        c.text_fit(&small, p.r(Rect { y: r.y + 80, h: 16, ..inner }), a, t.muted);
    }
    if base {
        c.text_fit(&head, p.r(pg.recent_head), "ZULETZT BENUTZT", t.muted);
    }
    if let Some(frame) = recent_frame.filter(|_| layer != Layer::Blocks) {
        let first = pg.recents[0];
        let fb = p.b(frame);
        if base {
            c.rrect(fb, p.v(12.0), fill(false));
        }
        let kind = font(t, 11.0, Weight::SemiBold).spaced(0.08);
        for (i, (r, (k, n, when))) in pg.recents.iter().zip(&l.recent).enumerate() {
            if !shows(Part::Recent(i)) {
                continue;
            }
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
        if base {
            if l.recent.is_empty() {
                c.text_mid(&small, p.r(first), "Noch nichts benutzt", t.muted);
            }
            c.rrect_line(fb, p.v(12.0), p.line(), t.line);
        }
    }
    if blocks_shown && layer == Layer::Blocks {
        c.text_fit(&head, p.r(pg.blocks_head), "BAUSTEINE", t.muted);
        let big = font(t, 26.0, Weight::SemiBold);
        for (r, (ti, v, sm)) in pg.blocks.iter().zip(&l.blocks) {
            let b = p.b(*r);
            c.rrect(b, p.v(12.0), fill(false));
            c.rrect_line(b, p.v(12.0), p.line(), t.line);
            let inner = Rect { x: r.x + 14, w: r.w - 28, ..*r };
            c.text_fit(&small, p.r(Rect { y: r.y + 14, h: 16, ..inner }), ti, t.muted);
            c.text_fit(&big, p.r(Rect { y: r.y + 34, h: 34, ..inner }), v, t.text);
            c.text_fit(&small, p.r(Rect { y: r.y + 70, h: 16, ..inner }), sm, t.muted);
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look() -> StartLook {
        let r = |x, y| Rect { x, y, w: 100, h: 50 };
        let page = Page {
            date: r(0, 0),
            greet: r(0, 0),
            restore: r(10, 10),
            new: r(120, 10),
            fav_head: r(0, 0),
            favs: vec![r(10, 70)],
            recent_head: r(0, 0),
            recents: vec![r(120, 70)],
            blocks_head: r(10, 130),
            blocks: vec![r(10, 150), r(120, 150)],
        };
        StartLook {
            work: Rect { x: 0, y: 0, w: 300, h: 300 },
            page,
            theme: 1,
            date: String::new(),
            greet: String::new(),
            restore: None,
            favs: vec![("T".into(), "a".into())],
            recent: vec![("k".into(), "n".into(), "w".into())],
            blocks: vec![("CPU".into(), "1 %".into(), "16".into()), ("Netz".into(), "Kabel".into(), "".into())],
            hover: None,
        }
    }

    #[test]
    fn neither_the_pointer_nor_a_value_draws_the_page_again() {
        let a = look();
        let mut b = look();
        b.hover = Some(Part::Fav(0));
        b.blocks[0].1 = "99 %".into();
        assert_eq!(base_look(&a), base_look(&b));
        assert_ne!(blocks_look(&a), blocks_look(&b), "the blocks follow their values");
        assert_ne!(hover_look(&a), hover_look(&b), "the part follows the pointer");
        let mut c = b.clone();
        c.blocks[0].1 = "2 %".into();
        assert_eq!(hover_look(&b), hover_look(&c), "a value leaves the hovered part alone");
    }

    #[test]
    fn the_layers_lie_on_their_parts() {
        let mut l = look();
        assert_eq!(blocks_rect(&l), Some(Rect { x: 10, y: 130, w: 210, h: 70 }));
        assert_eq!(hover_rect(&l), None);
        l.hover = Some(Part::Fav(0));
        assert_eq!(hover_rect(&l), Some(l.page.favs[0]));
        l.hover = Some(Part::Fav(1));
        assert_eq!(hover_rect(&l), None, "no such favourite");
        l.hover = Some(Part::Restore);
        assert_eq!(hover_rect(&l), None, "nothing offered, nothing lights up");
        l.blocks.clear();
        assert_eq!(blocks_rect(&l), None);
    }
}
