//! GM-6b: the look of the sofa mode's Bibliothek and Medien (DS rule "new look files"; plan/spec/sofa-mode.md §1,
//! 2.2, 2.3, board Bibliothek.dc.html): the left column "Sammlungen" (items 60 board px, the active one accent_soft
//! with accent text, the counts in JetBrains Mono) with "SORTIERT NACH" and its button at the foot, the line at its
//! right; the title and "12 installiert · 238 GB auf /data"; the grid of 5 columns; the selection bar with
//! "AUSGEWÄHLT", the name, its facts, Spielen (accent) and In Steam öffnen (outlined). Pure: sofa_draw.rs caches it.

use crate::canvas::Canvas;
use crate::layout::Rect;
use crate::sofa::Frame;
use crate::sofa_draw::{Ink, font};
use crate::sofa_focus::Item;
use crate::sofa_header_draw::{local, token_ring};
use crate::sofa_layout::{self as lay, PAD};
use crate::sofa_model::Tile;
use crate::sofa_tile_draw::{self as td, GRID, band};
use crate::text::{Typeface, Weight};
use crate::theme::alpha;

#[derive(Clone, Debug, PartialEq)]
pub struct LibraryLook {
    pub frame: Frame,
    pub bw: i32,
    pub bh: i32,
    /// The column: (title, count) and the active one.
    pub colls: Vec<(&'static str, Option<usize>)>,
    pub active: usize,
    pub sort: String,
    pub title: String,
    pub info: String,
    pub tiles: Vec<Tile>,
    pub first: usize,
    pub sel: Option<Tile>,
    pub facts: String,
    pub items: Vec<Item>,
    pub focus: Option<String>,
    pub note: Option<String>,
    pub motion: bool,
}

pub const COLUMN: &str = "Sammlungen";
pub const SORTED: &str = "Sortiert nach";
pub const PICKED: &str = "Ausgewählt";
pub const PLAY: &str = "Spielen";
pub const IN_STEAM: &str = "In Steam öffnen";
pub const EMPTY: &str = "Hier ist noch nichts.";

/// The German word of a `sofa.sort` value.
pub fn sort_word(s: &str) -> &'static str {
    match s {
        "az" => "A–Z",
        "groesse" => "Größe",
        _ => "Zuletzt gespielt",
    }
}

fn rect_of(l: &LibraryLook, id: &str) -> Option<Rect> {
    l.items.iter().find(|i| i.id == id).map(|i| i.rect)
}

pub fn draw_library(l: &LibraryLook, o: Rect, ink: &Ink) -> Canvas {
    let f = &l.frame;
    let k = f.k as f32;
    let mut c = Canvas::new(o.w, o.h);
    let g = lay::lib_geom(l.bw, l.bh);
    let focus = |id: &str| l.focus.as_deref() == Some(id);
    // The column's line at its right.
    let line = local(f, o, Rect { x: g.side.x + g.side.w - 1, y: g.side.y, w: 1, h: g.side.h });
    c.fill(line.x as i32, line.y as i32, k.max(1.0).round() as i32, line.h as i32, ink.line);
    let lbl = td::label(k);
    c.text_fit(&lbl, band(local(f, o, Rect { x: PAD + 16, y: lay::HEADER_H + 40, w: 260, h: 16 })), &COLUMN.to_uppercase(), ink.muted);
    let item = font(Typeface::Figtree, 21.0, Weight::Medium, k);
    let item_on = font(Typeface::Figtree, 21.0, Weight::SemiBold, k);
    let num = font(Typeface::JetBrainsMono, 16.0, Weight::Regular, k);
    for (i, (title, n)) in l.colls.iter().enumerate() {
        let Some(r) = rect_of(l, &format!("coll:{i}")) else { continue };
        let b = local(f, o, r);
        let on = i == l.active;
        if on {
            c.rrect(b, 14.0 * k, ink.accent_soft);
        }
        let t = band(local(f, o, Rect { x: r.x + 20, w: r.w - 40, ..r }));
        c.text_fit(if on { &item_on } else { &item }, t, title, if on { ink.accent } else { ink.text });
        if let Some(n) = n {
            c.text_right(&num, t, &n.to_string(), if on { ink.accent } else { ink.muted });
        }
        if focus(&format!("coll:{i}")) {
            token_ring(&mut c, b, 14.0 * k, k, ink);
        }
    }
    if let Some(r) = rect_of(l, "sort") {
        c.text_fit(&lbl, band(local(f, o, Rect { x: r.x + 16, y: r.y - 26, w: r.w, h: 16 })), &SORTED.to_uppercase(), ink.muted);
        let b = local(f, o, r);
        c.rrect_line(b, 14.0 * k, k.max(1.0), ink.line_strong);
        c.text_fit(&font(Typeface::Figtree, 19.0, Weight::SemiBold, k), band(local(f, o, Rect { x: r.x + 20, w: r.w - 40, ..r })), sort_word(&l.sort), ink.text);
        if focus("sort") {
            token_ring(&mut c, b, 14.0 * k, k, ink);
        }
    }
    // The title and its line (baseline-aligned in the board; centred on the title's band here).
    let tf = font(Typeface::Bricolage, 48.0, Weight::SemiBold, k);
    let tw = tf.width(&l.title);
    let tb = local(f, o, g.title);
    c.text_fit(&tf, band(tb), &l.title, ink.text);
    let info = crate::canvas::Box2 { x: tb.x + tw as f32 + 20.0 * k, w: (tb.w - tw as f32 - 20.0 * k).max(0.0), ..tb };
    let (txt, col) = match &l.note {
        Some(n) => (n.as_str(), td::warnung()),
        None => (l.info.as_str(), ink.muted),
    };
    c.text_fit(&font(Typeface::Figtree, 19.0, Weight::Regular, k), band(info), txt, col);
    if l.tiles.is_empty() {
        c.text_fit(&font(Typeface::Figtree, 20.0, Weight::Regular, k), band(local(f, o, g.grid)), EMPTY, ink.muted);
    }
    for (i, t) in l.tiles.iter().enumerate() {
        let id = format!("tile:{i}");
        let Some(r) = rect_of(l, &id) else { continue };
        let r = Rect { y: r.y - if l.motion && focus(&id) { 4 } else { 0 }, ..r };
        td::tile(&mut c, f, o, r, t, GRID, ink);
        if focus(&id) {
            td::focus_ring(&mut c, local(f, o, r), 20.0 * k, k, ink);
        }
    }
    bar(&mut c, l, o, ink);
    c
}

fn bar(c: &mut Canvas, l: &LibraryLook, o: Rect, ink: &Ink) {
    let f = &l.frame;
    let k = f.k as f32;
    let g = lay::lib_geom(l.bw, l.bh);
    let b = local(f, o, g.bar);
    c.rrect(b, 20.0 * k, ink.panel);
    c.rrect_line(b, 20.0 * k, k.max(1.0), ink.line);
    let (Some(play), Some(steam)) = (rect_of(l, "play"), rect_of(l, "steam-open")) else { return };
    let mut x = g.bar.x + 24;
    let mid = |x: i32, w: i32| band(local(f, o, Rect { x, y: g.bar.y, w, h: g.bar.h }));
    let lbl = td::label(k);
    let lw = (lbl.width(&PICKED.to_uppercase()) as f32 / k).ceil() as i32;
    c.text_fit(&lbl, mid(x, lw + 2), &PICKED.to_uppercase(), ink.muted);
    x += lw + 16;
    let room = play.x - 16 - x;
    if let Some(t) = &l.sel {
        let nf = font(Typeface::Figtree, 20.0, Weight::SemiBold, k);
        let nw = ((nf.width(&t.name) as f32 / k).ceil() as i32).min(room * 2 / 3);
        c.text_fit(&nf, mid(x, nw), &t.name, ink.text);
        x += nw + 16;
        c.text_fit(&font(Typeface::Figtree, 18.0, Weight::Regular, k), mid(x, (play.x - 16 - x).max(0)), &l.facts, ink.muted);
    }
    let bf = font(Typeface::Figtree, 20.0, Weight::Bold, k);
    let pb = local(f, o, play);
    let can = l.sel.as_ref().is_some_and(|t| !t.greyed);
    c.rrect(pb, 14.0 * k, if can { ink.accent } else { alpha(ink.accent, 0.4) });
    c.text_mid(&bf, band(pb), PLAY, ink.on_accent);
    let sb = local(f, o, steam);
    let steamy = l.sel.as_ref().is_some_and(|t| t.steam.is_some());
    let fade = |col| if steamy { col } else { alpha(col, 0.4) };
    c.rrect_line(sb, 14.0 * k, k.max(1.0), fade(ink.line_strong));
    c.text_mid(&font(Typeface::Figtree, 20.0, Weight::SemiBold, k), band(sb), IN_STEAM, fade(ink.text));
    for (id, bb) in [("play", pb), ("steam-open", sb)] {
        if l.focus.as_deref() == Some(id) {
            token_ring(c, bb, 14.0 * k, k, ink);
        }
    }
}
