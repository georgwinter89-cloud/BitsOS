//! Small CPU images for everything bitscomp draws itself: chrome, headers, the circle menu, the start page. Pixels
//! are premultiplied ARGB (pixman's a8r8g8b8) at output resolution, so they map 1:1. Shapes are antialiased by
//! coverage from a distance function per pixel (WY-5: rounded rectangles, rings, dashed arcs, lines, convex
//! polygons); cheap enough for images that are drawn once and cached. Text comes from `text` (Exo 2 by fontdue).

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::Transform;

use crate::layout::Rect;
use crate::text::Font;
use crate::theme::Rgba;

pub fn pack(c: Rgba) -> u32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    b(c[3]) << 24 | b(c[0]) << 16 | b(c[1]) << 8 | b(c[2])
}

/// A rectangle in physical pixels with fractional edges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Box2 {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Box2 {
    pub fn of(r: Rect) -> Box2 {
        Box2 { x: r.x as f32, y: r.y as f32, w: r.w as f32, h: r.h as f32 }
    }
    pub fn inset(self, d: f32) -> Box2 {
        Box2 { x: self.x + d, y: self.y + d, w: (self.w - 2.0 * d).max(0.0), h: (self.h - 2.0 * d).max(0.0) }
    }
}

/// Signed distance from (px, py) to a rounded box (negative inside).
fn rbox_dist(b: Box2, r: f32, px: f32, py: f32) -> f32 {
    let r = r.min(b.w / 2.0).min(b.h / 2.0).max(0.0);
    let (cx, cy) = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    let qx = (px - cx).abs() - (b.w / 2.0 - r);
    let qy = (py - cy).abs() - (b.h / 2.0 - r);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

fn cov(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

pub struct Canvas {
    pub w: i32,
    pub h: i32,
    px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Self {
        Self { w: w.max(0), h: h.max(0), px: vec![0; (w.max(0) * h.max(0)) as usize] }
    }

    fn put(&mut self, x: i32, y: i32, c: u32) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            let i = (y * self.w + x) as usize;
            let a = c >> 24;
            self.px[i] = if a == 255 {
                c
            } else {
                // Premultiplied "over".
                let d = self.px[i];
                let ch = |s: u32| ((c >> s) & 255) + ((d >> s) & 255) * (255 - a) / 255;
                ch(24) << 24 | ch(16) << 16 | ch(8) << 8 | ch(0)
            };
        }
    }

    /// `c` with its alpha scaled by coverage `cov` (0..=255).
    pub(crate) fn put_cov(&mut self, x: i32, y: i32, c: u32, cov: u8) {
        if cov == 0 {
            return;
        }
        if cov == 255 {
            return self.put(x, y, c);
        }
        let k = |s: u32| ((c >> s) & 255) * cov as u32 / 255;
        self.put(x, y, k(24) << 24 | k(16) << 16 | k(8) << 8 | k(0));
    }

    /// Calls `f(px, py)` -> coverage 0..=1 for every pixel of the bounds and blends `c` by it.
    fn shade(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: Rgba, f: impl Fn(f32, f32) -> f32) {
        let c = pack(c);
        let (ax, ay) = ((x0.floor() as i32).max(0), (y0.floor() as i32).max(0));
        let (bx, by) = ((x1.ceil() as i32).min(self.w), (y1.ceil() as i32).min(self.h));
        for y in ay..by {
            for x in ax..bx {
                let k = f(x as f32 + 0.5, y as f32 + 0.5);
                self.put_cov(x, y, c, (k * 255.0).round() as u8);
            }
        }
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgba) {
        let c = pack(c);
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                self.put(xx, yy, c);
            }
        }
    }

    pub fn clear(&mut self, c: Rgba) {
        self.px.fill(pack(c));
    }

    /// A rounded rectangle, filled.
    pub fn rrect(&mut self, b: Box2, r: f32, c: Rgba) {
        self.shade(b.x - 1.0, b.y - 1.0, b.x + b.w + 1.0, b.y + b.h + 1.0, c, |x, y| cov(rbox_dist(b, r, x, y)));
    }

    /// `c` everywhere outside the rounded rectangle (to round off what lies below this image's corner).
    pub fn rrect_outside(&mut self, b: Box2, r: f32, c: Rgba) {
        let (w, h) = (self.w as f32, self.h as f32);
        self.shade(0.0, 0.0, w, h, c, |x, y| 1.0 - cov(rbox_dist(b, r, x, y)));
    }

    /// A rounded rectangle's border, `width` wide inside its edge (as a CSS border).
    pub fn rrect_line(&mut self, b: Box2, r: f32, width: f32, c: Rgba) {
        let inner = b.inset(width);
        let ri = (r - width).max(0.0);
        self.shade(b.x - 1.0, b.y - 1.0, b.x + b.w + 1.0, b.y + b.h + 1.0, c, |x, y| {
            (cov(rbox_dist(b, r, x, y)) - cov(rbox_dist(inner, ri, x, y))).max(0.0)
        });
    }

    /// The cheap glow (theme.effects cheap): `n` lines around `b`, each one pixel further out and fainter.
    pub fn glow(&mut self, b: Box2, r: f32, n: i32, c: Rgba) {
        for i in 1..=n {
            let k = 1.0 - (i - 1) as f32 / n as f32;
            let d = i as f32;
            let o = Box2 { x: b.x - d, y: b.y - d, w: b.w + 2.0 * d, h: b.h + 2.0 * d };
            self.rrect_line(o, r + d, 1.0, crate::theme::alpha(c, k));
        }
    }

    pub fn disc(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        self.shade(cx - r - 1.0, cy - r - 1.0, cx + r + 1.0, cy + r + 1.0, c, |x, y| {
            (r + 0.5 - (x - cx).hypot(y - cy)).clamp(0.0, 1.0)
        });
    }

    /// A ring of radius `r` (centre of its line) and `width`.
    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Rgba) {
        self.arc(cx, cy, r, width, None, c);
    }

    /// A ring, optionally dashed: `dash` = (on, off, start) in pixels along the circle, from the top clockwise.
    pub fn arc(&mut self, cx: f32, cy: f32, r: f32, width: f32, dash: Option<(f32, f32, f32)>, c: Rgba) {
        let e = r + width;
        self.shade(cx - e - 1.0, cy - e - 1.0, cx + e + 1.0, cy + e + 1.0, c, |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let radial = (width / 2.0 + 0.5 - (dx.hypot(dy) - r).abs()).clamp(0.0, 1.0);
            let Some((on, off, start)) = dash else { return radial };
            if radial == 0.0 {
                return 0.0;
            }
            let along = dx.atan2(-dy).rem_euclid(std::f32::consts::TAU) * r - start;
            let p = along.rem_euclid(on + off);
            // Distance outside the dash along the circle, for a soft dash end.
            let out = if p < on { -(p.min(on - p)) } else { (p - on).min(on + off - p) };
            radial * (0.5 - out).clamp(0.0, 1.0)
        });
    }

    /// An antialiased line with round ends.
    pub fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32), width: f32, c: Rgba) {
        let h = width / 2.0;
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        self.shade(x0.min(x1) - h - 1.0, y0.min(y1) - h - 1.0, x0.max(x1) + h + 1.0, y0.max(y1) + h + 1.0, c, |x, y| {
            let t = (((x - x0) * dx + (y - y0) * dy) / len2).clamp(0.0, 1.0);
            let d = (x - x0 - t * dx).hypot(y - y0 - t * dy);
            (h + 0.5 - d).clamp(0.0, 1.0)
        });
    }

    /// A convex polygon, points clockwise on screen.
    pub fn poly(&mut self, pts: &[(f32, f32)], c: Rgba) {
        if pts.len() < 3 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in pts {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        self.shade(x0 - 1.0, y0 - 1.0, x1 + 1.0, y1 + 1.0, c, |x, y| {
            let mut d = f32::MIN;
            for i in 0..pts.len() {
                let (ax, ay) = pts[i];
                let (bx, by) = pts[(i + 1) % pts.len()];
                let (ex, ey) = (bx - ax, by - ay);
                let l = ex.hypot(ey).max(1e-6);
                // Outward normal of a clockwise edge (screen y down) is (ey, -ex).
                d = d.max(((x - ax) * ey - (y - ay) * ex) / l);
            }
            cov(d)
        });
    }

    /// Draws `s` with its line's top at (x, y); returns the width drawn.
    pub fn text(&mut self, font: &Font, x: i32, y: i32, s: &str, c: Rgba) -> i32 {
        font.draw(self, x, y, s, pack(c))
    }

    /// `s` cut with an ellipsis to fit the band's width, vertically centred in it (pixels of this canvas).
    pub fn text_fit(&mut self, font: &Font, band: Rect, s: &str, c: Rgba) -> i32 {
        let shown = font.fit(s, band.w.max(0));
        self.text(font, band.x, band.y + (band.h - font.height()) / 2, &shown, c)
    }

    /// `s` centred in the band (cut to fit).
    pub fn text_mid(&mut self, font: &Font, band: Rect, s: &str, c: Rgba) {
        let shown = font.fit(s, band.w.max(0));
        let w = font.width(&shown);
        self.text(font, band.x + (band.w - w) / 2, band.y + (band.h - font.height()) / 2, &shown, c);
    }

    /// `s` ending at the band's right edge, vertically centred.
    pub fn text_right(&mut self, font: &Font, band: Rect, s: &str, c: Rgba) {
        let shown = font.fit(s, band.w.max(0));
        let w = font.width(&shown);
        self.text(font, band.x + band.w - w, band.y + (band.h - font.height()) / 2, &shown, c);
    }

    #[cfg(test)]
    pub fn pixel(&self, x: i32, y: i32) -> u32 {
        self.px[(y * self.w + x) as usize]
    }

    pub fn into_buffer(self) -> MemoryRenderBuffer {
        let bytes: Vec<u8> = self.px.iter().flat_map(|p| p.to_le_bytes()).collect();
        MemoryRenderBuffer::from_slice(&bytes, Fourcc::Argb8888, (self.w, self.h), 1, Transform::Normal, None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    /// "Let it float" (shown on a tiled window): two overlapping squares (the mock-up's ⧉).
    Float,
    /// "Back into a tile" (shown on a floating window): two tiles side by side.
    Tile,
    Close,
    Back,
    Forward,
    Home,
    /// WY-5c: maximise a floating window to the whole screen, and back.
    Maximise,
    Restore,
    /// The top bar's button of the folded workspace list: a panel with its left bar.
    Workspaces,
    /// The list's fold button (a double chevron to the left).
    Fold,
    /// The same button on a revealed list: keep it (dock it again).
    Pin,
}

/// The glyph alone, over whatever `c` holds, in the square `b`.
pub fn glyph_into(c: &mut Canvas, glyph: Glyph, b: Box2, fg: Rgba) {
    let s = b.w.min(b.h);
    let (x0, y0) = (b.x + (b.w - s) / 2.0, b.y + (b.h - s) / 2.0);
    let p = |fx: f32, fy: f32| (x0 + fx * s, y0 + fy * s);
    let th = (s / 12.0).max(1.2);
    match glyph {
        Glyph::Close => {
            c.line(p(0.3, 0.3), p(0.7, 0.7), th, fg);
            c.line(p(0.7, 0.3), p(0.3, 0.7), th, fg);
        }
        Glyph::Float => {
            let q = |fx: f32, fy: f32, fw: f32| Box2 { x: p(fx, fy).0, y: p(fx, fy).1, w: fw * s, h: fw * s };
            c.rrect_line(q(0.24, 0.24, 0.36), s * 0.04, th * 0.8, fg);
            c.rrect_line(q(0.40, 0.40, 0.36), s * 0.04, th * 0.8, fg);
        }
        Glyph::Tile => {
            let q = |fx: f32| Box2 { x: p(fx, 0.3).0, y: p(fx, 0.3).1, w: 0.18 * s, h: 0.4 * s };
            c.rrect_line(q(0.28), s * 0.03, th * 0.8, fg);
            c.rrect_line(q(0.54), s * 0.03, th * 0.8, fg);
        }
        Glyph::Back => {
            c.line(p(0.56, 0.32), p(0.42, 0.5), th, fg);
            c.line(p(0.42, 0.5), p(0.56, 0.68), th, fg);
        }
        Glyph::Forward => {
            c.line(p(0.44, 0.32), p(0.58, 0.5), th, fg);
            c.line(p(0.58, 0.5), p(0.44, 0.68), th, fg);
        }
        Glyph::Home => {
            c.line(p(0.25, 0.5), p(0.5, 0.27), th, fg);
            c.line(p(0.5, 0.27), p(0.75, 0.5), th, fg);
            c.line(p(0.33, 0.45), p(0.33, 0.73), th, fg);
            c.line(p(0.67, 0.45), p(0.67, 0.73), th, fg);
            c.line(p(0.33, 0.73), p(0.67, 0.73), th, fg);
        }
        Glyph::Maximise | Glyph::Restore => {
            let (a, z) = if glyph == Glyph::Maximise { (0.26, 0.74) } else { (0.36, 0.64) };
            let q = Box2 { x: p(a, a).0, y: p(a, a).1, w: (z - a) * s, h: (z - a) * s };
            c.rrect_line(q, s * 0.03, th * 0.8, fg);
            c.line(p(a, a + 0.04), p(z, a + 0.04), th * 0.9, fg);
            if glyph == Glyph::Restore {
                for (x0, y0, dx, dy) in [(0.22, 0.22, 1.0, 1.0), (0.78, 0.22, -1.0, 1.0), (0.22, 0.78, 1.0, -1.0), (0.78, 0.78, -1.0, -1.0)] {
                    c.line(p(x0, y0), p(x0 + dx * 0.09, y0 + dy * 0.09), th * 0.8, fg);
                }
            }
        }
        Glyph::Workspaces => {
            let q = Box2 { x: p(0.2, 0.26).0, y: p(0.2, 0.26).1, w: 0.6 * s, h: 0.48 * s };
            c.rrect_line(q, s * 0.05, th * 0.8, fg);
            c.line(p(0.4, 0.27), p(0.4, 0.73), th * 0.8, fg);
            for y in [0.38, 0.5] {
                c.line(p(0.25, y), p(0.34, y), th * 0.7, fg);
            }
        }
        Glyph::Fold => {
            for x in [0.38, 0.56] {
                c.line(p(x + 0.1, 0.32), p(x - 0.04, 0.5), th, fg);
                c.line(p(x - 0.04, 0.5), p(x + 0.1, 0.68), th, fg);
            }
        }
        Glyph::Pin => {
            let (cx, cy) = p(0.5, 0.36);
            c.ring(cx, cy, 0.12 * s, th * 0.8, fg);
            c.line(p(0.34, 0.52), p(0.66, 0.52), th, fg);
            c.line(p(0.5, 0.52), p(0.5, 0.8), th, fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_are_antialiased_and_stay_inside_the_image() {
        let mut c = Canvas::new(40, 40);
        let white = [1.0, 1.0, 1.0, 1.0];
        c.rrect(Box2 { x: 4.0, y: 4.0, w: 32.0, h: 32.0 }, 8.0, white);
        assert_eq!(c.pixel(20, 20), 0xffff_ffff, "inside is solid");
        assert_eq!(c.pixel(4, 4), 0, "the rounded corner leaves the corner pixel");
        assert!(c.pixel(4, 20) >> 24 > 0, "the straight edge pixel is covered");
        let corner = c.pixel(6, 6) >> 24;
        assert!(corner > 0 && corner < 255, "a corner pixel is partly covered ({corner})");
        // Out of range shapes draw nothing and do not panic.
        c.ring(-100.0, -100.0, 10.0, 2.0, white);
        c.line((-5.0, -5.0), (80.0, 90.0), 3.0, white);
        c.poly(&[(0.0, 0.0), (100.0, 0.0), (0.0, 100.0)], white);
        c.arc(20.0, 20.0, 15.0, 3.0, Some((10.0, 5.0, 0.0)), white);
    }

    #[test]
    fn a_dashed_arc_has_gaps() {
        let mut c = Canvas::new(60, 60);
        c.arc(30.0, 30.0, 20.0, 3.0, Some((20.0, 20.0, 0.0)), [1.0; 4]);
        // The dash starts at the top and runs clockwise: 0.3 rad after the top is on, 1 rad before it is off.
        assert!(c.pixel(35, 10) >> 24 > 200);
        assert_eq!(c.pixel(13, 19) >> 24, 0);
    }

    #[test]
    fn a_clockwise_triangle_is_filled() {
        let mut c = Canvas::new(30, 30);
        c.poly(&[(5.0, 5.0), (25.0, 15.0), (5.0, 25.0)], [1.0; 4]);
        assert_eq!(c.pixel(10, 15) >> 24, 255);
        assert_eq!(c.pixel(24, 5) >> 24, 0);
    }
}
