//! GM-6d: the look of the sofa mode's search (DS rule "new look files"; plan/spec/sofa-mode.md §1, 2.6, board
//! Tastatur.dc.html): the field (96 board px high, radius 28, panel with the token ring, the magnifier 34 px, the
//! text Bricolage 44 600 with its caret, "SPIELE · VIDEOS · MUSIK · WEB" at the right), the hit cards (120 high,
//! radius 20; the best program in its tile colour, YouTube and Web · Brave on panel), the second row of further
//! programs, and the keyboard panel at the foot (panel at 86 %, a line at its top; keys 76 high, radius 14, 28 600,
//! the wide keys 22, Suchen filled with the accent; the focused key accent_soft with the token ring) with its hint
//! line. The whole screen (no header, no footer); physical px = board px x k; always the dark tokens, no caret
//! blink. A pure drawing: sofa_draw.rs caches it by its look.

use crate::canvas::{Box2, Canvas};
use crate::layout::Rect;
use crate::sofa::Frame;
use crate::sofa_draw::{Ink, font};
use crate::sofa_focus::Item;
use crate::sofa_header_draw::{local, token_ring};
use crate::sofa_layout::{self as lay, FIELD, HIT_GAP, HIT_H, HIT_Y, KEY_HINT_H, SEARCH_SIDE};
use crate::sofa_search_app::Card;
use crate::sofa_tile_draw::{band, focus_ring, kontext, label, on_kontext};
use crate::text::{Typeface, Weight};
use crate::theme::alpha;

/// The field's word at the right on the search.
pub const KINDS: &str = "SPIELE · VIDEOS · MUSIK · WEB";
/// The keyboard's hint line (sign, words).
pub const HINTS: [(&str, &str); 4] = [("A", " Taste drücken · Klick"), ("X", " Löschen"), ("B", " Tastatur zu · Esc"), ("", "Eine echte Tastatur tippt einfach mit")];
/// What a secret shows per character.
pub const DOT: char = '•';

#[derive(Clone, Debug, PartialEq)]
pub struct SearchLook {
    pub frame: Frame,
    pub bw: i32,
    pub bh: i32,
    pub text: String,
    pub cursor: usize,
    /// A settings field's name (empty on the search).
    pub title: String,
    pub secret: bool,
    pub cards: Vec<Card>,
    pub first: usize,
    pub items: Vec<Item>,
    pub focus: Option<String>,
    pub shift: bool,
    pub note: Option<String>,
}

fn field(c: &mut Canvas, l: &SearchLook, o: Rect, ink: &Ink) {
    let f = &l.frame;
    let k = f.k as f32;
    let b = local(f, o, FIELD);
    c.rrect(b, 28.0 * k, ink.panel);
    token_ring(c, b, 28.0 * k, k, ink);
    // The magnifier (the board's 24-unit icon at 34 px): a circle and its handle, stroke 2 units.
    let u = 34.0 / 24.0 * k;
    let (ix, iy) = (b.x + 32.0 * k, b.y + (96.0 - 34.0) / 2.0 * k);
    c.ring(ix + 10.5 * u, iy + 10.5 * u, 6.5 * u, 2.0 * u, ink.accent);
    c.line((ix + 15.5 * u, iy + 15.5 * u), (ix + 21.0 * u, iy + 21.0 * u), 2.0 * u, ink.accent);
    let right = if l.title.is_empty() { KINDS.to_string() } else { l.title.to_uppercase() };
    let lf = label(k);
    let rw = lf.width(&right);
    let end = band(b).x + band(b).w - (32.0 * k) as i32;
    c.text_right(&lf, Rect { x: end - rw, y: band(b).y, w: rw, h: band(b).h }, &right, ink.muted);
    let tf = font(Typeface::Bricolage, 44.0, Weight::SemiBold, k);
    let tx = (ix + 54.0 * k) as i32;
    let avail = (end - rw - (24.0 * k) as i32 - tx).max(0);
    let shown: Vec<char> = if l.secret { vec![DOT; l.text.chars().count()] } else { l.text.chars().collect() };
    let cursor = l.cursor.min(shown.len());
    // The part around the cursor when the text is wider than the field.
    let mut start = 0;
    while start < cursor && tf.width(&shown[start..cursor].iter().collect::<String>()) > avail {
        start += 1;
    }
    let row = Rect { x: tx, y: band(b).y, w: avail, h: band(b).h };
    if shown.is_empty() {
        let hint = if l.title.is_empty() { "Suchen" } else { "Tippen" };
        c.text_fit(&tf, row, hint, alpha(ink.muted, 0.6));
    } else {
        c.text_fit(&tf, row, &shown[start..].iter().collect::<String>(), ink.text);
    }
    let cx = tx + tf.width(&shown[start..cursor].iter().collect::<String>());
    let h = tf.height();
    c.fill(cx, row.y + (row.h - h) / 2, (2.0 * k).round() as i32, h, ink.accent);
}

fn hit_card(c: &mut Canvas, f: &Frame, o: Rect, r: Rect, cd: &Card, focused: bool, ink: &Ink) {
    let k = f.k as f32;
    let b = local(f, o, r);
    let fade = |col| if cd.greyed { alpha(col, 0.45) } else { col };
    let (lab, name) = match cd.colour {
        Some(n) => {
            c.rrect(b, 20.0 * k, kontext()[n % 4]);
            (font(Typeface::JetBrainsMono, 13.0, Weight::SemiBold, k).spaced(0.08), font(Typeface::Bricolage, 30.0, Weight::SemiBold, k))
        }
        None => {
            c.rrect(b, 20.0 * k, ink.panel);
            c.rrect_line(b, 20.0 * k, k.max(1.0), ink.line);
            (label(k), font(Typeface::Figtree, 26.0, Weight::SemiBold, k))
        }
    };
    let (lc, nc) = if cd.colour.is_some() { (on_kontext(), on_kontext()) } else { (ink.muted, ink.text) };
    let x = r.x + 28;
    let w = r.w - 56;
    c.text_fit(&lab, band(local(f, o, Rect { x, y: r.y + 30, w, h: 18 })), &cd.label, fade(lc));
    c.text_fit(&name, band(local(f, o, Rect { x, y: r.y + 52, w, h: 40 })), &cd.name, fade(nc));
    if focused {
        focus_ring(c, b, 20.0 * k, k, ink);
    }
}

fn key(c: &mut Canvas, f: &Frame, o: Rect, it: &Item, l: &SearchLook, ink: &Ink) {
    let k = f.k as f32;
    let name = it.id.strip_prefix("key:").unwrap_or("");
    let b = local(f, o, it.rect);
    let focused = l.focus.as_deref() == Some(it.id.as_str());
    let enter = matches!(name, "Suchen" | "Fertig");
    let wide = name.chars().count() > 1;
    let active = name == "⇧ Groß" && l.shift;
    let (bg, fg) = if focused || active {
        (ink.accent_soft, ink.accent)
    } else if enter {
        (ink.accent, ink.on_accent)
    } else {
        (ink.panel, if name == "Leerzeichen" { ink.muted } else { ink.text })
    };
    c.rrect(b, 14.0 * k, bg);
    if !enter && !focused {
        c.rrect_line(b, 14.0 * k, k.max(1.0), ink.line);
    }
    let shown = if !wide && l.shift { crate::sofa_search::shifted(name.chars().next().unwrap_or(' '), true).to_string() } else { name.to_string() };
    let tf = font(Typeface::Figtree, if wide { 22.0 } else { 28.0 }, Weight::SemiBold, k);
    // ⌫ and ⇧ are in none of the fonts (Figtree, Bricolage): drawn as lines before the word.
    match shown.split_once(' ').filter(|(s, _)| matches!(*s, "⌫" | "⇧")) {
        Some((sign, word)) => {
            let (s, gap) = (22.0 * k, 8.0 * k);
            let w = s + gap + tf.width(word) as f32;
            let (x, y) = (b.x + (b.w - w) / 2.0, b.y + (b.h - s) / 2.0);
            icon(c, sign == "⌫", x, y, s, k, fg);
            c.text_fit(&tf, band(Box2 { x: x + s + gap, y: b.y, w: w - s - gap + 2.0, h: b.h }), word, fg);
        }
        None => c.text_mid(&tf, band(b), &shown, fg),
    }
    if focused {
        token_ring(c, b, 14.0 * k, k, ink);
    }
}

/// The Löschen sign (a key cap pointing left with an x) or the Groß sign (an arrow up), `s` px square at (x, y).
fn icon(c: &mut Canvas, delete: bool, x: f32, y: f32, s: f32, k: f32, col: crate::theme::Rgba) {
    let w = 2.0 * k;
    let pts: Vec<(f32, f32)> = if delete {
        vec![(0.0, 0.5), (0.32, 0.12), (1.0, 0.12), (1.0, 0.88), (0.32, 0.88)]
    } else {
        vec![(0.5, 0.05), (0.95, 0.5), (0.7, 0.5), (0.7, 0.95), (0.3, 0.95), (0.3, 0.5), (0.05, 0.5)]
    };
    let p = |(u, v): (f32, f32)| (x + u * s, y + v * s);
    for i in 0..pts.len() {
        c.line(p(pts[i]), p(pts[(i + 1) % pts.len()]), w, col);
    }
    if delete {
        c.line(p((0.5, 0.33)), p((0.8, 0.67)), w, col);
        c.line(p((0.8, 0.33)), p((0.5, 0.67)), w, col);
    }
}

fn hints(c: &mut Canvas, f: &Frame, o: Rect, l: &SearchLook, ink: &Ink) {
    let k = f.k as f32;
    let (sign, words) = (font(Typeface::Figtree, 17.0, Weight::SemiBold, k), font(Typeface::Figtree, 17.0, Weight::Regular, k));
    let gap = (40.0 * k) as i32;
    let total: i32 = HINTS.iter().map(|(s, w)| sign.width(s) + words.width(w)).sum::<i32>() + gap * (HINTS.len() as i32 - 1);
    let line = band(local(f, o, Rect { x: 0, y: l.bh - 44 - KEY_HINT_H, w: l.bw, h: KEY_HINT_H }));
    let mut x = (o.w - total) / 2;
    for (s, w) in HINTS {
        let top = line.y + (line.h - words.height()) / 2;
        x += c.text(&sign, x, top, s, ink.text);
        x += c.text(&words, x, top, w, ink.muted) + gap;
    }
}

/// The whole screen; `o` is the image's physical place (the output).
pub fn draw_search(l: &SearchLook, o: Rect, ink: &Ink) -> Canvas {
    let f = &l.frame;
    let k = f.k as f32;
    let mut c = Canvas::new(o.w, o.h);
    field(&mut c, l, o, ink);
    let (lo, hi) = (SEARCH_SIDE - 1, l.bw - SEARCH_SIDE + 1);
    for it in &l.items {
        let Some(n) = it.id.strip_prefix("hit:").and_then(|n| n.parse::<usize>().ok()) else { continue };
        // The second row's cards beside the screen are not drawn (the focus brings them in).
        if it.rect.x < lo || it.rect.x + it.rect.w > hi {
            continue;
        }
        if let Some(cd) = l.cards.get(n) {
            hit_card(&mut c, f, o, it.rect, cd, l.focus.as_deref() == Some(it.id.as_str()), ink);
        }
    }
    let top = lay::keys_top(l.bh);
    let panel = local(f, o, Rect { x: 0, y: top, w: l.bw, h: l.bh - top });
    let py = panel.y.round() as i32;
    c.fill(0, py, o.w, o.h - py, alpha(ink.panel, 0.86));
    c.fill(0, py, o.w, k.max(1.0).round() as i32, ink.lifted_ring);
    for it in l.items.iter().filter(|i| i.id.starts_with("key:")) {
        key(&mut c, f, o, it, l, ink);
    }
    hints(&mut c, f, o, l, ink);
    if let Some(n) = &l.note {
        let y = if l.cards.is_empty() { HIT_Y } else { (HIT_Y + 2 * (HIT_H + HIT_GAP)).min(top - 40) };
        let r = Rect { x: SEARCH_SIDE, y, w: l.bw - 2 * SEARCH_SIDE, h: 28 };
        c.text_fit(&font(Typeface::Figtree, 19.0, Weight::Medium, k), band(local(f, o, r)), n, ink.accent);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sofa_layout::Level;

    #[test]
    fn every_text_of_the_search_has_its_glyphs() {
        let k = 4.0 / 3.0;
        let mut texts: Vec<(crate::text::Font, String)> = Vec::new();
        for (lv, enter) in [(Level::Letters, "Suchen"), (Level::Symbols, "Fertig")] {
            for (l, _) in lay::key_rows(lv, enter).into_iter().flatten() {
                let up = crate::sofa_search::shifted(l.chars().next().unwrap_or(' '), true).to_string();
                if l.chars().count() == 1 {
                    texts.push((font(Typeface::Figtree, 28.0, Weight::SemiBold, k), format!("{l}{up}")));
                }
                // ⌫ and ⇧ are drawn as lines (no font has them).
                let l = l.trim_start_matches(['⌫', '⇧']);
                texts.push((font(Typeface::Figtree, 22.0, Weight::SemiBold, k), l.into()));
            }
        }
        let typed = "Größe Dungeon Keeper 2 äöüÄÖÜß€§@\"'»«•";
        texts.push((font(Typeface::Bricolage, 44.0, Weight::SemiBold, k), format!("{typed} Suchen Tippen")));
        texts.push((label(k), format!("{KINDS} SPIEL PROGRAMM YOUTUBE · KOMMT BALD WEB · BRAVE WLAN-NAME WLAN-PASSWORT")));
        texts.push((font(Typeface::Figtree, 26.0, Weight::SemiBold, k), format!("»{typed}« Im Netz suchen Nichts gefunden")));
        texts.push((font(Typeface::Bricolage, 30.0, Weight::SemiBold, k), "Dungeon Keeper 2 Größe".into()));
        for (s, w) in HINTS {
            texts.push((font(Typeface::Figtree, 17.0, Weight::SemiBold, k), s.into()));
            texts.push((font(Typeface::Figtree, 17.0, Weight::Regular, k), w.into()));
        }
        texts.push((font(Typeface::Figtree, 19.0, Weight::Medium, k), "Kein Programm passt zu „Größe“. Das WLAN-Passwort geht nur mit dem Einstellungsdienst.".into()));
        for (f, s) in texts {
            assert!(s.chars().all(|c| c == ' ' || f.draws(c)), "{s}");
        }
    }

    #[test]
    fn the_search_draws_at_2560x1440() {
        let ink = Ink::dark();
        let f = Frame::of(2560, 1440, 100);
        let (bw, bh) = f.board();
        let items = lay::search(bw, bh, true, 0, 0, Level::Letters, "Suchen");
        let card = |c: Option<usize>| Card { label: "SPIEL".into(), name: "Dungeon Keeper 2".into(), colour: c, greyed: false };
        let look = SearchLook {
            frame: f, bw, bh, text: "dung".into(), cursor: 4, title: String::new(), secret: false,
            cards: vec![card(Some(0)), card(None), card(None)], first: 0, items: items.clone(), focus: Some("key:z".into()), shift: false, note: None,
        };
        let o = Rect { x: 0, y: 0, w: 2560, h: 1440 };
        let c = draw_search(&look, o, &ink);
        let px = |id: &str, dx: i32, dy: i32| {
            let r = f.r(items.iter().find(|i| i.id == id).unwrap().rect);
            c.pixel(r.x + dx, r.y + dy)
        };
        let p = crate::canvas::pack;
        assert_eq!(px("hit:0", 12, 60), p(kontext()[0]), "the best hit in its tile colour");
        assert_eq!(px("key:Suchen", 6, 50), p(ink.accent), "Suchen filled with the accent");
        assert_eq!(px("key:z", 8, 50), p(ink.accent_soft), "the focused key");
        assert_eq!(px("key:q", 8, 50), p(ink.panel), "a key");
        let fr = f.r(FIELD);
        assert_eq!(c.pixel(fr.x + fr.w / 2, fr.y - 5), p(ink.accent), "the field's accent ring");
    }
}
