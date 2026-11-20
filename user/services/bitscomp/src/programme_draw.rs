//! PK-7a: the "Programme" page's look (DS rule: new look files), in the settings page's Absicht look: paper
//! ground, panel cards with a line, pills for the tabs (the chosen one in accent), the library's tiles with their
//! icons, the plan card's phases as five steps. Theme tokens only (panel, panel2, chip, accent, warm, danger), so
//! dark and light follow. Redrawn only when its look changes; nothing on it moves by itself.

use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::{Px, font};
use crate::icon_png::Argb;
use crate::layout::Rect;
use crate::library::Icon;
use crate::page::Part;
use crate::page_draw::{FieldLook, button, field, pill};
use crate::programme_layout::{Layout, MORE};
use crate::programme_text::{PHASES, Tone};
use crate::programme_view::{Item, ProgPart};
use crate::text::{Family, Weight};
use crate::theme::{Theme, alpha};

pub const PLACEHOLDER: &str = "Programm suchen, zum Beispiel „foot“ oder „ripgrep“";

#[derive(Clone, Debug, PartialEq)]
pub struct ProgLook {
    pub layout: Layout,
    pub items: Vec<Item>,
    pub theme: u64,
    pub address: String,
    pub tabs: Vec<String>,
    pub tab: usize,
    /// The line under the address: bitspkgd's state.
    pub state: (String, bool),
    pub msg: Option<(String, bool)>,
    pub hover: Option<Part>,
    /// The search text, and the field while it is edited.
    pub query: String,
    pub field: Option<FieldLook>,
    pub dialog: Option<crate::page_confirm::Dialog>,
    pub live: bool,
}

fn tone(t: &Theme, k: Tone) -> crate::theme::Rgba {
    match k {
        Tone::Head | Tone::Text | Tone::Item => t.text,
        Tone::Muted => t.muted,
        Tone::Good => t.accent,
        Tone::Bad => t.danger,
    }
}

pub fn draw(l: &ProgLook, t: &Theme) -> Canvas {
    let lay = &l.layout;
    let p = Px::new(lay.content, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.paper);
    let hover = |q: Part| l.hover == Some(q);
    let (body, small, ctl) = (font(t, 15.0, Weight::Medium), font(t, 12.0, Weight::Regular), font(t, 14.0, Weight::Medium));
    let mono = t.bfont(Family::Mono, 13.0, Weight::Regular);
    let label = font(t, 13.0, Weight::SemiBold).spaced(0.14);
    for r in &lay.cards {
        let b = p.b(*r);
        c.rrect(b, p.v(12.0), t.panel);
        c.rrect_line(b, p.v(12.0), p.line(), t.line);
    }
    let in_view = |r: &Rect| r.y + r.h >= lay.content.y && r.y <= lay.content.y + lay.content.h;
    for (it, r) in l.items.iter().zip(&lay.items) {
        if !in_view(r) {
            continue;
        }
        match it {
            Item::Head(s) => {
                c.text_fit(&label, p.r(*r), s, t.muted);
            }
            Item::Line(s, k) => {
                let (f, band) = match k {
                    Tone::Head => (font(t, 18.0, Weight::SemiBold), *r),
                    Tone::Item => (mono, Rect { x: r.x + 16, w: r.w - 16, ..*r }),
                    Tone::Muted => (small, *r),
                    _ => (body, *r),
                };
                c.text_fit(&f, p.r(band), s, tone(t, *k));
            }
            Item::Row { title, sub, meta, rust, buttons } => {
                let bw: i32 = buttons.iter().map(|(_, b)| crate::programme_layout::button_w(b) + 8).sum();
                let tw = r.w - bw - 12;
                let mw = if meta.is_empty() { 0 } else { (r.w / 4).min(220) };
                c.text_fit(&body, p.r(Rect { x: r.x, y: r.y + 12, w: tw - mw, h: 24 }), title, t.text);
                c.text_fit(&small, p.r(Rect { x: r.x, y: r.y + 38, w: tw, h: 18 }), sub, t.muted);
                if !meta.is_empty() {
                    c.text_right(&small, p.r(Rect { x: r.x + tw - mw, y: r.y + 14, w: mw, h: 18 }), meta, t.muted);
                }
                if *rust {
                    let mark = Rect { x: r.x + tw - mw - 64, y: r.y + 14, w: 52, h: 20 };
                    let b = p.b(mark);
                    c.rrect(b, p.v(10.0), t.accent_soft);
                    c.text_mid(&small, p.r(mark), "Rust", t.accent);
                }
                c.fill(p.r(*r).x, p.r(*r).y + p.r(*r).h - 1, p.r(*r).w, p.line().round() as i32, t.line);
                for (part, b) in buttons {
                    if let Some(rect) = lay.part(*part) {
                        button(&mut c, &p, t, rect, b, hover(Part::Prog(*part)), l.live, &ctl);
                    }
                }
            }
            Item::Buttons(list) => {
                for (part, b, on) in list {
                    if let Some(rect) = lay.part(*part) {
                        let go = matches!(part, ProgPart::CardGo);
                        if go && *on && l.live {
                            pill(&mut c, &p, t, rect, b, true, hover(Part::Prog(*part)), true, &ctl);
                        } else {
                            button(&mut c, &p, t, rect, b, *on && hover(Part::Prog(*part)), *on && l.live, &ctl);
                        }
                    }
                }
            }
            Item::Check(part, s, on) => {
                let bx = Rect { x: r.x, y: r.y + 7, w: 22, h: 22 };
                let b = p.b(bx);
                c.rrect(b, p.v(5.0), if *on { t.accent } else { t.panel2 });
                c.rrect_line(b, p.v(5.0), p.line(), if *on || hover(Part::Prog(*part)) { t.accent } else { t.line_strong });
                if *on {
                    let rr = p.r(bx);
                    let k = p.v(1.0);
                    let (x0, y0) = (rr.x as f32, rr.y as f32);
                    c.line((x0 + 5.0 * k, y0 + 11.0 * k), (x0 + 9.5 * k, y0 + 15.5 * k), p.v(2.2), t.on_accent);
                    c.line((x0 + 9.5 * k, y0 + 15.5 * k), (x0 + 17.0 * k, y0 + 6.5 * k), p.v(2.2), t.on_accent);
                }
                c.text_fit(&body, p.r(Rect { x: r.x + 34, y: r.y + 6, w: r.w - 34, h: 24 }), s, t.text);
            }
            Item::Progress(rank, done, share) => steps(&mut c, &p, t, *r, *rank, *done, *share, &small),
            Item::Search => {
                let f = l.field.as_ref().filter(|f| f.part == Part::Prog(ProgPart::Search));
                field(&mut c, &p, t, *r, &l.query, f, hover(Part::Prog(ProgPart::Search)), &body);
                if f.is_none() && l.query.is_empty() {
                    c.text_fit(&body, p.r(Rect { x: r.x + 12, w: r.w - 24, ..*r }), PLACEHOLDER, t.muted);
                }
            }
            Item::Tiles(title, tiles, first) => {
                c.text_fit(&label, p.r(*r), &title.to_uppercase(), t.muted);
                for (i, tile) in tiles.iter().enumerate() {
                    let Some(rect) = lay.tiles.get(first + i).copied().filter(in_view) else { continue };
                    let on = hover(Part::Prog(ProgPart::Tile(first + i))) || hover(Part::Prog(ProgPart::More(first + i)));
                    draw_tile(&mut c, &p, t, rect, tile, on, hover(Part::Prog(ProgPart::More(first + i))));
                }
            }
        }
    }
    // The head lies over what scrolled under it.
    let hd = p.r(lay.head);
    c.fill(hd.x, hd.y, hd.w, hd.h, t.paper);
    if lay.scrolled {
        c.fill(hd.x, hd.y + hd.h - 1, hd.w, p.line().round() as i32, t.line);
    }
    c.text_fit(&font(t, 28.0, Weight::Light), p.r(lay.title), "Programme", t.text);
    let (state, bad) = &l.state;
    let sub = if state.is_empty() { l.address.clone() } else { format!("{} · {state}", l.address) };
    c.text_fit(&small, p.r(lay.sub), &sub, if *bad { t.danger } else { t.muted });
    if let Some((m, err)) = &l.msg {
        c.text_fit(&small, p.r(lay.msg), m, if *err { t.danger } else { t.accent });
    }
    for (i, name) in l.tabs.iter().enumerate() {
        if let Some(b) = lay.part(ProgPart::Tab(i)) {
            pill(&mut c, &p, t, b, name, i == l.tab, hover(Part::Prog(ProgPart::Tab(i))), true, &ctl);
        }
    }
    if let Some(b) = lay.part(ProgPart::Sources) {
        button(&mut c, &p, t, b, "Quellen und Platz", hover(Part::Prog(ProgPart::Sources)), true, &ctl);
    }
    if let Some(d) = &l.dialog {
        crate::page_confirm::draw(&mut c, &p, t, lay.content, d, l.hover);
    }
    c
}

/// The run's five steps: done ones in accent, the one running outlined, the download share as a bar under it.
#[allow(clippy::too_many_arguments)]
fn steps(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, rank: Option<usize>, done: bool, share: u32, f: &crate::text::Font) {
    let n = PHASES.len() as i32;
    let sw = (r.w - (n - 1) * 6) / n;
    for (i, name) in PHASES.iter().enumerate() {
        let b = Rect { x: r.x + i as i32 * (sw + 6), y: r.y + 6, w: sw, h: 28 };
        let state = match rank {
            Some(k) if i < k || (i == k && done) => 2,
            Some(k) if i == k => 1,
            _ => 0,
        };
        let bb = p.b(b);
        c.rrect(bb, p.v(14.0), if state == 2 { t.chip } else { t.panel2 });
        c.rrect_line(bb, p.v(14.0), p.line(), if state > 0 { t.accent } else { t.line });
        c.text_mid(f, p.r(b), name, if state > 0 { t.text } else { t.muted });
        if i == 0 && share > 0 {
            let bar = Rect { x: b.x, y: b.y + 34, w: (b.w as i64 * share as i64 / 1000) as i32, h: 4 };
            let rr = p.r(bar);
            c.fill(rr.x, rr.y, rr.w.max(1), rr.h.max(1), t.accent);
        }
    }
}

/// A library tile: icon (or the letter on an accent square), name, and for a package the "…" to its view.
fn draw_tile(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, tile: &crate::programme_view::TileItem, hover: bool, more_hover: bool) {
    let b = p.b(r);
    if hover {
        c.rrect(b, p.v(16.0), t.panel2);
        c.rrect_line(b, p.v(16.0), p.line(), t.line);
    }
    let side = 80;
    let icon = Rect { x: r.x + (r.w - side) / 2, y: r.y + 12, w: side, h: side };
    let ib = p.r(icon);
    match &tile.icon {
        Icon::Image(_, a) => blit(c, a, ib),
        Icon::Fallback(ch) => letter(c, Box2::of(ib), *ch, t),
        Icon::Pending(_) => letter(c, Box2::of(ib), crate::library::letter(&tile.name), t),
    }
    let name = Rect { x: r.x + 4, y: icon.y + side + 10, w: r.w - 8, h: 22 };
    c.text_mid(&t.bfont(Family::Ui, 14.0, Weight::Medium), p.r(name), &tile.name, t.text);
    if tile.package.is_some() {
        let m = Rect { x: r.x + r.w - MORE - 4, y: r.y + 4, w: MORE, h: MORE };
        let mb = p.b(m);
        c.rrect(mb, p.v(14.0), if more_hover { t.chip } else { alpha(t.panel2, 0.9) });
        c.rrect_line(mb, p.v(14.0), p.line(), if more_hover { t.accent } else { t.line });
        c.text_mid(&font(t, 14.0, Weight::SemiBold), p.r(m), "…", t.text);
    }
}

/// An icon scaled into `b` (nearest pixel; a look change redraws it once).
fn blit(c: &mut Canvas, a: &Argb, b: Rect) {
    if a.w == 0 || a.h == 0 || b.w <= 0 || b.h <= 0 {
        return;
    }
    for y in 0..b.h {
        let sy = (y as u64 * a.h as u64 / b.h as u64) as u32;
        for x in 0..b.w {
            let sx = (x as u64 * a.w as u64 / b.w as u64) as u32;
            let px = a.px[(sy * a.w + sx) as usize];
            if px >> 24 > 0 {
                c.put_cov(b.x + x, b.y + y, px, 255);
            }
        }
    }
}

fn letter(c: &mut Canvas, b: Box2, ch: char, t: &Theme) {
    c.rrect(b, b.w * 0.22, t.accent);
    let f = t.bfont(Family::Display, 40.0, Weight::SemiBold);
    c.text_mid(&f, Rect { x: b.x as i32, y: b.y as i32, w: b.w as i32, h: b.h as i32 }, &ch.to_string(), t.on_accent);
}
