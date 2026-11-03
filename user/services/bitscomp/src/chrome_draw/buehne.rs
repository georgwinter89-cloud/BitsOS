//! DS-3: drawing the Bühne's header, headline and right column (Main.dc.html redrawn in Rust; geometry from
//! `chrome_render::buehne`). Each is one cached image, redrawn only when its look changes; nothing here moves by
//! itself (theme.effects cheap: no breathing dots, no radar, no beam), so a still desktop draws 0 frames.

use crate::canvas::{Box2, Canvas};
use crate::chrome::Part as C;
use crate::chrome_draw::Px;
use crate::chrome_render::buehne::{Buehne, Part, more_text};
use crate::deco::hud::{self, Glass};
use crate::layout::Rect;
use crate::status::Block;
use crate::text::{Family, Weight};
use crate::theme::{Rgba, Style, Theme, alpha};

/// What the pointer is over: one of WY's parts or one of the Bühne's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    Chrome(C),
    Buehne(Part),
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeaderLook {
    pub geo: Buehne,
    pub theme: u64,
    /// Name and context colour (0..3) of every workspace.
    pub chips: Vec<(String, usize)>,
    pub active: usize,
    /// The shown workspace's context address part (`kontext/<slug>`).
    pub slug: String,
    /// Super+L: (text, text before the cursor), drawn in the address's place.
    pub edit: Option<(String, String)>,
    pub blocks: Vec<Block>,
    pub clock: String,
    pub hover: Option<Hover>,
    /// Notices waiting (the bell's badge; none until there are notices).
    pub notices: usize,
}

/// Board px to this image's pixels.
pub fn k(t: &Theme) -> f32 {
    t.board as f32
}

pub fn header(l: &HeaderLook, t: &Theme) -> Canvas {
    let g = &l.geo;
    let p = Px::new(g.header, t.scale);
    let (w, h) = p.size();
    let kk = k(t);
    let mut c = Canvas::new(w, h);
    c.clear(hud::glass(t, Glass::Bar));
    c.fill(0, h - p.line().round() as i32, w, p.line().round() as i32, t.line);
    // The mark: a ring with a core and "BitsOS" in the display face.
    let m = p.r(g.mark);
    let (mx, my) = (m.x as f32 + 13.0 * kk, m.y as f32 + m.h as f32 / 2.0);
    c.ring(mx, my, 10.0 * kk, 2.2 * kk, t.text);
    c.disc(mx, my, 3.6 * kk, t.accent);
    let brand = t.bfont(Family::Display, 17.0, Weight::SemiBold).spaced(-0.01);
    let bx = m.x + (36.0 * kk) as i32;
    c.text_fit(&brand, Rect { x: bx, w: m.x + m.w - bx, ..m }, "BitsOS", t.text);
    divider(&mut c, m.x + m.w + (24.0 * kk) as i32, h, t);
    for (i, (name, colour)) in l.chips.iter().enumerate().take(g.shown) {
        let hover = l.hover == Some(Hover::Chrome(C::Ws(i)));
        chip(&mut c, p.b(g.chips[i]), name, t.kontext[*colour % 3], i == l.active, hover, t);
    }
    if let Some(r) = g.more {
        let b = p.b(r);
        let lit = l.active >= g.shown;
        c.rrect(b, b.h / 2.0, alpha(t.panel, 0.6));
        let edge = if lit { t.accent } else if l.hover == Some(Hover::Buehne(Part::More)) { t.line_strong } else { t.line };
        c.rrect_line(b, b.h / 2.0, kk, edge);
        let f = t.bfont(Family::Mono, 12.0, Weight::Medium);
        c.text_mid(&f, p.r(r), &more_text(l.chips.len() - g.shown), if lit { t.text } else { t.muted });
    }
    // The new workspace: a dashed round chip with a plus.
    let nb = p.b(g.ws_new);
    let (cx, cy, rr) = (nb.x + nb.w / 2.0, nb.y + nb.h / 2.0, nb.w / 2.0 - kk / 2.0);
    let hover_new = l.hover == Some(Hover::Chrome(C::WsNew));
    let col = if hover_new { t.muted } else { t.line_strong };
    c.arc(cx, cy, rr, kk, Some((4.0 * kk, 3.0 * kk, 0.0)), col);
    for (dx, dy) in [(1.0, 0.0), (0.0, 1.0)] {
        c.line((cx - 6.0 * kk * dx, cy - 6.0 * kk * dy), (cx + 6.0 * kk * dx, cy + 6.0 * kk * dy), 1.6 * kk, col);
    }
    address(&mut c, &p, l, t);
    blocks(&mut c, p.r(g.blocks), &l.blocks, t);
    // Claude's ring (its own sprite lies over this image), the sun and the bell.
    let rb = p.b(g.ring);
    if l.hover == Some(Hover::Chrome(C::Ring)) {
        c.disc(rb.x + rb.w / 2.0, rb.y + rb.h / 2.0, rb.w / 2.0, t.chip);
    }
    let round = |c: &mut Canvas, b: Box2, edge: Rgba| c.rrect_line(b, b.w / 2.0, kk, edge);
    let sb = p.b(g.sun);
    round(&mut c, sb, if l.hover == Some(Hover::Buehne(Part::Sun)) { t.line_strong } else { t.line });
    sun(&mut c, sb.x + sb.w / 2.0, sb.y + sb.h / 2.0, kk, t.text);
    let bb = p.b(g.bell);
    let lit = l.hover == Some(Hover::Buehne(Part::Bell));
    round(&mut c, bb, alpha(t.accent, if lit { 0.85 } else { 0.5 }));
    bell(&mut c, bb.x + bb.w / 2.0, bb.y + bb.h / 2.0, kk, t.accent);
    if l.notices > 0 {
        let f = t.bfont(Family::Ui, 11.0, Weight::Bold);
        let s = l.notices.to_string();
        let bw = (f.width(&s) as f32 + 10.0 * kk).max(18.0 * kk);
        let badge = Box2 { x: bb.x + bb.w + 4.0 * kk - bw, y: bb.y - 4.0 * kk, w: bw, h: 18.0 * kk };
        c.rrect(badge, badge.h / 2.0, t.accent);
        let r = Rect { x: badge.x as i32, y: badge.y as i32, w: badge.w as i32, h: badge.h as i32 };
        c.text_mid(&f, r, &s, t.on_accent);
    }
    divider(&mut c, (bb.x + bb.w) as i32 + (22.0 * kk) as i32, h, t);
    clock(&mut c, p.r(g.clock), &l.clock, t);
    c
}

fn divider(c: &mut Canvas, x: i32, h: i32, t: &Theme) {
    let kk = k(t);
    let dh = (24.0 * kk) as i32;
    c.fill(x, (h - dh) / 2, kk.round().max(1.0) as i32, dh, t.line);
}

/// A workspace chip: the shown one filled with its context colour and ringed in the accent, the others glass
/// with a dot of their colour.
fn chip(c: &mut Canvas, b: Box2, name: &str, colour: Rgba, active: bool, hover: bool, t: &Theme) {
    let kk = k(t);
    let r = b.h / 2.0;
    let (dot, ink, weight) = if active {
        c.rrect_line(Box2 { x: b.x - 3.0 * kk, y: b.y - 3.0 * kk, w: b.w + 6.0 * kk, h: b.h + 6.0 * kk }, r + 3.0 * kk, kk, t.accent);
        c.rrect(b, r, colour);
        (t.on_kontext, t.on_kontext, Weight::SemiBold)
    } else {
        c.rrect(b, r, alpha(t.panel, 0.6));
        c.rrect_line(b, r, kk, if hover { t.line_strong } else { t.line });
        (colour, if hover { t.text } else { t.muted }, Weight::Medium)
    };
    c.disc(b.x + 18.0 * kk, b.y + b.h / 2.0, 4.0 * kk, dot);
    let f = t.bfont(Family::Ui, 13.0, weight);
    let x = (b.x + 30.0 * kk) as i32;
    let band = Rect { x, y: b.y as i32, w: (b.x + b.w) as i32 - x - (10.0 * kk) as i32, h: b.h as i32 };
    c.text_fit(&f, band, name, ink);
}

/// The context address (read-only), or the text being edited with Super+L in its place.
fn address(c: &mut Canvas, p: &Px, l: &HeaderLook, t: &Theme) {
    let kk = k(t);
    let a = p.r(l.geo.address);
    let mono = t.bfont(Family::Mono, 12.0, Weight::Regular);
    match &l.edit {
        Some((text, before)) => {
            let b = Box2::of(a);
            c.rrect(b, 10.0 * kk, t.panel2);
            c.rrect_line(b, 10.0 * kk, kk, t.accent);
            let f = t.bfont(Family::Ui, 14.0, Weight::Regular);
            let x = a.x + (14.0 * kk) as i32;
            let band = Rect { x, w: a.w - (28.0 * kk) as i32, ..a };
            if text.is_empty() {
                c.text_fit(&f, band, "Adresse eingeben …", t.muted);
            } else {
                c.text_fit(&f, band, text, t.text);
            }
            let cx = (x + f.width(before)) as f32 + 1.0;
            c.line((cx, a.y as f32 + 8.0 * kk), (cx, (a.y + a.h) as f32 - 8.0 * kk), 1.5 * kk, t.accent);
        }
        None => {
            let head = "kontext/";
            let (hw, sw) = (mono.width(head), mono.width(&l.slug));
            let x = a.x + ((a.w - hw - sw) / 2).max(0);
            let wx = c.text_fit(&mono, Rect { x, w: (a.x + a.w - x).max(0), ..a }, head, t.muted);
            c.text_fit(&mono, Rect { x: x + wx, w: (a.x + a.w - x - wx).max(0), ..a }, &l.slug, t.text);
        }
    }
}

/// The status blocks moved up: a small label over the value, as many as fit their fixed place.
fn blocks(c: &mut Canvas, r: Rect, blocks: &[Block], t: &Theme) {
    let kk = k(t);
    let lab = t.bfont(Family::Mono, 10.0, Weight::Medium).spaced(0.08);
    let val = t.bfont(Family::Mono, 13.0, Weight::Regular);
    // As many as fit, right-aligned against Claude's ring (the room left of them goes to the address).
    let gap = (22.0 * kk) as i32;
    let mut widths = Vec::new();
    let mut total = -gap;
    for b in blocks {
        let bw = lab.width(&b.label.to_uppercase()).max(val.width(&b.value));
        if total + gap + bw > r.w {
            break;
        }
        total += gap + bw;
        widths.push(bw);
    }
    let mut x = r.x + r.w - total.max(0);
    for (b, bw) in blocks.iter().zip(widths) {
        let label = b.label.to_uppercase();
        let top = r.y + (r.h - lab.height() - val.height() - (2.0 * kk) as i32) / 2;
        c.text(&lab, x, top, &label, t.muted);
        c.text(&val, x, top + lab.height() + (2.0 * kk) as i32, &b.value, t.text);
        x += bw + gap;
    }
}

/// "14:32" big in mono and "Mi 28.10." beside it, from the status clock ("Mi 28.10. · 14:32[ UTC]").
fn clock(c: &mut Canvas, r: Rect, text: &str, t: &Theme) {
    let kk = k(t);
    let (date, time) = text.split_once(" · ").unwrap_or(("", text));
    // No time zone: the status clock's " UTC" goes to the date, the time stays the big digits.
    let (time, date) = match time.split_once(' ') {
        Some((tm, zone)) => (tm, format!("{date} {zone}")),
        None => (time, date.to_string()),
    };
    let tf = t.bfont(Family::Mono, 20.0, Weight::Medium);
    let df = t.bfont(Family::Ui, 13.0, Weight::Regular);
    let tw = c.text_fit(&tf, r, time, t.text);
    let x = r.x + tw + (8.0 * kk) as i32;
    // The date's baseline near the time's: both centred, the smaller one moved down a little.
    let band = Rect { x, y: r.y + (2.0 * kk) as i32, w: (r.x + r.w - x).max(0), h: r.h };
    c.text_fit(&df, band, &date, t.muted);
}

/// The sun of the design's header (a ring with eight rays), centred at (x, y).
fn sun(c: &mut Canvas, x: f32, y: f32, kk: f32, col: Rgba) {
    c.ring(x, y, 3.5 * kk, 1.6 * kk, col);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (dx, dy) = (a.cos(), a.sin());
        c.line((x + dx * 5.5 * kk, y + dy * 5.5 * kk), (x + dx * 7.5 * kk, y + dy * 7.5 * kk), 1.6 * kk, col);
    }
}

/// The bell (dome, sides, flared rim, clapper), its 18 px box centred at (x, y).
fn bell(c: &mut Canvas, x: f32, y: f32, kk: f32, col: Rgba) {
    let (ox, oy) = (x - 9.0 * kk, y - 9.0 * kk);
    let at = |px: f32, py: f32| (ox + px * kk, oy + py * kk);
    let wd = 1.6 * kk;
    let r = 4.5 * kk;
    let half = std::f32::consts::PI * r;
    let (cx, cy) = at(9.0, 8.0);
    c.arc(cx, cy, r, wd, Some((half, half, 1.5 * half)), col);
    for (a, b) in [((4.5, 8.0), (4.5, 12.5)), ((13.5, 8.0), (13.5, 12.5)), ((4.5, 12.5), (3.0, 14.0)), ((13.5, 12.5), (15.0, 14.0)), ((3.0, 14.0), (15.0, 14.0))] {
        c.line(at(a.0, a.1), at(b.0, b.1), wd, col);
    }
    let rc = 1.7 * kk;
    let hc = std::f32::consts::PI * rc;
    let (kx, ky) = at(9.0, 15.5);
    c.arc(kx, ky, rc, wd, Some((hc, hc, 0.5 * hc)), col);
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeadlineLook {
    pub rect: Rect,
    pub theme: u64,
    pub slug: String,
    pub text: String,
}

/// The Absicht over the stage: "ABSICHT · KONTEXT/…" with a dot, the typed headline in the display face.
pub fn headline(l: &HeadlineLook, t: &Theme) -> Canvas {
    let kk = k(t);
    let p = Px::new(l.rect, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    c.disc(4.0 * kk, 8.0 * kk, 4.0 * kk, t.accent);
    let lab = t.style(Style::Label);
    let label = Theme::label(Style::Label, &format!("Absicht · kontext/{}", l.slug));
    let x = (18.0 * kk) as i32;
    c.text_fit(&lab, Rect { x, y: 0, w: w - x, h: (16.0 * kk) as i32 }, &label, t.muted);
    let f = t.style(Style::Absicht);
    let y = (28.0 * kk) as i32;
    c.text_fit(&f, Rect { x: 0, y, w, h: (48.0 * kk) as i32 }, &l.text, t.text);
    c
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnLook {
    pub map: Rect,
    pub next: Rect,
    pub theme: u64,
    pub workspaces: usize,
}

/// How far an image reaches around its panel for brackets and the shadow's nearer half (board px).
pub const MARGIN: f32 = 24.0;

/// The image rect of a panel: `r` with the margin, kept off `keep_left` (the tiles' edge) on the left.
pub fn padded(r: Rect, t: &Theme, keep_left: i32) -> Rect {
    let m = t.bl(MARGIN);
    let x = (r.x - m).max(keep_left);
    Rect { x, y: r.y - m, w: r.x + r.w + m - x, h: r.h + 2 * m }
}

/// A frame of the design (glass card, radius-a, accent edge at 18 %, the raised shadow), optionally bracketed.
pub fn frame(c: &mut Canvas, b: Box2, t: &Theme, brackets: bool) {
    let kk = k(t);
    let r = t.radius_card as f32 * kk;
    hud::shadow(c, b, r, &t.shadow_raised, t);
    c.rrect(b, r, hud::glass(t, Glass::Card));
    c.rrect_line(b, r, kk, alpha(t.accent, 0.18));
    if brackets {
        hud::brackets(c, b, t);
    }
}

/// The right column: "Übersicht" (the map comes with the overview) and "Als Nächstes" (filled while an assistant
/// is connected), each as its frame and title.
pub fn column(l: &ColumnLook, t: &Theme, area: Rect) -> Canvas {
    let kk = k(t);
    let p = Px::new(area, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let title = t.bfont(Family::Display, 20.0, Weight::SemiBold);
    let lab = t.style(Style::Label);
    let n = l.workspaces;
    let count = format!("Karte · {n} {}", if n == 1 { "Arbeitsraum" } else { "Arbeitsräume" });
    for (r, head, label, lit, br) in [(l.map, "Übersicht", count, false, true), (l.next, "Als Nächstes", "von Claude".to_string(), true, false)] {
        let b = p.b(r);
        frame(&mut c, b, t, br);
        let pr = p.r(r);
        let band = Rect { x: pr.x + (24.0 * kk) as i32, y: pr.y + (22.0 * kk) as i32, w: pr.w - (48.0 * kk) as i32, h: (24.0 * kk) as i32 };
        let tw = c.text_fit(&title, band, head, t.text);
        let rest = Rect { x: band.x + tw + (12.0 * kk) as i32, w: band.w - tw - (12.0 * kk) as i32, ..band };
        c.text_right(&lab, rest, &Theme::label(Style::Label, &label), if lit { t.accent } else { t.muted });
    }
    c
}
