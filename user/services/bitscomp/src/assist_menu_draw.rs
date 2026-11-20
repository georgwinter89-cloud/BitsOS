//! Drawing the sidebar's model list (`assist_menu_app::MenuLook`) into the panel's or the classic bar's image,
//! over the conversation: closed, a quiet pill (the page's choice pills) with the model's name and a chevron;
//! open, the address bar's suggestion list under it with the model set now marked in the accent.

use crate::assist_layout::Part;
use crate::assist_menu::{CHEVRON, MARK, PAD};
use crate::assist_menu_app::{MenuLook, label_font, row_font};
use crate::canvas::Canvas;
use crate::chrome_draw::Px;
use crate::layout::Rect;
use crate::theme::Theme;

fn chevron(c: &mut Canvas, p: &Px, r: Rect, up: bool, col: crate::theme::Rgba) {
    let b = p.b(r);
    let (cx, cy, w) = (b.x + b.w / 2.0, b.y + b.h / 2.0, p.v(CHEVRON as f32) / 2.0);
    let d = if up { -w } else { w };
    let lw = p.v(1.5).max(1.0);
    c.line((cx - w, cy - d / 2.0), (cx, cy + d / 2.0), lw, col);
    c.line((cx, cy + d / 2.0), (cx + w, cy - d / 2.0), lw, col);
}

pub fn draw(c: &mut Canvas, p: &Px, t: &Theme, m: &MenuLook) {
    let r = m.place.closed;
    let lit = m.open || m.hover == Some(Part::Model);
    let b = p.b(r);
    let rad = p.v(r.h as f32 / 2.0);
    c.rrect(b, rad, if lit { t.panel2 } else { t.panel });
    c.rrect_line(b, rad, p.line(), if lit { t.accent } else { t.line });
    let text = Rect { x: r.x + PAD, w: r.w - 2 * PAD - 6 - CHEVRON, ..r };
    c.text_fit(&label_font(t), p.r(text), &m.label, if lit { t.text } else { t.muted });
    chevron(c, p, Rect { x: r.x + r.w - PAD - CHEVRON, w: CHEVRON, ..r }, m.open, if lit { t.accent } else { t.muted });
    if m.place.rows.is_empty() {
        return;
    }
    let f = m.place.frame;
    let fb = p.b(f);
    crate::deco::hud::shadow(c, fb, p.v(12.0), &t.shadow_raised, t);
    c.rrect(fb, p.v(12.0), t.panel);
    let rf = row_font(t);
    for (i, (rr, (label, now))) in m.place.rows.iter().zip(&m.rows).enumerate() {
        if m.pick == i || m.hover == Some(Part::ModelRow(i)) {
            c.rrect(p.b(Rect { x: rr.x + 4, w: rr.w - 8, ..*rr }), p.v(8.0), t.chip);
        }
        if *now {
            let dot = p.b(Rect { x: rr.x + 14, y: rr.y + rr.h / 2 - 3, w: 6, h: 6 });
            c.disc(dot.x + dot.w / 2.0, dot.y + dot.h / 2.0, dot.w / 2.0, t.accent);
        }
        let band = Rect { x: rr.x + 14 + MARK, w: rr.w - 28 - MARK, ..*rr };
        c.text_fit(&rf, p.r(band), label, if *now { t.accent } else { t.text });
    }
    c.rrect_line(fb, p.v(12.0), p.line(), t.line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assist_menu::Place;

    #[test]
    fn closed_and_open_in_dark_and_light() {
        for mode in ["dark", "light"] {
            let mut v = crate::settings::Values::default();
            v.set("theme.mode", mode);
            let t = Theme::build(&v);
            let area = Rect { x: 0, y: 0, w: 400, h: 300 };
            let closed = Rect { x: 100, y: 20, w: 90, h: 22 };
            let rows = vec![Rect { x: 100, y: 50, w: 200, h: 30 }, Rect { x: 100, y: 80, w: 200, h: 30 }];
            let m = MenuLook {
                place: Place { closed, frame: Rect { x: 100, y: 46, w: 200, h: 68 }, rows },
                label: "Opus".into(),
                open: true,
                hover: None,
                rows: vec![("Standard".into(), false), ("Opus".into(), true)],
                pick: 0,
            };
            let p = Px::new(area, t.scale);
            let (w, h) = p.size();
            let mut c = Canvas::new(w, h);
            c.clear(t.bg);
            draw(&mut c, &p, &t, &m);
            let px = |x: i32, y: i32| c.pixel((x as f64 * t.scale) as i32, (y as f64 * t.scale) as i32);
            assert_ne!(px(150, 5), px(150, 31), "{mode}: the control is drawn");
            assert_ne!(px(250, 65), px(250, 140), "{mode}: the list is drawn");
            let bg = px(5, 5);
            assert_eq!(px(380, 280), bg, "{mode}: nothing outside");
        }
    }
}
