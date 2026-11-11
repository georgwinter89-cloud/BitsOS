//! PK-L1b: the program library's look (DS rule "new look files"), paint layer `Library`: the screen below the top
//! bar dimmed, and a panel in the Feld board's Absicht look (Feld.dc.html: 1120x800 board px, radius 28, an accent
//! edge): the search line at its top (Bricolage Grotesque), the rows' uppercase mono labels, tiles of a 96 board px
//! icon (128 px at 2560x1440) with the name below (Figtree), the focus as the design's ring (2 px ground, 4 px
//! accent), the key hints at its foot. Cheap level only: one state change on hover (the tile lifts with
//! theme.motion on, no animation). Two cached images: the panel (search, hints) and the scrolled tiles, so a focus
//! move redraws the tiles alone.

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::utils::{Physical, Point, Scale};

use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::Px;
use crate::header::px_rect;
use crate::icon_png::Argb;
use crate::layout::Rect;
use crate::library::Icon;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::{Motion, Style, Theme, alpha};

pub const PLACEHOLDER: &str = "Programm suchen …";
pub const ESC: &str = "Esc · Schließen";
pub const HINTS: [&str; 4] = ["↑ ↓ ← → wählen", "Tab nächste Reihe", "Enter starten", "Rechtsklick Menü"];

#[derive(Clone, Debug, PartialEq)]
struct PanelLook {
    panel: Rect,
    search: Rect,
    hints: Rect,
    query: String,
    note: Option<String>,
    /// PK-L1c: the pad's Y focused the search field: it gets the focus ring.
    search_focus: bool,
    theme: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct Tile {
    rect: Rect,
    name: String,
    icon: Icon,
    focus: bool,
    hover: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct BodyLook {
    /// The image's place: the body with room for the focus ring around it.
    at: Rect,
    heads: Vec<(Rect, &'static str)>,
    tiles: Vec<Tile>,
    lift: bool,
    theme: u64,
}

#[derive(Default)]
pub struct Images {
    dim: Option<SolidColorBuffer>,
    panel: Option<(PanelLook, MemoryRenderBuffer)>,
    body: Option<(BodyLook, MemoryRenderBuffer)>,
}

fn label(t: &Theme) -> crate::text::Font {
    t.style(Style::Label)
}

fn draw_panel(l: &PanelLook, t: &Theme) -> Canvas {
    let p = Px::new(l.panel, t.scale);
    let (w, h) = p.size();
    let k = t.board as f32;
    let mut c = Canvas::new(w, h);
    let all = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, 28.0 * k, t.panel);
    c.rrect_line(all, 28.0 * k, k.max(1.0), alpha(t.accent, 0.4));
    // The search line: the text typed (or what to do), its caret, the Esc chip at the right.
    let s = p.r(l.search);
    c.fill(0, s.y + s.h - k.max(1.0) as i32, w, k.max(1.0) as i32, t.line);
    let big = t.bfont(Family::Display, 34.0, Weight::Medium);
    let esc_w = (label(t).width(ESC) as f32 + 28.0 * k) as i32;
    let esc = Rect { x: s.x + s.w - (20.0 * k) as i32 - esc_w, y: s.y + (s.h - (44.0 * k) as i32) / 2, w: esc_w, h: (44.0 * k) as i32 };
    let text = Rect { x: s.x + (32.0 * k) as i32, y: s.y, w: esc.x - s.x - (48.0 * k) as i32, h: s.h };
    if l.query.is_empty() {
        c.text_fit(&big, text, PLACEHOLDER, t.muted);
        c.fill(text.x, s.y + (s.h - (36.0 * k) as i32) / 2, (3.0 * k) as i32, (36.0 * k) as i32, t.accent);
    } else {
        let tw = c.text_fit(&big, text, &l.query, t.text);
        c.fill(text.x + tw + (4.0 * k) as i32, s.y + (s.h - (36.0 * k) as i32) / 2, (3.0 * k) as i32, (36.0 * k) as i32, t.accent);
    }
    if l.search_focus {
        let ring = Box2::of(s).inset(8.0 * k);
        c.rrect_line(ring, 20.0 * k, 4.0 * k, t.accent);
    }
    c.rrect_line(Box2::of(esc), 14.0 * k, k.max(1.0), t.line);
    c.text_mid(&label(t), esc, &Theme::label(Style::Label, ESC), t.muted);
    // The foot: the note (a failed start, a pin) or the key hints.
    let f = p.r(l.hints);
    match &l.note {
        Some(n) => c.text_mid(&t.bfont(Family::Ui, 15.0, Weight::Medium), f, n, t.accent),
        None => {
            let hints: Vec<String> = HINTS.iter().map(|h| Theme::label(Style::Label, h)).collect();
            c.text_mid(&label(t), f, &hints.join("    "), t.muted);
        }
    }
    c
}

/// An icon scaled into `b` (nearest pixel: the tile size is fixed, so this runs once per look).
fn blit(c: &mut Canvas, a: &Argb, b: Rect) {
    if a.w == 0 || a.h == 0 || b.w <= 0 || b.h <= 0 {
        return;
    }
    for y in 0..b.h {
        let sy = (y as u64 * a.h as u64 / b.h as u64) as u32;
        for x in 0..b.w {
            let sx = (x as u64 * a.w as u64 / b.w as u64) as u32;
            let px = a.px[(sy * a.w + sx) as usize];
            let cov = (px >> 24) as u8;
            if cov > 0 {
                c.put_cov(b.x + x, b.y + y, px, 255);
            }
        }
    }
}

fn draw_tile(c: &mut Canvas, p: &Px, tile: &Tile, lift: bool, t: &Theme) {
    let k = t.board as f32;
    let lifted = lift && tile.hover;
    let r = if lifted { Rect { y: tile.rect.y - t.bl(4.0), ..tile.rect } } else { tile.rect };
    let b = p.b(r);
    let rad = 18.0 * k;
    if lifted {
        crate::deco::hud::shadow(c, b, rad, &t.shadow_lifted, t);
    }
    if tile.hover || tile.focus || lifted {
        c.rrect(b, rad, t.panel2);
    }
    if tile.focus {
        let (g, a) = (2.0 * k, 4.0 * k);
        c.rrect_line(b.inset(-g), rad + g, g, t.bg);
        c.rrect_line(b.inset(-g - a), rad + g + a, a, t.accent);
    }
    let side = t.bl(96.0);
    let icon = Rect { x: r.x + (r.w - side) / 2, y: r.y + t.bl(10.0), w: side, h: side };
    let ib = p.r(icon);
    match &tile.icon {
        Icon::Image(_, a) => blit(c, a, ib),
        Icon::Fallback(ch) => fallback(c, Box2::of(ib), *ch, t),
        Icon::Pending(_) => fallback(c, Box2::of(ib), crate::library::letter(&tile.name), t),
    }
    let name = Rect { x: r.x + t.bl(4.0), y: icon.y + icon.h + t.bl(8.0), w: r.w - t.bl(8.0), h: t.bl(24.0) };
    c.text_mid(&t.bfont(Family::Ui, 15.0, Weight::Medium), p.r(name), &tile.name, t.text);
}

/// The fallback icon: the name's first letter on an accent-coloured rounded square (desk-PK67-plan.md §3).
fn fallback(c: &mut Canvas, b: Box2, ch: char, t: &Theme) {
    c.rrect(b, b.w * 0.22, t.accent);
    let f = t.bfont(Family::Display, 52.0, Weight::SemiBold);
    c.text_mid(&f, Rect { x: b.x as i32, y: b.y as i32, w: b.w as i32, h: b.h as i32 }, &ch.to_string(), t.on_accent);
}

fn draw_body(l: &BodyLook, t: &Theme) -> Canvas {
    let p = Px::new(l.at, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    for (r, title) in &l.heads {
        c.text_fit(&label(t), p.r(*r), &Theme::label(Style::Label, title), t.muted);
    }
    for tile in &l.tiles {
        draw_tile(&mut c, &p, tile, l.lift, t);
    }
    c
}

fn solid<R: CompRenderer>(buf: &mut SolidColorBuffer, r: Rect, s: f64, c: crate::theme::Rgba) -> CompElement<R> {
    buf.update((r.w, r.h), c);
    let p = px_rect(r, s);
    let loc: Point<i32, Physical> = Point::from((p.x, p.y));
    SolidColorRenderElement::from_buffer(buf, loc, Scale::from(s), 1.0, Kind::Unspecified).into()
}

/// The overlay's elements, topmost first; none while it is closed.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let mut out = Vec::new();
    if !app.lib.open {
        app.lib.images.panel = None;
        app.lib.images.body = None;
        return out;
    }
    let t = app.theme.clone();
    let s = t.scale;
    let tg = app.theme_gen;
    let g = app.library_geo();
    let (rows, heads, placed) = app.library_tiles();
    let focus = app.lib.focus.clone();
    let tiles = placed
        .iter()
        .filter_map(|pl| {
            let e = app.library.entries.iter().find(|e| e.id == pl.id)?;
            Some(Tile {
                rect: pl.rect,
                name: e.name.clone(),
                icon: e.icon.clone(),
                focus: !app.lib.search_focus && focus.as_ref().is_some_and(|(r, id)| *r == pl.row && *id == pl.id),
                hover: app.lib.hover.as_ref().is_some_and(|(r, id)| *r == pl.row && *id == pl.id),
            })
        })
        .collect();
    let _ = rows;
    let m = t.bl(10.0);
    let body = BodyLook { at: Rect { x: g.body.x - m, y: g.body.y - m, w: g.body.w + 2 * m, h: g.body.h + 2 * m }, heads, tiles, lift: t.motion != Motion::Off, theme: tg };
    let panel = PanelLook { panel: g.panel, search: g.search, hints: g.hints, query: app.lib.query.clone(), note: app.lib.note.clone(), search_focus: app.lib.search_focus, theme: tg };
    let im = &mut app.lib.images;
    if im.body.as_ref().is_none_or(|(l, _)| *l != body) {
        im.body = Some((body.clone(), draw_body(&body, &t).into_buffer()));
    }
    if im.panel.as_ref().is_none_or(|(l, _)| *l != panel) {
        im.panel = Some((panel.clone(), draw_panel(&panel, &t).into_buffer()));
    }
    for (r, buf) in [(body.at, &im.body.as_ref().unwrap().1), (panel.panel, &im.panel.as_ref().unwrap().1)] {
        let p = px_rect(r, s);
        out.extend(crate::deco::image(renderer, buf, r, (p.w, p.h), s));
    }
    let dim = im.dim.get_or_insert_with(|| SolidColorBuffer::new((1, 1), t.bg));
    out.push(solid(dim, g.dim, s, alpha(t.bg, 0.62)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_of_the_overlay_has_its_glyphs() {
        let t = Theme::default();
        let mut texts: Vec<&str> = vec![PLACEHOLDER, ESC, "„Brave“ ist an der Startseite angeheftet.", "ließ sich nicht starten", "gelöst", "Kein Programm passt"];
        texts.extend(HINTS);
        let (big, ui, lab) = (t.bfont(Family::Display, 34.0, Weight::Medium), t.bfont(Family::Ui, 15.0, Weight::Medium), label(&t));
        for s in [PLACEHOLDER, "wri"] {
            assert!(s.chars().all(|c| c == ' ' || big.draws(c)), "{s}");
        }
        for s in &texts {
            assert!(s.chars().all(|c| c == ' ' || ui.draws(c)), "{s}");
        }
        for s in [ESC].iter().chain(HINTS.iter()) {
            assert!(Theme::label(Style::Label, s).chars().all(|c| c == ' ' || lab.draws(c)), "{s}");
        }
    }

    #[test]
    fn the_panel_and_a_fallback_tile_draw_in_both_modes() {
        for dark in [true, false] {
            let mut t = Theme::default();
            t.dark = dark;
            let panel = PanelLook { panel: Rect { x: 10, y: 50, w: 800, h: 600 }, search: Rect { x: 10, y: 50, w: 800, h: 72 }, hints: Rect { x: 10, y: 614, w: 800, h: 36 }, query: "wri".into(), note: None, search_focus: true, theme: 0 };
            let c = draw_panel(&panel, &t);
            assert!(c.w > 0 && c.pixel(c.w / 2, c.h / 2) != 0, "the panel is filled");
            let tile = Tile { rect: Rect { x: 40, y: 140, w: 108, h: 112 }, name: "Writer".into(), icon: Icon::Fallback('W'), focus: true, hover: true };
            let body = BodyLook { at: Rect { x: 20, y: 120, w: 400, h: 300 }, heads: vec![(Rect { x: 30, y: 120, w: 300, h: 20 }, "Favoriten")], tiles: vec![tile], lift: true, theme: 0 };
            let b = draw_body(&body, &t);
            let p = Px::new(body.at, t.scale);
            let centre = p.r(Rect { x: 40 + 54, y: 140 + 8 + 36, w: 1, h: 1 });
            assert_ne!(b.pixel(centre.x, centre.y), 0, "the fallback square is drawn");
        }
    }
}
