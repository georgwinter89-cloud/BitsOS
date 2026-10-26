//! A window header as one image (WY-5, Main.dc.html): the tile's rounded top with its border (accent and glow
//! when focused), the tab strip like a browser's (the shown tab joined to the content, with the accent top line
//! on the focused tile), per-tab close, "+N", the float and close buttons, and with `theme.layout.address_bar =
//! tile` the tile's own address bar above the tabs. Redrawn only when its `Look` changes.

use smithay::desktop::Window;

use crate::canvas::{Box2, Canvas, Glyph, glyph_into};
use crate::layout::{Place, Rect};
use crate::state::App;
use crate::tabs;
use crate::text::{Font, Weight};
use crate::theme::{Theme, alpha};
use crate::zones::{self, Part};

/// Logical pixels the frame image reaches outside the frame, for the glow lines.
pub const GLOW: i32 = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub frame: Rect,
    pub focused: bool,
    pub floating: bool,
    /// WY-5c: a floating window maximised to the whole screen (square corners, the restore glyph).
    pub maximised: bool,
    /// Every page's tab label, in stack order.
    pub labels: Vec<String>,
    pub shown: usize,
    pub hover: Option<Part>,
    /// The tile's own address bar: its text, and the text before the cursor while it is edited.
    pub address: Option<(String, Option<String>)>,
    pub hover_address: bool,
    pub theme: u64,
}

/// The labels of the pages sharing `win`'s header (just `win` for a floating window).
pub fn labels(app: &App, win: &Window, place: Place) -> Vec<String> {
    // WY-4: a hung window says so in its tab too (the label is part of `Look`, so the header is redrawn).
    let label = |w: &Window| {
        let (app_id, title) = crate::ctl::names(w);
        let l = tabs::label(&title, &app_id);
        if app.is_hung(w) { format!("{l} ({})", crate::veil::SAYS) } else { l }
    };
    match place {
        Place::Tiled { tile, .. } => app.ws.tiles[tile].stack.iter().map(label).collect(),
        Place::Floating { .. } => vec![label(win)],
    }
}

/// Logical to physical as Smithay places an image element (memory.rs `physical_size`: the rounded origin plus the
/// rounded size), so an image drawn at this size is shown 1:1, never scaled (a scaled image blurs its text).
pub fn px_rect(r: Rect, s: f64) -> Rect {
    Rect { x: px(r.x, s), y: px(r.y, s), w: px(r.w, s), h: px(r.h, s) }
}

/// A logical coordinate in output pixels, halves rounded up also below zero (WY-5c: a window at the top edge has
/// its header image at y = -2; rounding -2.5 away from zero left a one pixel row between header and content).
pub fn px(v: i32, s: f64) -> i32 {
    (v as f64 * s + 0.5).floor() as i32
}

/// The header image's place: the header plus the glow margin above and at the sides.
pub fn image_rect(f: Rect, t: &Theme) -> Rect {
    Rect { x: f.x - GLOW, y: f.y - GLOW, w: f.w + 2 * GLOW, h: t.header_height + GLOW }
}

/// The tile's own address field (layout `tile`), inside its bar.
pub fn tile_address_rect(f: Rect, t: &Theme) -> Rect {
    Rect { x: f.x + 10 + 30, y: f.y + (t.tile_bar - 24) / 2, w: (f.w - 50).max(10), h: 24 }
}

pub fn tab_font(t: &Theme) -> Font {
    Font::new(t.fs(13.0), Weight::Regular, t.scale)
}

pub fn draw(look: &Look, t: &Theme) -> Canvas {
    let s = t.scale;
    let f = look.frame;
    let origin = px_rect(image_rect(f, t), s);
    let rel = |r: Rect| {
        let p = px_rect(r, s);
        Rect { x: p.x - origin.x, y: p.y - origin.y, ..p }
    };
    let b = |r: Rect| Box2::of(rel(r));
    let v = |l: f32| l * s as f32;
    let mut c = Canvas::new(origin.w, origin.h);
    let frame = b(f);
    let rad = if look.maximised { 0.0 } else { v(t.radius as f32) };
    if look.focused {
        c.glow(frame, rad, if t.effects_gpu { 6 } else { 2 }, t.glow);
    }
    c.rrect(frame, rad, t.panel2);
    c.rrect_line(frame, rad, v(1.0).max(1.0), if look.focused { t.accent } else { t.line });
    let hovered = |p: Part| look.hover == Some(p);
    let font = tab_font(t);

    if let Some((text, cursor)) = &look.address {
        let bar = Rect { h: t.tile_bar, ..f };
        let line = rel(Rect { y: bar.y + bar.h - 1, h: 1, ..bar });
        c.fill(line.x + 1, line.y, line.w - 2, line.h.max(1), t.line);
        let nav = rel(Rect { x: f.x + 8, y: bar.y + (bar.h - 24) / 2, w: 28, h: 24 });
        glyph_into(&mut c, Glyph::Back, Box2 { w: nav.w as f32 / 2.0, ..Box2::of(nav) }, t.muted);
        glyph_into(&mut c, Glyph::Forward, Box2 { x: nav.x as f32 + nav.w as f32 / 2.0, w: nav.w as f32 / 2.0, ..Box2::of(nav) }, t.muted);
        let field = tile_address_rect(f, t);
        let fb = b(field);
        let edge = if look.focused || cursor.is_some() || look.hover_address { t.accent } else { t.line };
        c.rrect(fb, v(6.0), t.bg);
        c.rrect_line(fb, v(6.0), v(1.0).max(1.0), edge);
        let inner = rel(Rect { x: field.x + 8, w: field.w - 16, ..field });
        c.text_fit(&font, inner, text, t.text);
        if let Some(before) = cursor {
            let x = inner.x as f32 + font.width(before) as f32 + 1.0;
            c.line((x, inner.y as f32 + v(5.0)), (x, (inner.y + inner.h) as f32 - v(5.0)), v(1.5), t.accent);
        }
    }

    let strip = tabs::strip(zones::tab_frame(f, look.floating, t), look.labels.len(), look.shown, t);
    for (i, &tab) in strip.tabs.iter().enumerate() {
        let page = strip.first + i;
        let shown = page == look.shown;
        // The tab's box reaches below the header (cut off by the image), so only its top corners are round.
        let tb = b(Rect { h: tab.h + t.radius_small, ..tab });
        let r8 = v(t.radius_small as f32);
        if shown {
            if look.focused {
                c.rrect(tb, r8, t.accent);
                c.rrect(Box2 { y: tb.y + v(2.0), ..tb }, r8, t.paper);
            } else {
                c.rrect(tb, r8, t.paper);
            }
        } else if hovered(Part::Tab(page)) || hovered(Part::TabClose(page)) {
            c.rrect(tb, r8, t.panel);
        }
        let dot = rel(Rect { x: tab.x + t.tab_inner, y: tab.y + (tab.h - t.tab_dot) / 2, w: t.tab_dot, h: t.tab_dot });
        c.rrect(Box2::of(dot), v(2.0), if shown && look.focused { t.accent } else { t.muted });
        let (tx0, tx1) = tabs::text_room(tab, t);
        let room = rel(Rect { x: tx0, w: tx1 - tx0, ..tab });
        c.text_fit(&font, room, &look.labels[page], if shown { t.text } else { t.muted });
        let x = rel(tabs::close_rect(tab, t));
        let xb = Box2::of(x);
        if hovered(Part::TabClose(page)) {
            c.rrect(xb.inset(-v(3.0)), v(4.0), t.danger_chip);
            glyph_into(&mut c, Glyph::Close, xb, t.text);
        } else {
            glyph_into(&mut c, Glyph::Close, xb, alpha(if shown { t.text } else { t.muted }, 0.6));
        }
    }
    if let Some((more, n)) = strip.more {
        let r = rel(more);
        c.rrect(Box2::of(r), v(t.radius_small as f32), if hovered(Part::More) { t.chip } else { t.panel });
        c.text_mid(&font, r, &format!("+{n}"), t.muted);
    }
    let glyph = if look.floating { Glyph::Tile } else { Glyph::Float };
    let max = look.floating.then(|| (Part::Max, if look.maximised { Glyph::Restore } else { Glyph::Maximise }, zones::max_button(f, t)));
    for (part, g, r) in
        [(Part::Float, glyph, zones::float_button(f, t)), (Part::Close, Glyph::Close, zones::close_button(f, t))].into_iter().chain(max)
    {
        let bb = b(r);
        let on = hovered(part);
        if on {
            c.rrect(bb, v(t.radius_small as f32), if g == Glyph::Close { t.danger_chip } else { t.panel });
        }
        glyph_into(&mut c, g, bb.inset(v(4.0)), if on { t.text } else { t.muted });
    }
    c
}
