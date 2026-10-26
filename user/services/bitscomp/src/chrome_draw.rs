//! Drawing the chrome of `chrome` (Main.dc.html, WY-5): each bar is one image at output resolution, redrawn only
//! when what it shows changes (`chrome_render` keeps them). Sizes are the mock-up's CSS px.

use crate::canvas::{Box2, Canvas, Glyph, glyph_into};
use crate::chrome::{Chrome, Part, fav_font, fav_label};
use crate::desk::Ring;
use crate::header::px_rect;
use crate::layout::Rect;
use crate::status::Block;
use crate::suggest::Row;
use crate::text::{Font, Weight};
use crate::theme::Theme;

/// Logical rects to pixels of an image whose top left is `origin` (logical).
pub struct Px {
    s: f64,
    o: Rect,
}

impl Px {
    pub fn new(origin: Rect, s: f64) -> Px {
        Px { s, o: px_rect(origin, s) }
    }
    pub fn size(&self) -> (i32, i32) {
        (self.o.w, self.o.h)
    }
    pub fn r(&self, r: Rect) -> Rect {
        let p = px_rect(r, self.s);
        Rect { x: p.x - self.o.x, y: p.y - self.o.y, ..p }
    }
    pub fn b(&self, r: Rect) -> Box2 {
        Box2::of(self.r(r))
    }
    pub fn v(&self, l: f32) -> f32 {
        l * self.s as f32
    }
    pub fn line(&self) -> f32 {
        (self.s as f32).max(1.0)
    }
}

pub fn font(t: &Theme, px: f32, w: Weight) -> Font {
    Font::new(t.fs(px), w, t.scale)
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopLook {
    pub chrome: Chrome,
    pub theme: u64,
    /// What the field shows: the selected tab's address or the text being edited.
    pub text: String,
    pub cursor: Option<String>,
    pub hover: Option<Part>,
    pub ring: Ring,
    /// The context chip slot (none until the context service, CX-6).
    pub context: Option<String>,
}

pub fn top(l: &TopLook, t: &Theme) -> Canvas {
    let c0 = &l.chrome;
    let p = Px::new(c0.top, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.panel);
    let bottom = p.r(Rect { y: c0.top.y + c0.top.h - 1, h: 1, ..c0.top });
    c.fill(0, bottom.y, w, bottom.h.max(1), t.line);
    // The mark: the mock-up's ring with a core, and "BitsOS".
    let m = p.r(c0.mark);
    let (mx, my) = (m.x as f32 + p.v(13.0), m.y as f32 + p.v(13.0));
    c.ring(mx, my, p.v(10.0), p.v(2.0), t.accent);
    c.disc(mx, my, p.v(4.0), t.accent);
    let brand = font(t, 18.0, Weight::SemiBold).spaced(0.04);
    c.text_fit(&brand, Rect { x: m.x + p.v(34.0) as i32, w: m.w - p.v(34.0) as i32, ..m }, "BitsOS", t.text);
    // WY-5c: the folded workspace list's button, left of the mark.
    if let Some(r) = c0.ws_button {
        let b = p.b(r);
        let on = l.hover == Some(Part::WsButton);
        c.rrect(b, p.v(8.0), if on { t.chip } else { t.panel2 });
        c.rrect_line(b, p.v(8.0), p.line(), if on { t.muted } else { t.line });
        glyph_into(&mut c, Glyph::Workspaces, b.inset(p.v(5.0)), if on { t.text } else { t.muted });
    }
    // Back and forward: no tab history yet, so both are shown as not available.
    for (r, g, part) in [(c0.back, Glyph::Back, Part::Back), (c0.forward, Glyph::Forward, Part::Forward)] {
        let b = p.b(r);
        c.rrect(b, p.v(8.0), t.panel2);
        c.rrect_line(b, p.v(8.0), p.line(), if l.hover == Some(part) { t.muted } else { t.line });
        glyph_into(&mut c, g, b.inset(p.v(6.0)), t.muted);
    }
    let a = p.b(c0.address);
    let top_mode = t.layout.address_top;
    let lit = top_mode || l.cursor.is_some() || l.hover == Some(Part::Address);
    if lit {
        c.rrect_line(a.inset(-p.v(3.0)), p.v(13.0), p.v(3.0), t.glow);
    }
    c.rrect(a, p.v(10.0), t.panel2);
    c.rrect_line(a, p.v(10.0), p.line(), if lit { t.accent } else { t.line });
    let ar = p.r(c0.address);
    let mut x = ar.x + p.v(14.0) as i32;
    let right = ar.x + ar.w - p.v(14.0) as i32;
    if let Some(ctx) = &l.context {
        let lab = font(t, 12.0, Weight::SemiBold).spaced(0.08);
        x += c.text_fit(&lab, Rect { x, w: right - x, ..ar }, "KONTEXT", t.accent) + p.v(10.0) as i32;
        let chip = font(t, 13.0, Weight::Regular);
        let cw = chip.width(ctx) + p.v(20.0) as i32;
        let cb = Box2 { x: x as f32, y: ar.y as f32 + (ar.h as f32 - p.v(24.0)) / 2.0, w: cw as f32, h: p.v(24.0) };
        c.rrect(cb, cb.h / 2.0, t.chip);
        c.text_mid(&chip, Rect { x, w: cw, ..ar }, ctx, t.text);
        x += cw + p.v(10.0) as i32;
    }
    let f16 = font(t, 16.0, Weight::Regular);
    let hint = font(t, 12.0, Weight::Regular);
    let hint_text = "Alt+Enter: neuer Tab";
    let hint_w = if top_mode && hint.width(hint_text) * 3 < right - x { hint.width(hint_text) + p.v(10.0) as i32 } else { 0 };
    if hint_w > 0 {
        c.text_right(&hint, Rect { x, w: right - x, ..ar }, hint_text, t.muted);
    }
    let room = Rect { x, w: right - x - hint_w, ..ar };
    if l.text.is_empty() && l.cursor.is_none() {
        let ph = if top_mode { "Adresse, Suche oder eine Frage an Claude …" } else { "Suchen, öffnen oder Claude fragen …" };
        c.text_fit(&f16, room, ph, t.muted);
    } else {
        c.text_fit(&f16, room, &l.text, t.text);
    }
    if let Some(before) = &l.cursor {
        let cx = (x + f16.width(before)) as f32 + 1.0;
        c.line((cx, ar.y as f32 + p.v(10.0)), (cx, (ar.y + ar.h) as f32 - p.v(10.0)), p.v(1.5), t.accent);
    }
    // The ring button: Claude's ring, small; it folds the sidebar in and out.
    let rb = p.r(c0.ring);
    if l.hover == Some(Part::Ring) {
        c.disc(rb.x as f32 + rb.w as f32 / 2.0, rb.y as f32 + rb.h as f32 / 2.0, rb.w as f32 / 2.0, t.chip);
    }
    crate::ring::draw(&mut c, rb.x as f32 + rb.w as f32 / 2.0, rb.y as f32 + rb.h as f32 / 2.0, p.v(40.0) * 48.0 / 44.0 * 0.9, l.ring, 0.0, t);
    c
}

#[derive(Clone, Debug, PartialEq)]
pub struct FavLook {
    pub chrome: Chrome,
    pub theme: u64,
    pub items: Vec<String>,
    pub hover: Option<usize>,
}

pub fn favs(l: &FavLook, t: &Theme) -> Canvas {
    let bar = l.chrome.favs.unwrap_or_default();
    let p = Px::new(bar, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.panel);
    let line = p.r(Rect { y: bar.y + bar.h - 1, h: 1, ..bar });
    c.fill(0, line.y, w, line.h.max(1), t.line);
    let f = fav_font(t);
    for (i, (r, a)) in l.chrome.fav_items.iter().zip(&l.items).enumerate() {
        let on = l.hover == Some(i);
        if on {
            c.rrect(p.b(*r), p.v(6.0), t.chip);
        }
        c.text_mid(&f, p.r(*r), fav_label(a), if on { t.text } else { t.muted });
    }
    c
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListLook {
    pub chrome: Chrome,
    pub theme: u64,
    /// Name and "2 Kacheln · 3 Tabs" per workspace.
    pub entries: Vec<(String, String)>,
    pub active: usize,
    pub hover: Option<Part>,
}

pub fn list(l: &ListLook, t: &Theme) -> Canvas {
    let c0 = &l.chrome;
    let p = Px::new(c0.list, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.panel);
    let edge = p.r(Rect { x: c0.list.x + c0.list.w - 1, w: 1, ..c0.list });
    c.fill(edge.x, 0, edge.w.max(1), h, t.line);
    let head = font(t, 11.0, Weight::SemiBold).spaced(0.12);
    c.text_fit(&head, p.r(c0.ws_title), "ARBEITSRÄUME", t.muted);
    // WY-5c: the fold button at the list's top edge; on a list revealed over the tiles it pins the list.
    let fb = p.b(c0.ws_fold);
    let on = l.hover == Some(Part::WsFold);
    if on {
        c.rrect(fb, p.v(8.0), t.panel2);
    }
    let g = if c0.list_docked { Glyph::Fold } else { Glyph::Pin };
    glyph_into(&mut c, g, fb.inset(p.v(4.0)), if on { t.text } else { t.muted });
    let (name_on, name, sub) = (font(t, 15.0, Weight::SemiBold), font(t, 15.0, Weight::Medium), font(t, 12.0, Weight::Regular));
    for (i, r) in c0.ws_items.iter().take(c0.ws_shown()).enumerate() {
        let Some((n, s)) = l.entries.get(i) else { break };
        let b = p.b(*r);
        let active = i == l.active;
        if active {
            c.rrect(b, p.v(10.0), t.chip);
            c.rrect_line(b, p.v(10.0), p.line(), t.accent);
        } else if l.hover == Some(Part::Ws(i)) {
            c.rrect(b, p.v(10.0), t.panel2);
        }
        let inner = p.r(Rect { x: r.x + 12, w: r.w - 24, ..*r });
        if r.h >= crate::chrome::WS_FULL {
            let top = Rect { y: inner.y + p.v(10.0) as i32, h: p.v(20.0) as i32, ..inner };
            c.text_fit(if active { &name_on } else { &name }, top, n, t.text);
            c.text_fit(&sub, Rect { y: top.y + top.h + p.v(2.0) as i32, h: p.v(16.0) as i32, ..inner }, s, t.muted);
        } else {
            c.text_fit(if active { &name_on } else { &name }, inner, n, t.text);
        }
    }
    if c0.ws_new.y + c0.ws_new.h <= c0.home.y {
        let b = p.b(c0.ws_new);
        if l.hover == Some(Part::WsNew) {
            c.rrect(b, p.v(10.0), t.panel2);
        }
        dashed(&mut c, b, p.v(10.0), p.line(), t, if l.hover == Some(Part::WsNew) { t.panel2 } else { t.panel });
        let r = p.r(Rect { x: c0.ws_new.x + 12, w: c0.ws_new.w - 24, ..c0.ws_new });
        c.text_fit(&font(t, 14.0, Weight::Regular), r, "+ Neuer Arbeitsraum", t.muted);
    }
    let hb = p.r(c0.home);
    if l.hover == Some(Part::Home) {
        c.rrect(Box2::of(hb), p.v(10.0), t.panel2);
    }
    let icon = Box2 { x: hb.x as f32 + p.v(8.0), y: hb.y as f32 + (hb.h as f32 - p.v(20.0)) / 2.0, w: p.v(20.0), h: p.v(20.0) };
    glyph_into(&mut c, Glyph::Home, icon, t.muted);
    let tx = hb.x + p.v(34.0) as i32;
    c.text_fit(&font(t, 14.0, Weight::Regular), Rect { x: tx, w: hb.x + hb.w - tx, ..hb }, "Startseite", t.muted);
    c
}

/// A dashed border: the full line, then gaps painted over its straight parts in the colour below (`under`).
fn dashed(c: &mut Canvas, b: Box2, r: f32, w: f32, t: &Theme, under: crate::theme::Rgba) {
    c.rrect_line(b, r, w, t.line);
    let step = (4.0 * w).round().max(3.0) as i32;
    let (x0, x1) = ((b.x + r) as i32, (b.x + b.w - r) as i32);
    let (y0, y1) = ((b.y + r) as i32, (b.y + b.h - r) as i32);
    let lw = w.ceil() as i32;
    for x in (x0..x1).step_by(2 * step as usize) {
        c.fill(x + step, b.y as i32, step.min(x1 - x - step).max(0), lw, under);
        c.fill(x + step, (b.y + b.h) as i32 - lw, step.min(x1 - x - step).max(0), lw, under);
    }
    for y in (y0..y1).step_by(2 * step as usize) {
        c.fill(b.x as i32, y + step, lw, step.min(y1 - y - step).max(0), under);
        c.fill((b.x + b.w) as i32 - lw, y + step, lw, step.min(y1 - y - step).max(0), under);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SideLook {
    pub rect: Rect,
    pub theme: u64,
    pub name: String,
    pub ring: Ring,
}

/// The line the sidebar shows until the conversation comes (CL-3): no made-up chat.
pub fn side_note(name: &str) -> String {
    format!("Hier sprichst du bald mit {name}.")
}

pub fn side(l: &SideLook, t: &Theme) -> Canvas {
    let p = Px::new(l.rect, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.clear(t.panel);
    c.fill(0, 0, p.line().round() as i32, h, t.line);
    let r = l.rect;
    let ring = p.r(Rect { x: r.x + 16, y: r.y + 18, w: 44, h: 44 });
    crate::ring::draw(&mut c, ring.x as f32 + ring.w as f32 / 2.0, ring.y as f32 + ring.h as f32 / 2.0, p.v(44.0) * 48.0 / 44.0 * 0.92, l.ring, 0.0, t);
    let tx = r.x + 16 + 44 + 12;
    c.text_fit(&font(t, 16.0, Weight::SemiBold), p.r(Rect { x: tx, y: r.y + 20, w: r.w - (tx - r.x) - 16, h: 22 }), &l.name, t.text);
    let state = font(t, 12.0, Weight::Regular).spaced(0.06);
    c.text_fit(&state, p.r(Rect { x: tx, y: r.y + 42, w: r.w - (tx - r.x) - 16, h: 16 }), l.ring.word(), t.accent);
    let note = Rect { x: r.x + 16, y: r.y + 18 + 44 + 14, w: r.w - 32, h: 0 };
    let f = font(t, 14.0, Weight::Regular);
    let lines = wrap(&f, &side_note(&l.name), p.r(note).w - p.v(28.0) as i32);
    let lh = p.v(21.0) as i32;
    let nb = p.r(Rect { h: 24, ..note });
    let bh = lines.len() as i32 * lh + p.v(24.0) as i32;
    c.rrect(Box2 { h: bh as f32, ..Box2::of(nb) }, p.v(12.0), t.panel2);
    for (i, line) in lines.iter().enumerate() {
        c.text(&f, nb.x + p.v(14.0) as i32, nb.y + p.v(12.0) as i32 + i as i32 * lh + (lh - f.height()) / 2, line, t.text);
    }
    c
}

/// Words of `s` in lines of at most `max` pixels.
pub fn wrap(f: &Font, s: &str, max: i32) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in s.split_whitespace() {
        match out.last_mut() {
            Some(l) if f.width(&format!("{l} {word}")) <= max => {
                l.push(' ');
                l.push_str(word);
            }
            _ => out.push(word.to_string()),
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatusLook {
    pub rect: Rect,
    pub theme: u64,
    pub blocks: Vec<Block>,
    pub clock: String,
}

/// Logical pixels above the status bar its image covers, for the glow lines going up.
pub const STATUS_GLOW: i32 = 4;

pub fn status(l: &StatusLook, t: &Theme) -> Canvas {
    let area = Rect { y: l.rect.y - STATUS_GLOW, h: l.rect.h + STATUS_GLOW, ..l.rect };
    let p = Px::new(area, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let bar = p.r(l.rect);
    c.fill(0, bar.y, w, h - bar.y, t.panel);
    let line = p.line().round() as i32;
    c.fill(0, bar.y, w, line, t.accent);
    for (i, a) in [t.glow, t.glow_soft].into_iter().enumerate() {
        c.fill(0, bar.y - line * (i as i32 + 1), w, line, a);
    }
    let label = font(t, 13.0, Weight::Regular);
    let value = font(t, 13.0, Weight::Medium);
    let mut x = bar.x + p.v(16.0) as i32;
    let right = bar.x + bar.w - p.v(16.0) as i32;
    let clock_w = label.width(&l.clock);
    for b in &l.blocks {
        let need = label.width(&b.label) + p.v(5.0) as i32 + value.width(&b.value);
        if x + need > right - clock_w - p.v(22.0) as i32 {
            break;
        }
        x += c.text_fit(&label, Rect { x, w: need, ..bar }, &b.label, t.muted) + p.v(5.0) as i32;
        x += c.text_fit(&value, Rect { x, w: need, ..bar }, &b.value, t.text) + p.v(22.0) as i32;
    }
    c.text_right(&label, Rect { x: right - clock_w, w: clock_w, ..bar }, &l.clock, t.text);
    c
}

#[derive(Clone, Debug, PartialEq)]
pub struct SuggestLook {
    pub frame: Rect,
    pub rows: Vec<(Option<Rect>, Rect)>,
    pub items: Vec<Row>,
    pub pick: Option<usize>,
    pub hover: Option<usize>,
    pub theme: u64,
}

pub fn suggest(l: &SuggestLook, t: &Theme) -> Canvas {
    let p = Px::new(l.frame, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let all = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, p.v(12.0), t.panel);
    let head = font(t, 11.0, Weight::SemiBold).spaced(0.12);
    let (name, addr) = (font(t, 15.0, Weight::Regular), font(t, 12.0, Weight::Regular));
    for (i, ((hd, r), row)) in l.rows.iter().zip(&l.items).enumerate() {
        if let Some(hr) = hd {
            c.text_fit(&head, p.r(Rect { x: hr.x + 14, y: hr.y + 8, w: hr.w - 28, h: hr.h - 8 }), row.group, t.muted);
        }
        let rr = p.r(*r);
        if l.pick == Some(i) || l.hover == Some(i) {
            c.fill(rr.x, rr.y, rr.w, rr.h, t.chip);
        }
        let inner = p.r(Rect { x: r.x + 14, w: r.w - 28, ..*r });
        let aw = addr.width(&row.addr).min(inner.w / 2);
        c.text_fit(&name, Rect { w: inner.w - aw - p.v(10.0) as i32, ..inner }, &row.name, t.text);
        c.text_right(&addr, inner, &row.addr, t.muted);
    }
    c.rrect_line(all, p.v(12.0), p.line(), t.line);
    c
}

/// A short notice under the address bar ("Webseiten kann BitsOS noch nicht öffnen: …").
pub fn notice(text: &str, rect: Rect, t: &Theme) -> Canvas {
    let p = Px::new(rect, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let b = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(b, p.v(8.0), t.panel2);
    c.rrect_line(b, p.v(8.0), p.line(), t.accent);
    c.text_mid(&font(t, 13.0, Weight::Regular), Rect { x: p.v(12.0) as i32, y: 0, w: w - p.v(24.0) as i32, h }, text, t.text);
    c
}
