//! Drawing the settings page (`page`, WY-5b) into one image of a window's content, in the look of the start
//! page and the mock-up (panel cards with a line, pills, the accent on what is chosen). Redrawn only when its look
//! changes (a value, the hover, the scroll, the theme); nothing on it moves by itself.

use crate::canvas::Canvas;
use crate::chrome_draw::{Px, font};
use crate::layout::Rect;
use crate::page::{self, Heads, Kind, Layout, Part, Row, SWATCHES};
use crate::text::{Family, Weight};
use crate::theme::{Theme, alpha, hex};

/// A field being edited: its text and the text before the cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldLook {
    pub part: Part,
    pub text: String,
    pub before: String,
}

/// CL-5: how a row's small line reads: as a note, as good news, as a warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Good,
    Bad,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageLook {
    pub layout: Layout,
    pub theme: u64,
    /// The page's address (shown under the title).
    pub address: String,
    /// The model's rows and the value each shows.
    pub rows: Vec<Row>,
    pub values: Vec<String>,
    pub fav_text: String,
    pub favs: Vec<String>,
    /// The key the address points at (lit).
    pub target: Option<String>,
    pub hover: Option<Part>,
    pub field: Option<FieldLook>,
    /// The last answer of settingsd to a change (text, is an error).
    pub msg: Option<(String, bool)>,
    /// WY-10: the confirmation dialog over the page.
    pub dialog: Option<crate::page_confirm::Dialog>,
    /// settingsd answers: changes are possible.
    pub live: bool,
    /// CL-5: the page's title and the line under the address while nothing was answered.
    pub title: String,
    pub hint: String,
    /// CL-5: per row, a sentence instead of the key's address under its label (the page "KI"); empty: none has.
    pub subs: Vec<Option<(String, Tone)>>,
    /// WY-SET: per row, its marks (a dot when it differs from its default); empty: none.
    pub flags: Vec<page::Flag>,
}

pub const TITLE: &str = "Einstellungen";
pub const HINT: &str = "Jede Änderung lässt sich rückgängig machen.";

pub fn draw(l: &PageLook, t: &Theme) -> Canvas {
    let lay = &l.layout;
    let p = Px::new(lay.content, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.paper);
    let hover = |q: Part| l.live && l.hover == Some(q);
    let (label, small, ctl) = (font(t, 15.0, Weight::Medium), font(t, 12.0, Weight::Regular), font(t, 14.0, Weight::Medium));
    let head = font(t, 13.0, Weight::SemiBold).spaced(0.14);
    let inside = lay.heads_look != Heads::Above;
    let card_r = if inside { 22.0 } else { 12.0 };
    for r in &lay.cards {
        let b = p.b(*r);
        c.rrect(b, p.v(card_r), t.panel);
        c.rrect_line(b, p.v(card_r), p.line(), t.line);
    }
    let (title_font, crumb) = (t.font(Family::Display, 20.0, Weight::SemiBold), t.font(Family::Mono, 11.0, Weight::Medium).spaced(0.08));
    for (name, r) in &lay.heads {
        match lay.heads_look {
            Heads::Above => c.text_fit(&head, p.r(*r), name, t.muted),
            Heads::Title => c.text_fit(&title_font, p.r(*r), name, t.text),
            Heads::Crumb => c.text_fit(&crumb, p.r(*r), &name.to_uppercase(), t.muted),
        };
    }
    let sep = |c: &mut Canvas, r: Rect| {
        let rr = p.r(r);
        c.fill(rr.x + p.v(16.0) as i32, rr.y + rr.h - 1, rr.w - p.v(32.0) as i32, p.line().round() as i32, t.line);
    };
    for (i, ((row, rect), value)) in l.rows.iter().zip(&lay.rows).zip(&l.values).enumerate() {
        // Rows scrolled far out of the content are not drawn (the canvas clips the rest).
        if rect.y + rect.h < lay.content.y || rect.y > lay.content.y + lay.content.h {
            continue;
        }
        if l.target.as_deref() == Some(row.key.as_str()) {
            let b = p.b(*rect).inset(p.v(2.0));
            c.glow(b, p.v(10.0), 6, t.glow);
            c.rrect_line(b, p.v(10.0), p.line(), t.accent);
        }
        if !lay.last.get(i).copied().unwrap_or(true) {
            sep(&mut c, *rect);
        }
        let extra = lay.extra.get(i).copied().unwrap_or(0);
        let (_, _, avail, _) = page::control_place_beside(row, rect.x, rect.y, rect.w, lay.narrow, extra);
        if let Kind::Fold { open, count, changed } = &row.kind {
            fold(&mut c, &p, t, *rect, &row.label, *open, *count, *changed, hover(Part::Fold(i)));
            continue;
        }
        let stacked = rect.h != page::ROW_H;
        let lw = if stacked { rect.w - 40 } else { rect.w - 40 - page::controls_width(row, avail) - extra - 24 };
        let band = Rect { x: rect.x + 20, y: rect.y + 12, w: lw, h: 24 };
        let used = c.text_fit(&label, p.r(band), &row.label, t.text);
        if l.flags.get(i).is_some_and(|f| f.changed) {
            // The mock-up's amber dot: this key differs from its default.
            let lb = p.r(band);
            let dx = (lb.x + used + p.v(10.0) as i32).min(lb.x + lb.w - p.v(4.0) as i32);
            c.disc(dx as f32, lb.y as f32 + lb.h as f32 / 2.0, p.v(4.0), t.warm);
        }
        if let Some(b) = lay.part(Part::Reset(i)) {
            button(&mut c, &p, t, b, "Zurücksetzen", hover(Part::Reset(i)), l.live, &ctl);
        }
        let under = Rect { y: rect.y + 36, h: 18, ..band };
        match l.subs.get(i).and_then(Option::as_ref) {
            Some((text, tone)) => {
                let col = match tone {
                    Tone::Plain => t.muted,
                    Tone::Good => t.accent,
                    Tone::Bad => t.danger,
                };
                c.text_fit(&small, p.r(under), text, col);
            }
            None => {
                let mut addr = page::address_of_key(&row.key);
                if row.confirm {
                    addr = format!("{addr} · ändern mit Rückfrage");
                }
                if !row.note.is_empty() {
                    addr = format!("{addr} · {}", row.note);
                }
                c.text_fit(&small, p.r(under), &addr, t.muted);
            }
        }
        let edit = |q: Part| l.field.as_ref().filter(|f| f.part == q);
        match &row.kind {
            Kind::Choice(opts) => {
                for (o, (v, name)) in opts.iter().enumerate() {
                    if let Some(b) = lay.part(Part::Choice(i, o)) {
                        pill(&mut c, &p, t, b, name, value == v, hover(Part::Choice(i, o)), l.live, &ctl);
                    }
                }
            }
            Kind::Accent => {
                for (k, s) in SWATCHES.iter().enumerate() {
                    let Some(b) = lay.part(Part::Swatch(k)) else { continue };
                    let r = p.r(b);
                    let (cx, cy, rad) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0, r.w as f32 / 2.0);
                    if value == s || hover(Part::Swatch(k)) {
                        c.ring(cx, cy, rad - p.v(1.0), p.v(2.0), if value == s { t.text } else { t.muted });
                    }
                    c.disc(cx, cy, rad - p.v(6.0), hex(s).unwrap_or(t.accent));
                }
                if let Some(b) = lay.part(Part::Hex) {
                    field(&mut c, &p, t, b, value, edit(Part::Hex), hover(Part::Hex), &ctl);
                }
            }
            Kind::Number { unit, word, .. } => {
                let is_word = word.as_ref().filter(|(w, _)| w == value);
                if let (Some((_, name)), Some(b)) = (word, lay.part(Part::Word(i))) {
                    pill(&mut c, &p, t, b, name, is_word.is_some(), hover(Part::Word(i)), l.live, &ctl);
                }
                let (less, more, mid) = (Part::Less(i), Part::More(i), Part::Field(i));
                if let (Some(a), Some(z), Some(m)) = (lay.part(less), lay.part(more), lay.part(mid)) {
                    button(&mut c, &p, t, a, "−", hover(less), l.live, &ctl);
                    button(&mut c, &p, t, z, "+", hover(more), l.live, &ctl);
                    match edit(mid) {
                        Some(e) => field(&mut c, &p, t, m, value, Some(e), true, &ctl),
                        None => {
                            c.rrect(p.b(m), p.v(8.0), t.panel2);
                            if hover(mid) {
                                c.rrect_line(p.b(m), p.v(8.0), p.line(), t.accent);
                            }
                            let shown = match is_word {
                                Some((_, name)) => name.clone(),
                                None if unit.is_empty() => value.clone(),
                                None => format!("{value} {unit}"),
                            };
                            c.text_mid(&ctl, p.r(m), &shown, t.text);
                        }
                    }
                }
            }
            Kind::Switch => {
                if let Some(b) = lay.part(Part::Switch(i)) {
                    switch(&mut c, &p, t, b, value == "true", hover(Part::Switch(i)), l.live);
                }
            }
            Kind::Field { colour, .. } => {
                if let Some(b) = lay.part(Part::Field(i)) {
                    if *colour {
                        let r = p.r(Rect { x: b.x - SWATCH_GAP, w: page::SWATCH, ..b });
                        let (cx, cy, rad) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0, r.w as f32 / 2.0);
                        c.ring(cx, cy, rad - p.v(1.0), p.v(1.0), t.line);
                        c.disc(cx, cy, rad - p.v(4.0), hex(value).unwrap_or(t.panel2));
                    }
                    field(&mut c, &p, t, b, value, edit(Part::Field(i)), hover(Part::Field(i)), &ctl);
                }
            }
            Kind::Pills { opts, multi, notes, .. } => {
                let on = |v: &str| if *multi { value.split(',').any(|x| x.trim() == v) } else { value == v };
                for (o, (v, name)) in opts.iter().enumerate() {
                    if let Some(b) = lay.part(Part::Pill(i, o)) {
                        pill(&mut c, &p, t, b, name, on(v), hover(Part::Pill(i, o)), l.live, &ctl);
                        // The line beside the choice says what it is for.
                        if let Some(n) = notes.get(o) {
                            let nb = Rect { x: b.x + b.w + 16, y: b.y + 9, w: rect.x + rect.w - 20 - (b.x + b.w + 16), h: 18 };
                            c.text_fit(&small, p.r(nb), n, if on(v) { t.text } else { t.muted });
                        }
                    }
                }
            }
            Kind::Secret { .. } => {
                if let Some(b) = lay.part(Part::Field(i)) {
                    // What is typed is never drawn, nor how much of it: an empty field and a cursor at its start.
                    let typing = edit(Part::Field(i)).map(|f| FieldLook { part: f.part, text: String::new(), before: String::new() });
                    field(&mut c, &p, t, b, if typing.is_some() { "" } else { value }, typing.as_ref(), hover(Part::Field(i)), &ctl);
                    if typing.is_some() {
                        c.text_fit(&small, p.r(Rect { x: b.x + 20, w: b.w - 28, ..b }), "wird nicht angezeigt", t.muted);
                    }
                }
                if let Some(b) = lay.part(Part::Button(i, 0)) {
                    button(&mut c, &p, t, b, "Löschen", hover(Part::Button(i, 0)), l.live, &ctl);
                }
            }
            Kind::Link(_) => {
                if let Some(b) = lay.part(Part::Link(i)) {
                    button(&mut c, &p, t, b, page::LINK_TEXT, hover(Part::Link(i)), l.live, &ctl);
                }
            }
            Kind::Buttons(bs) => {
                for (o, (_, text)) in bs.iter().enumerate() {
                    if let Some(b) = lay.part(Part::Button(i, o)) {
                        button(&mut c, &p, t, b, text, hover(Part::Button(i, o)), l.live, &ctl);
                    }
                }
            }
            Kind::Fold { .. } => {}
            Kind::Subs(subs) => {
                let vals: Vec<&str> = value.split(';').collect();
                for (k, s) in subs.iter().enumerate() {
                    let Some(b) = lay.part(Part::Sub(i, k)) else { continue };
                    let v = vals.get(k).copied().unwrap_or("");
                    let q = Part::Sub(i, k);
                    if s.switch {
                        let sw = Rect { x: b.x + b.w - 52, y: b.y + 4, w: 52, h: 28 };
                        c.text_fit(&ctl, p.r(Rect { w: b.w - 60, ..b }), &s.label, t.text);
                        switch(&mut c, &p, t, sw, v == "true", hover(q), l.live);
                    } else {
                        match edit(q) {
                            Some(e) => field(&mut c, &p, t, b, v, Some(e), true, &ctl),
                            None => {
                                c.rrect(p.b(b), p.v(10.0), t.panel2);
                                c.rrect_line(p.b(b), p.v(10.0), p.line(), if hover(q) { t.accent } else { t.line_strong });
                                let text = if s.unit.is_empty() { format!("{} · {v}", s.label) } else { format!("{} · {v} {}", s.label, s.unit) };
                                c.text_mid(&t.font(Family::Mono, 12.0, Weight::Regular), p.r(b), &text, t.text);
                            }
                        }
                    }
                }
            }
            Kind::Chart => {
                let (vx, vy, vw, _) = page::control_place(row, rect.x, rect.y, rect.w, lay.narrow);
                chart(&mut c, &p, t, Rect { x: vx, y: vy, w: vw, h: page::CTL_H }, value);
            }
            Kind::Fixed(_) if crate::page_join::quiet(&row.key) => {}
            Kind::Fixed(why) => {
                let (vx, vy, vw, _) = page::control_place(row, rect.x, rect.y, rect.w, lay.narrow);
                let vb = Rect { x: vx, y: vy - 6, w: vw, h: 22 };
                let shown = if value.is_empty() { "–" } else { value.as_str() };
                if stacked {
                    c.text_fit(&ctl, p.r(Rect { y: vy, h: 22, ..vb }), shown, t.text);
                    c.text_fit(&small, p.r(Rect { y: vy + 22, h: 16, ..vb }), why, t.muted);
                } else {
                    c.text_right(&ctl, p.r(vb), shown, t.text);
                    c.text_right(&small, p.r(Rect { y: vy + 18, h: 16, ..vb }), why, t.muted);
                }
            }
        }
    }
    // Favourites: the key's own text above the list (in the head row's right half).
    if let Some((n, r)) = lay.heads.iter().find(|(n, _)| n.eq_ignore_ascii_case("FAVORITEN")) {
        let hf = if inside { &title_font } else { &head };
        let hw = hf.width(n) as f64 / t.scale;
        let band = Rect { x: r.x + hw as i32 + 16, w: r.w - hw as i32 - 16, ..*r };
        c.text_fit(&small, p.r(band), &l.fav_text, t.muted);
    }
    for (i, (r, a)) in lay.favs.iter().zip(&l.favs).enumerate() {
        sep(&mut c, *r);
        let band = Rect { x: r.x + 20, w: r.w - 40 - 72, ..*r };
        c.text_fit(&font(t, 15.0, Weight::Regular), p.r(band), a, t.text);
        if let Some(b) = lay.part(Part::FavUp(i)) {
            button(&mut c, &p, t, b, "", hover(Part::FavUp(i)), l.live, &ctl);
            let r = p.r(b);
            let (cx, cy, k) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0, p.v(1.0));
            c.poly(&[(cx, cy - 6.0 * k), (cx + 6.0 * k, cy + 4.0 * k), (cx - 6.0 * k, cy + 4.0 * k)], t.text);
        }
        if let Some(b) = lay.part(Part::FavRemove(i)) {
            button(&mut c, &p, t, b, "", hover(Part::FavRemove(i)), l.live, &ctl);
            let r = p.r(b);
            let (cx, cy, k) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0, p.v(5.0));
            let col = if hover(Part::FavRemove(i)) { t.danger } else { t.text };
            c.line((cx - k, cy - k), (cx + k, cy + k), p.v(1.6), col);
            c.line((cx - k, cy + k), (cx + k, cy - k), p.v(1.6), col);
        }
    }
    if l.favs.is_empty() {
        let r = lay.fav_add_row;
        c.text_fit(&small, p.r(Rect { x: r.x + 20, y: r.y - 4, w: r.w - 40, h: 16 }), "Noch keine Favoriten", t.muted);
    }
    if let Some(b) = lay.part(Part::FavField) {
        let f = l.field.as_ref().filter(|f| f.part == Part::FavField);
        field(&mut c, &p, t, b, "", f, hover(Part::FavField), &ctl);
        if f.is_none() {
            let r = Rect { x: b.x + 12, w: b.w - 24, ..b };
            c.text_fit(&ctl, p.r(r), "Neue Adresse, z. B. programm/terminal", t.muted);
        }
    }
    if let Some(b) = lay.part(Part::FavAdd) {
        button(&mut c, &p, t, b, "Hinzufügen", hover(Part::FavAdd), l.live, &ctl);
    }
    // The head lies over what scrolled under it.
    let hd = p.r(lay.head);
    c.fill(hd.x, hd.y, hd.w, hd.h, t.paper);
    if lay.scrolled {
        c.fill(hd.x, hd.y + hd.h - 1, hd.w, p.line().round() as i32, t.line);
    }
    let big = if lay.rail.is_some() { t.font(Family::Display, 32.0, Weight::SemiBold) } else { font(t, 28.0, Weight::Light) };
    c.text_fit(&big, p.r(lay.title), &l.title, t.text);
    c.text_fit(&small, p.r(lay.sub), &l.address, t.muted);
    match (&l.msg, l.live) {
        (_, false) => {
            c.text_fit(&small, p.r(lay.msg), "Der Einstellungsdienst antwortet nicht: Änderungen sind gerade nicht möglich.", t.danger)
        }
        (Some((m, err)), true) => c.text_fit(&small, p.r(lay.msg), m, if *err { t.danger } else { t.accent }),
        (None, true) => c.text_fit(&small, p.r(lay.msg), &l.hint, t.muted),
    };
    if let Some(b) = lay.part(Part::Undo) {
        button(&mut c, &p, t, b, "Rückgängig", hover(Part::Undo), l.live, &ctl);
    }
    if let Some(r) = &lay.rail {
        crate::page_rail::draw(&mut c, &p, t, r, l.hover.filter(|_| l.live), l.field.as_ref(), l.live);
    }
    if let Some(d) = &l.dialog {
        crate::page_confirm::draw(&mut c, &p, t, lay.content, d, l.hover);
    }
    c
}

/// WY-SET: a card's "Mehr" line: a triangle (right closed, down open), its text, and on the right how many rows
/// it holds and how many of them differ from their default.
#[allow(clippy::too_many_arguments)]
fn fold(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, text: &str, open: bool, count: usize, changed: usize, hover: bool) {
    let b = p.r(Rect { x: r.x + 20, y: r.y + (r.h - 12) / 2, w: 12, h: 12 });
    let (x, y, k) = (b.x as f32, b.y as f32, p.v(1.0));
    let col = if hover { t.accent } else { t.muted };
    if open {
        c.poly(&[(x, y + 2.0 * k), (x + 12.0 * k, y + 2.0 * k), (x + 6.0 * k, y + 10.0 * k)], col);
    } else {
        c.poly(&[(x + 2.0 * k, y), (x + 10.0 * k, y + 6.0 * k), (x + 2.0 * k, y + 12.0 * k)], col);
    }
    let right = match (count, changed) {
        (1, 0) => "1 Einstellung".to_string(),
        (n, 0) => format!("{n} Einstellungen"),
        (n, m) => format!("{n} {}, {m} geändert", if n == 1 { "Einstellung" } else { "Einstellungen" }),
    };
    let small = font(t, 13.0, Weight::Regular);
    let rw = (small.width(&right) as f64 / t.scale) as i32 + 4;
    c.text_right(&small, p.r(Rect { x: r.x + r.w - 20 - rw, y: r.y, w: rw, h: r.h }), &right, t.muted);
    c.text_fit(&font(t, 15.0, Weight::Medium), p.r(Rect { x: r.x + 44, y: r.y, w: r.w - 44 - rw - 36, h: r.h }), text, if hover { t.accent } else { t.text });
}

/// SELFOPT-1: a benchmark's nights as a line in `r` (oldest left, one slot per night, gaps where a night has no
/// value), the last night as a larger dot, the CachyOS value as a dashed line. The scale is the values' own
/// range: the chart shows the shape, the line under the label says the numbers.
fn chart(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, value: &str) {
    let (slots, cachy) = crate::leistung::chart_parse(value);
    let b = p.r(r);
    let (x0, y0, w, h) = (b.x as f32, b.y as f32, b.w as f32, b.h as f32);
    c.line((x0, y0 + h), (x0 + w, y0 + h), p.line(), t.line);
    let vals: Vec<f64> = slots.iter().flatten().copied().chain(cachy).collect();
    if vals.is_empty() || slots.is_empty() {
        return;
    }
    let (lo, hi) = vals.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, z), v| (a.min(*v), z.max(*v)));
    let pad = ((hi - lo) * 0.15).max(hi.abs() * 0.02).max(1e-9);
    let (lo, hi) = (lo - pad, hi + pad);
    let y = |v: f64| y0 + h - ((v - lo) / (hi - lo)) as f32 * h;
    let step = if slots.len() > 1 { w / (slots.len() - 1) as f32 } else { 0.0 };
    let x = |i: usize| x0 + i as f32 * step;
    if let Some(cv) = cachy {
        let cy = y(cv);
        let dash = p.v(6.0);
        let mut at = x0;
        while at < x0 + w {
            c.line((at, cy), ((at + dash).min(x0 + w), cy), p.v(1.2), t.muted);
            at += dash * 2.0;
        }
    }
    let mut prev: Option<(f32, f32)> = None;
    for (i, s) in slots.iter().enumerate() {
        match s {
            Some(v) => {
                let pt = (x(i), y(*v));
                if let Some(q) = prev {
                    c.line(q, pt, p.v(1.6), t.accent);
                }
                c.disc(pt.0, pt.1, p.v(1.8), t.accent);
                prev = Some(pt);
            }
            None => prev = None,
        }
    }
    if let Some((i, Some(v))) = slots.iter().enumerate().rev().find(|(_, s)| s.is_some()) {
        c.disc(x(i), y(*v), p.v(3.5), t.accent);
    }
}

/// From a colour row's disc to its field.
const SWATCH_GAP: i32 = page::SWATCH + 8;

#[allow(clippy::too_many_arguments)]
pub(crate) fn button(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, text: &str, hover: bool, live: bool, f: &crate::text::Font) {
    let b = p.b(r);
    c.rrect(b, p.v(8.0), if hover { t.chip } else { t.panel2 });
    c.rrect_line(b, p.v(8.0), p.line(), if hover { t.accent } else { t.line });
    if !text.is_empty() {
        c.text_mid(f, p.r(r), text, if live { t.text } else { t.muted });
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn pill(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, text: &str, on: bool, hover: bool, live: bool, f: &crate::text::Font) {
    let b = p.b(r);
    let rad = p.v(r.h as f32 / 2.0);
    if on {
        c.glow(b, rad, 6, t.glow_soft);
    }
    c.rrect(b, rad, if on { t.chip } else if hover { t.panel2 } else { t.panel });
    c.rrect_line(b, rad, p.line(), if on || hover { t.accent } else { t.line });
    let col = if on { t.accent } else if live { t.text } else { t.muted };
    c.text_mid(f, p.r(r), text, col);
}

fn switch(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, on: bool, hover: bool, live: bool) {
    let b = p.b(r);
    let rad = p.v(r.h as f32 / 2.0);
    c.rrect(b, rad, if on { alpha(t.accent, 0.35) } else { t.panel2 });
    c.rrect_line(b, rad, p.line(), if on || hover { t.accent } else { t.line });
    let rr = p.r(r);
    let knob = rr.h as f32 / 2.0 - p.v(4.0);
    let cy = rr.y as f32 + rr.h as f32 / 2.0;
    let cx = if on { (rr.x + rr.w) as f32 - rr.h as f32 / 2.0 } else { rr.x as f32 + rr.h as f32 / 2.0 };
    c.disc(cx, cy, knob, if on { t.accent } else if live { t.muted } else { t.line });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn field(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, value: &str, edit: Option<&FieldLook>, hover: bool, f: &crate::text::Font) {
    let b = p.b(r);
    c.rrect(b, p.v(8.0), t.panel2);
    c.rrect_line(b, p.v(8.0), p.line(), if edit.is_some() || hover { t.accent } else { t.line });
    let band = p.r(Rect { x: r.x + 12, w: r.w - 24, ..r });
    let text = edit.map_or(value, |e| e.text.as_str());
    c.text_fit(f, band, text, t.text);
    if let Some(e) = edit {
        let x = band.x + f.width(&e.before);
        let ch = f.height();
        c.fill(x, band.y + (band.h - ch) / 2, p.line().round() as i32 + 1, ch, t.accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Inputs, layout};

    fn look(live: bool, field: Option<FieldLook>, h: i32) -> PageLook {
        let c = Rect { x: 300, y: 160, w: 1200, h };
        let m = crate::page::tests::test_model();
        PageLook {
            layout: layout(c, &m, &Inputs { favourites: 2, scroll: 0 }),
            theme: 0,
            address: "einstellungen/aussehen".into(),
            values: m.rows.iter().map(|r| r.default.clone()).collect(),
            rows: m.rows,
            fav_text: "Favoriten".into(),
            favs: vec!["programm/terminal".into(), "einstellungen/aussehen".into()],
            target: Some("theme.accent".into()),
            hover: Some(Part::Swatch(1)),
            field,
            msg: Some(("Akzentfarbe: Geändert".into(), false)),
            dialog: None,
            live,
            title: TITLE.into(),
            hint: HINT.into(),
            subs: Vec::new(),
            flags: Vec::new(),
        }
    }

    #[test]
    fn the_page_draws_in_both_modes_and_offline() {
        assert!(crate::text::Font::has('−') && crate::text::Font::has('„') && crate::text::Font::has('ü'));
        let t = crate::theme::test_theme();
        let field = FieldLook { part: Part::Hex, text: "#3ddc".into(), before: "#3ddc".into() };
        // 4000 px: every kind of row is drawn (a long choice, a number, a colour, a text, a secret).
        for l in [look(true, None, 800), look(false, Some(field), 4000)] {
            let c = draw(&l, t);
            let p = Px::new(l.layout.content, t.scale);
            // The chosen swatch's disc is drawn in its own colour (centre pixel).
            let b = p.r(l.layout.part(Part::Swatch(0)).unwrap());
            assert_eq!(c.pixel(b.x + b.w / 2, b.y + b.h / 2), crate::canvas::pack(hex(SWATCHES[0]).unwrap()));
        }
    }

    #[test]
    fn a_chart_draws_its_nights_and_its_last_dot() {
        let t = crate::theme::test_theme();
        let mut l = look(true, None, 800);
        let m = crate::page::Model { rows: crate::leistung::rows(), sections: vec![crate::page::Section::Rows("LEISTUNG".into(), 0..crate::leistung::rows().len())], ..Default::default() };
        l.layout = layout(l.layout.content, &m, &Inputs { favourites: 0, scroll: 0 });
        l.rows = m.rows;
        let mut v = vec![String::new(); l.rows.len()];
        let i = l.rows.iter().position(|r| r.key == "leistung.verlauf.boot").unwrap();
        v[i] = format!("{};12500;12000;13000|9000", ";".repeat(26));
        l.values = v;
        l.target = None;
        l.hover = None;
        let c = draw(&l, t);
        let p = Px::new(l.layout.content, t.scale);
        let rect = l.layout.rows[i];
        let (vx, vy, vw, _) = page::control_place(&l.rows[i], rect.x, rect.y, rect.w, l.layout.narrow);
        let b = p.r(Rect { x: vx, y: vy, w: vw, h: page::CTL_H });
        let accent = crate::canvas::pack(t.accent);
        let lit = (b.x..b.x + b.w).flat_map(|x| (b.y..b.y + b.h).map(move |y| (x, y))).filter(|(x, y)| c.pixel(*x, *y) == accent).count();
        assert!(lit > 30, "the line and its dots: {lit} accent pixels");
        // The last night's dot sits at the right end.
        let right = (b.y..b.y + b.h).any(|y| (b.x + b.w - 3..b.x + b.w).any(|x| c.pixel(x, y) == accent));
        assert!(right, "the last night is at the right end");
    }
}
