//! The circle menu in the holographic style of Details.dc.html (WY-5): a translucent accent band under the
//! items, a dashed outer ring and a thin inner ring, the centre disc; the items are pills, the hovered one lit
//! (accent border, chip fill, glow lines). One image, centred on the menu (size from `menu::extent`).

use crate::canvas::{Box2, Canvas};
use crate::layout::Rect;
use crate::menu::{Items, RING_IN, RING_OUT, extent, item_rect};
use crate::text::{Font, Weight};
use crate::theme::{Theme, alpha};

/// Text padding inside an item.
pub const ITEM_PAD: i32 = 10;

pub fn item_font(t: &Theme) -> Font {
    Font::new(t.fs(13.0), Weight::Regular, t.scale)
}

pub fn menu(items: &Items, hover: Option<usize>, t: &Theme) -> Canvas {
    let s = t.scale;
    let (ex, ey) = extent(t);
    let px = |v: i32| (v as f64 * s).round() as i32;
    let v = |l: i32| (l as f64 * s) as f32;
    let font = item_font(t);
    let mut c = Canvas::new(px(2 * ex), px(2 * ey));
    let (cx, cy) = (px(ex) as f32, px(ey) as f32);
    let r = v(t.menu_radius);
    // A calm backdrop so the menu reads over any window, then the band behind the items: wide and faint (the
    // mock-up's stroke-opacity 0.25, 44 px; scaled with theme.glow).
    c.disc(cx, cy, r + v(RING_OUT), alpha(t.panel, 0.82));
    c.ring(cx, cy, r, v(t.menu_item_size.1 + 16), alpha(t.accent, 0.2 * t.glow[3] / 0.18));
    c.arc(cx, cy, r + v(RING_OUT), v(1).max(1.0), Some((v(4), v(8), 0.0)), t.accent);
    c.ring(cx, cy, r - v(RING_IN), v(1).max(1.0), alpha(t.accent, 0.8));
    c.disc(cx, cy, v(22), t.chip);
    c.ring(cx, cy, v(22) - 0.75, 1.5 * s as f32, t.accent);
    for (d, it) in items.iter().enumerate() {
        let Some(it) = it else { continue };
        let ir = item_rect(d, t);
        let b = Box2 { x: cx + v(ir.x), y: cy + v(ir.y), w: v(ir.w), h: v(ir.h) };
        let on = hover == Some(d) && it.enabled;
        let rad = v(t.radius_small);
        if on {
            c.glow(b, rad, if t.effects_gpu { 6 } else { 2 }, t.glow);
        }
        c.rrect(b, rad, t.panel2);
        if on {
            c.rrect(b, rad, t.chip);
        }
        c.rrect_line(b, rad, (s as f32).max(1.0), if on { t.accent } else { t.line });
        let band = Rect { x: b.x as i32 + px(ITEM_PAD), y: b.y as i32, w: b.w as i32 - 2 * px(ITEM_PAD), h: b.h as i32 };
        c.text_mid(&font, band, it.label, if it.enabled { t.text } else { t.muted });
    }
    c
}
