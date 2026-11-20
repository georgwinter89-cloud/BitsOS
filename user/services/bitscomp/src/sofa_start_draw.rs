//! GM-6b: the look of the sofa mode's Start screen (DS rule "new look files"; plan/spec/sofa-mode.md §1, 2.1, board
//! Main.dc.html): the hero tile in the game's kontext colour with its two ring shapes (bg at 14 % and 12 %), the
//! label, the name, "Zuletzt gespielt heute um 19:40 · 2 Std. 15 Min." and the buttons Spielen and Details (the
//! empty state: "Bibliothek" with the count of programs and one button); the YouTube and Musik cards (the music card
//! greyed "Musik: noch nicht da" without a player); "Zuletzt gespielt" with "Alle ansehen", its tiles and the dashed
//! "Steam öffnen". Physical px = board px x k; always the dark tokens. A pure drawing: sofa_draw.rs caches it.

use crate::canvas::{Box2, Canvas};
use crate::layout::Rect;
use crate::sofa::Frame;
use crate::sofa_draw::{Ink, font};
use crate::sofa_focus::Item;
use crate::sofa_header_draw::{local, token_ring};
use crate::sofa_layout::{self as lay, HERO, HERO_IN, PAD, Text};
use crate::sofa_model::{Hero, Tile};
use crate::sofa_music::Music;
use crate::sofa_tile_draw::{self as td, START, band};
use crate::text::{Typeface, Weight};
use crate::theme::alpha;

#[derive(Clone, Debug, PartialEq)]
pub struct StartLook {
    pub frame: Frame,
    pub bw: i32,
    pub bh: i32,
    pub hero: Hero,
    pub recent: Vec<Tile>,
    /// The YouTube tile (its line and whether it waits, S1).
    pub youtube: Tile,
    pub music: Option<Music>,
    pub items: Vec<Item>,
    pub focus: Option<String>,
    pub note: Option<String>,
    pub motion: bool,
    /// DB4: "Gerade passend" under Zuletzt gespielt, (name, reason) each.
    pub passend: Vec<(String, String)>,
}

pub const YT_LABEL: &str = "YouTube · Weiterschauen";
pub const YT_TITLE: &str = "YouTube öffnen";
pub const MUSIC_LABEL: &str = "Musik · Zuletzt gehört";
pub const MUSIC_NONE: &str = "Musik: noch nicht da";
pub const RECENT_TITLE: &str = "Zuletzt gespielt";
pub const ALL: &str = "Alle ansehen";
pub const MUSIC_LATER: &str = "Der Musikspieler läuft noch nicht";

fn rect_of(l: &StartLook, id: &str) -> Option<Rect> {
    l.items.iter().find(|i| i.id == id).map(|i| i.rect)
}

/// The body between the header and the footer; `o` is the image's physical place.
pub fn draw_start(l: &StartLook, o: Rect, ink: &Ink) -> Canvas {
    let f = &l.frame;
    let k = f.k as f32;
    let mut c = Canvas::new(o.w, o.h);
    hero(&mut c, l, o, ink);
    cards(&mut c, l, o, ink);
    let sy = HERO.y + HERO.h + 44;
    let sec = font(Typeface::Bricolage, 34.0, Weight::SemiBold, k);
    let tw = crate::sofa_draw::measure(RECENT_TITLE, Text::Section);
    c.text_fit(&sec, band(local(f, o, Rect { x: PAD, y: sy, w: tw + 4, h: 38 })), RECENT_TITLE, ink.text);
    if let Some(r) = rect_of(l, "alle") {
        let b = local(f, o, r);
        c.text_fit(&font(Typeface::Figtree, 18.0, Weight::SemiBold, k), band(b), ALL, ink.accent);
        if l.focus.as_deref() == Some("alle") {
            token_ring(&mut c, b.inset(-4.0 * k), 8.0 * k, k, ink);
        }
    }
    if let Some(n) = &l.note {
        let r = Rect { x: PAD + 700, y: sy, w: l.bw - 2 * PAD - 700, h: 38 };
        c.text_right(&font(Typeface::Figtree, 18.0, Weight::SemiBold, k), band(local(f, o, r)), n, td::warnung());
    }
    let lift = |id: &str| if l.motion && l.focus.as_deref() == Some(id) { 4 } else { 0 };
    for (i, t) in l.recent.iter().enumerate() {
        let id = format!("tile:{i}");
        let Some(r) = rect_of(l, &id) else { continue };
        let r = Rect { y: r.y - lift(&id), ..r };
        td::tile(&mut c, f, o, r, t, START, ink);
        if l.focus.as_deref() == Some(id.as_str()) {
            td::focus_ring(&mut c, local(f, o, r), 20.0 * k, k, ink);
        }
    }
    crate::passend::draw_sofa(&mut c, l, o, ink);
    if let Some(r) = rect_of(l, "steam") {
        let r = Rect { y: r.y - lift("steam"), ..r };
        td::steam_tile(&mut c, f, o, r, ink);
        if l.focus.as_deref() == Some("steam") {
            td::focus_ring(&mut c, local(f, o, r), 20.0 * k, k, ink);
        }
    }
    c
}

fn hero(c: &mut Canvas, l: &StartLook, o: Rect, ink: &Ink) {
    let f = &l.frame;
    let k = f.k as f32;
    let b = local(f, o, HERO);
    let rad = 28.0 * k;
    c.rrect(b, rad, td::kontext()[l.hero.colour.min(3)]);
    let (cx, cy) = (b.x + 930.0 * k, b.y + 230.0 * k);
    c.ring_in(cx, cy, 300.0 * k, 56.0 * k, b, rad, alpha(ink.bg, 0.14));
    c.ring_in(cx, cy, 150.0 * k, 40.0 * k, b, rad, alpha(ink.bg, 0.12));
    let on = td::on_kontext();
    let x = HERO.x + HERO_IN;
    let w = HERO.w - 2 * HERO_IN;
    let row = |y: i32, h: i32| band(local(f, o, Rect { x, y: HERO.y + y, w, h }));
    c.text_fit(&font(Typeface::JetBrainsMono, 15.0, Weight::SemiBold, k).spaced(0.08), row(182, 16), &l.hero.label, on);
    c.text_fit(&font(Typeface::Bricolage, 76.0, Weight::SemiBold, k), row(206, 90), &l.hero.name, on);
    c.text_fit(&font(Typeface::Figtree, 20.0, Weight::Medium, k), row(304, 26), &l.hero.line, on);
    let bf = font(Typeface::Figtree, 22.0, Weight::Bold, k);
    let m = &crate::sofa_draw::measure;
    for (word, r) in lay::hero_buttons(l.hero.key.is_some(), m) {
        let bb = local(f, o, r);
        let br = 14.0 * k;
        if word == "Details" {
            c.rrect_line(bb, br, 2.0 * k, on);
            c.text_mid(&bf, band(bb), word, on);
            if l.focus.as_deref() == Some("hero:details") {
                token_ring(c, bb, br, k, ink);
            }
        } else if word == "Spielen" {
            c.rrect(bb, br, ink.bg);
            // The play triangle (20 x 22) left of the word, 12 px between.
            let (tx, ty) = (bb.x + 40.0 * k, bb.y + (bb.h - 22.0 * k) / 2.0);
            c.poly(&[(tx + 2.0 * k, ty + 2.0 * k), (tx + 19.0 * k, ty + 11.0 * k), (tx + 2.0 * k, ty + 20.0 * k)], ink.accent);
            let t = Box2 { x: tx + 32.0 * k, y: bb.y, w: bb.w - 72.0 * k, h: bb.h };
            c.text_fit(&bf, band(t), word, ink.accent);
        } else {
            c.rrect(bb, br, ink.bg);
            c.text_mid(&bf, band(bb), word, ink.accent);
        }
    }
    if l.focus.as_deref() == Some("hero") {
        td::focus_ring(c, b, rad, k, ink);
    }
}

fn cards(c: &mut Canvas, l: &StartLook, o: Rect, ink: &Ink) {
    let f = &l.frame;
    let k = f.k as f32;
    let cx = HERO.x + HERO.w + 32;
    let cw = l.bw - PAD - cx;
    let ch = (HERO.h - 32) / 2;
    let title = font(Typeface::Bricolage, 32.0, Weight::SemiBold, k);
    let body = font(Typeface::Figtree, 18.0, Weight::Regular, k);
    let card = |c: &mut Canvas, id: &str, y: i32, lbl: &str, head: &str, line: &str, greyed: bool| {
        let lift = if l.motion && l.focus.as_deref() == Some(id) { 4 } else { 0 };
        let r = Rect { x: cx, y: y - lift, w: cw, h: ch };
        let b = local(f, o, r);
        c.rrect(b, 28.0 * k, ink.panel);
        c.rrect_line(b, 28.0 * k, k.max(1.0), ink.line);
        let g = |col| if greyed { alpha(col, 0.45) } else { col };
        let inner = |y: i32, h: i32| band(local(f, o, Rect { x: r.x + 32, y: r.y + y, w: r.w - 64, h }));
        c.text_fit(&td::label(k), inner(32, 16), &lbl.to_uppercase(), g(ink.muted));
        c.text_fit(&title, inner((ch - 36) / 2, 40), head, g(ink.text));
        c.text_fit(&body, inner(ch - 32 - 26, 26), line, g(ink.muted));
        if l.focus.as_deref() == Some(id) {
            td::focus_ring(c, b, 28.0 * k, k, ink);
        }
    };
    let yt = &l.youtube;
    card(c, "card:youtube", HERO.y, YT_LABEL, YT_TITLE, if yt.greyed { &yt.meta } else { "" }, yt.greyed);
    let y2 = HERO.y + ch + 32;
    match &l.music {
        Some(m) => {
            let (head, line) = match &m.last {
                Some((list, n, out)) if out.is_empty() => (list.clone(), format!("{n} Titel")),
                Some((list, n, out)) => (list.clone(), format!("{n} Titel · {out}")),
                None => ("Musik".to_string(), format!("{} Titel", m.tracks)),
            };
            card(c, "card:musik", y2, MUSIC_LABEL, &head, &line, false);
        }
        None => card(c, "card:musik", y2, "Musik", MUSIC_NONE, MUSIC_LATER, true),
    }
}
