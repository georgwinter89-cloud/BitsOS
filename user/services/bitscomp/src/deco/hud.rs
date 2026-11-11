//! The HUD kit at the cheap effect level (DS-2, plan/notes/wy-design-v2-scout.md §3): what the design draws with
//! blur and live layers, drawn once into bitscomp's own cached images. Glass is a translucent fill over the baked
//! wallpaper (behind a panel lies the still wallpaper, so a live blur would change nothing visible) and a
//! precomputed solid blend where a panel lies over windows; the HUD corner brackets; box shadows from a cached
//! nine-patch coverage sprite (drawn only outside the box, as CSS does); the soft glow is the same sprite in the
//! accent. Sizes are board px (`Theme::b`, 4/3 on 2560x1440).

use std::cell::RefCell;
use std::collections::HashMap;

use crate::canvas::{Box2, Canvas, pack};
use crate::theme::{Rgba, Shadow, Theme, alpha};

/// Which glass: a bar (the design's header: sunken surface at 72 % dark, raised at 70 % light) or a card (the
/// stage: raised surface at 55 % dark, 60 % light).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glass {
    Bar,
    Card,
}

fn glass_parts(t: &Theme, g: Glass) -> (Rgba, f32) {
    match (g, t.dark) {
        (Glass::Bar, true) => (t.panel2, 0.72),
        (Glass::Bar, false) => (t.panel, 0.70),
        (Glass::Card, true) => (t.panel, 0.55),
        (Glass::Card, false) => (t.panel, 0.60),
    }
}

/// The translucent fill over the wallpaper (premultiplied).
pub fn glass(t: &Theme, g: Glass) -> Rgba {
    let (c, k) = glass_parts(t, g);
    alpha(c, k)
}

/// The same glass as an opaque colour, for a panel over windows (a revealed bar): the blend over the ground.
pub fn glass_solid(t: &Theme, g: Glass) -> Rgba {
    let (c, k) = glass_parts(t, g);
    let mix = |i: usize| c[i] * k + t.bg[i] * (1.0 - k);
    [mix(0), mix(1), mix(2), 1.0]
}

/// The glass for a panel: translucent over the wallpaper, solid over windows.
pub fn glass_for(t: &Theme, g: Glass, over_windows: bool) -> Rgba {
    // WY-19a: the gpu level blurs what lies under it, so the panel stays glass over windows too.
    if over_windows && !t.effects_gpu { glass_solid(t, g) } else { glass(t, g) }
}

/// How far the brackets reach outside their panel (board px): the margin an image needs around a bracketed panel.
pub const BRACKET_OUT: f32 = 7.0;

/// The brackets' colour: the accent at 85 % (dark) / 70 % (light, Hell.dc.html), raised to 85 % by the light pass
/// (plan/notes/ds-DS-2.md: 70 % gave 2.9:1 on the sage grey, under the 3:1 of a graphic; 85 % gives 3.8:1).
pub fn bracket_colour(t: &Theme) -> Rgba {
    alpha(t.accent, 0.85)
}

/// The HUD corner brackets of a panel `b` (pixels of `c`): a 20 px L with a 12 px rounded corner, 2 px wide, 7 px
/// outside the top left and the bottom right corner (`.hud::before/::after`).
pub fn brackets(c: &mut Canvas, b: Box2, t: &Theme) {
    let k = t.board as f32;
    let (len, out, w, rad) = (20.0 * k, BRACKET_OUT * k, 2.0 * k, 12.0 * k);
    let col = bracket_colour(t);
    // The line's centre runs half its width inside the 20 px box, as a CSS border.
    let (rc, h) = (rad - w / 2.0, w / 2.0);
    let quarter = std::f32::consts::FRAC_PI_2 * rc;
    for (ox, oy, sx, sy, from) in [(b.x - out, b.y - out, 1.0, 1.0, 3.0), (b.x + b.w + out, b.y + b.h + out, -1.0, -1.0, 1.0)] {
        // The corner's centre of curvature, then the two straight arms to the L's ends.
        let (cx, cy) = (ox + sx * rad, oy + sy * rad);
        c.arc(cx, cy, rc, w, Some((quarter, 3.0 * quarter, from * quarter)), col);
        let (ex, ey) = (ox + sx * (len - h), oy + sy * (len - h));
        c.line((cx, oy + sy * h), (ex, oy + sy * h), w, col);
        c.line((ox + sx * h, cy), (ox + sx * h, ey), w, col);
    }
}

/// Signed distance to a rounded box (negative inside), as `canvas` measures it.
fn rbox_dist(b: Box2, r: f32, px: f32, py: f32) -> f32 {
    let r = r.min(b.w / 2.0).min(b.h / 2.0).max(0.0);
    let (cx, cy) = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    let qx = (px - cx).abs() - (b.w / 2.0 - r);
    let qy = (py - cy).abs() - (b.h / 2.0 - r);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

/// Coverage (0..=1) of the rounded box at a pixel centre.
pub fn rbox_cov(b: Box2, r: f32, px: f32, py: f32) -> f32 {
    (0.5 - rbox_dist(b, r, px, py)).clamp(0.0, 1.0)
}

/// The nine-patch sprite: a blurred rounded rectangle's coverage, `w` x `h` px.
struct Sprite {
    w: usize,
    h: usize,
    cov: Vec<f32>,
}

/// One box blur pass of radius `r` along the rows (or the columns) of a `w` x `h` field.
fn box_pass(v: &mut [f32], (w, h): (usize, usize), r: usize, along_rows: bool) {
    let (n, lines) = if along_rows { (w, h) } else { (h, w) };
    let mut line = vec![0.0f32; n];
    for o in 0..lines {
        let at = |i: usize| if along_rows { o * w + i } else { i * w + o };
        let mut sum = 0.0;
        for i in 0..=r.min(n - 1) {
            sum += v[at(i)];
        }
        for (i, slot) in line.iter_mut().enumerate() {
            *slot = sum / (2 * r + 1) as f32;
            if i + r + 1 < n {
                sum += v[at(i + r + 1)];
            }
            if i >= r {
                sum -= v[at(i - r)];
            }
        }
        for (i, x) in line.iter().enumerate() {
            v[at(i)] = *x;
        }
    }
}

/// Gaussian blur with standard deviation `sigma` (three box passes per axis).
fn blur(v: &mut [f32], size: (usize, usize), sigma: f32) {
    if sigma < 0.5 {
        return;
    }
    // Three boxes of width 2r+1 have the variance 3 * ((2r+1)^2 - 1) / 12.
    let r = (((4.0 * sigma * sigma + 1.0).sqrt() - 1.0) / 2.0).round().max(1.0) as usize;
    for along_rows in [true, false] {
        for _ in 0..3 {
            box_pass(v, size, r, along_rows);
        }
    }
}

/// The blurred rounded rectangle of `w` x `h` px, radius `r`, blur `sigma`, with `pad` (3 sigma) room around it.
fn blurred(w: usize, h: usize, r: f32, sigma: f32, pad: usize) -> Sprite {
    let (sw, sh) = (w + 2 * pad, h + 2 * pad);
    let b = Box2 { x: pad as f32, y: pad as f32, w: w as f32, h: h as f32 };
    let mut cov: Vec<f32> = (0..sw * sh).map(|i| rbox_cov(b, r, (i % sw) as f32 + 0.5, (i / sw) as f32 + 0.5)).collect();
    blur(&mut cov, (sw, sh), sigma);
    Sprite { w: sw, h: sh, cov }
}

thread_local! {
    /// Sprites by (radius, sigma in 1/4 px, capped width, capped height): a few per theme (one per panel size).
    static SPRITES: RefCell<HashMap<(u32, u32, usize, usize), std::rc::Rc<Sprite>>> = RefCell::new(HashMap::new());
}

/// The sprite for a `w` x `h` box: along an axis longer than `2 * (r + 3 sigma) + 1` only that much is drawn (its
/// middle line is then the profile of a straight edge and is stretched), so the sprite is at most about
/// `2 (r + 6 sigma)` square whatever the panel's size, and exact.
fn sprite(w: usize, h: usize, r: f32, sigma: f32, pad: usize) -> std::rc::Rc<Sprite> {
    let cap = 2 * (r.ceil() as usize + pad) + 1;
    let key = ((r * 4.0).round() as u32, (sigma * 4.0).round() as u32, w.min(cap), h.min(cap));
    SPRITES.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 64 {
            // Old panel sizes (another scale or output): drawn again when they come back.
            m.clear();
        }
        m.entry(key).or_insert_with(|| std::rc::Rc::new(blurred(key.2, key.3, r, sigma, pad))).clone()
    })
}

/// Paints `col` scaled by `cov(x, y)` over every pixel of `area` that lies outside the box `skip` (CSS paints a
/// box shadow only outside its box; the glass over it stays clear).
fn paint(c: &mut Canvas, area: (i32, i32, i32, i32), skip: (Box2, f32), col: Rgba, cov: impl Fn(i32, i32) -> f32) {
    let p = pack(col);
    let (x0, y0, x1, y1) = area;
    let (b, r) = skip;
    // Inside the box's straight part nothing is painted; well outside it the box takes nothing away.
    let inner = b.inset(r.min(b.w / 2.0).min(b.h / 2.0) + 1.0);
    let has = |q: Box2, x: f32, y: f32| x >= q.x && x < q.x + q.w && y >= q.y && y < q.y + q.h;
    let outer = b.inset(-1.0);
    for y in y0.max(0)..y1.min(c.h) {
        for x in x0.max(0)..x1.min(c.w) {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            if has(inner, px, py) {
                continue;
            }
            let free = if has(outer, px, py) { 1.0 - rbox_cov(b, r, px, py) } else { 1.0 };
            let k = cov(x - x0, y - y0) * free;
            let k = (k * 255.0).round().clamp(0.0, 255.0) as u8;
            c.put_cov(x, y, p, k);
        }
    }
}

/// One shadow layer of box `b` (radius `r`, pixels of `c`): (dy, blur, spread) in board px, CSS meaning (blur =
/// 2 sigma). A layer without blur is a ring of `spread` width (the design's 1 px rings).
pub fn shadow_layer(c: &mut Canvas, b: Box2, r: f32, (dy, blur_px, spread): (f32, f32, f32), col: Rgba, t: &Theme) {
    let k = t.board as f32;
    let (dy, sigma, spread) = (dy * k, blur_px * k / 2.0, spread * k);
    if sigma < 0.5 {
        if spread > 0.0 {
            let o = Box2 { x: b.x - spread, y: b.y - spread + dy, w: b.w + 2.0 * spread, h: b.h + 2.0 * spread };
            c.rrect_line(o, r + spread, spread, col);
        }
        return;
    }
    let sb = Box2 { x: b.x - spread, y: b.y - spread + dy, w: (b.w + 2.0 * spread).max(1.0), h: (b.h + 2.0 * spread).max(1.0) };
    let sr = (r + spread).max(0.0);
    let pad = (3.0 * sigma).ceil() as usize;
    let (w, h) = (sb.w.round() as usize, sb.h.round() as usize);
    let s = sprite(w, h, sr, sigma, pad);
    let (bw, bh) = ((w + 2 * pad) as i32, (h + 2 * pad) as i32);
    let (ax, ay) = ((sb.x.round() as i32) - pad as i32, (sb.y.round() as i32) - pad as i32);
    // Each axis: the sprite's first half, its middle line stretched, its second half.
    let map = |d: i32, span: i32, n: usize| {
        let half = (n / 2) as i32;
        if d < half { d } else if d >= span - half { n as i32 - (span - d) } else { half }
    };
    paint(c, (ax, ay, ax + bw, ay + bh), (b, r), col, |x, y| {
        let (sx, sy) = (map(x, bw, s.w), map(y, bh, s.h));
        s.cov.get(sy as usize * s.w + sx as usize).copied().unwrap_or(0.0)
    });
}

/// A design shadow token (shadow-raised, shadow-lifted): both layers, the outer first.
pub fn shadow(c: &mut Canvas, b: Box2, r: f32, s: &Shadow, t: &Theme) {
    for l in s.0.iter().rev() {
        shadow_layer(c, b, r, (l.dy, l.blur, l.spread), l.color, t);
    }
}

/// A glass panel's shadow in its image: only at the cheap level, the gpu level draws it as `fx::Kind::Shadow`.
pub fn panel_shadow(c: &mut Canvas, b: Box2, r: f32, s: &Shadow, t: &Theme) {
    if !t.effects_gpu {
        shadow(c, b, r, s, t);
    }
}

/// A lit panel's glow in its image, likewise.
pub fn panel_glow(c: &mut Canvas, b: Box2, r: f32, t: &Theme) {
    if !t.effects_gpu {
        glow_soft(c, b, r, t);
    }
}

/// The design's soft glow (`glowSoft`: 0 0 40px accent 22 % at theme.glow 60) around a lit panel.
pub fn glow_soft(c: &mut Canvas, b: Box2, r: f32, t: &Theme) {
    // t.glow carries theme.glow's strength (18 % dark at the default); 22 % is the design's value at it.
    let a = t.glow[3] / if t.dark { 0.18 } else { 0.14 } * 0.22;
    shadow_layer(c, b, r, (0.0, 40.0, 0.0), alpha(t.accent, a.min(0.5)), t);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(p: u32) -> (u32, u32, u32) {
        ((p >> 16) & 255, (p >> 8) & 255, p & 255)
    }

    #[test]
    fn glass_is_translucent_and_its_solid_twin_is_the_blend_over_the_ground() {
        for mode in ["dark", "light"] {
            let mut v = crate::settings::Values::default();
            v.set("theme.mode", mode);
            let t = Theme::build(&v);
            for g in [Glass::Bar, Glass::Card] {
                let a = glass(&t, g)[3];
                assert!(a > 0.5 && a < 0.8, "{mode} {g:?}: alpha {a}");
                let mut c = Canvas::new(1, 1);
                c.fill(0, 0, 1, 1, t.bg);
                c.fill(0, 0, 1, 1, glass(&t, g));
                let mut d = Canvas::new(1, 1);
                d.fill(0, 0, 1, 1, glass_solid(&t, g));
                let (p, q) = (rgb(c.pixel(0, 0)), rgb(d.pixel(0, 0)));
                let near = |a: u32, b: u32| a.abs_diff(b) <= 2;
                assert!(near(p.0, q.0) && near(p.1, q.1) && near(p.2, q.2), "{mode} {g:?}: {p:?} vs {q:?}");
                assert_eq!(d.pixel(0, 0) >> 24, 255);
            }
        }
    }

    #[test]
    fn brackets_sit_outside_two_corners_only() {
        let t = crate::theme::test_theme();
        let k = t.board as f32;
        let mut c = Canvas::new(400, 300);
        let b = Box2 { x: 50.0, y: 50.0, w: 300.0, h: 200.0 };
        brackets(&mut c, b, t);
        let ink = |x: f32, y: f32| c.pixel(x as i32, y as i32) >> 24;
        // The top arm of the top-left L: 7 px above the box, from its corner to 20 px.
        assert!(ink(b.x - 7.0 * k + 16.0 * k, b.y - 7.0 * k + 0.5 * k) > 150, "top-left arm");
        assert!(ink(b.x - 7.0 * k + 0.5 * k, b.y - 7.0 * k + 16.0 * k) > 150, "left arm");
        assert!(ink(b.x + b.w + 7.0 * k - 0.5 * k, b.y + b.h + 7.0 * k - 16.0 * k) > 150, "bottom-right arm");
        // Nothing inside the box, nothing at the other two corners, nothing beyond 20 px.
        assert_eq!(ink(b.x + 20.0, b.y + 20.0), 0);
        assert_eq!(ink(b.x + b.w + 5.0, b.y - 5.0), 0, "top-right free");
        assert_eq!(ink(b.x - 5.0, b.y + b.h + 5.0), 0, "bottom-left free");
        assert_eq!(ink(b.x - 7.0 * k + 23.0 * k, b.y - 7.0 * k + 0.5 * k), 0, "the arm ends at 20 px");
    }

    #[test]
    fn the_nine_patch_shadow_matches_the_exact_blur_and_leaves_the_box_clear() {
        let t = crate::theme::test_theme();
        let k = t.board as f32;
        let col = [0.0, 0.0, 0.0, 0.8];
        // A wide box: the stretched sprite against the blur of the whole box.
        let b = Box2 { x: 200.0, y: 200.0, w: 1400.0, h: 600.0 };
        let mut nine = Canvas::new(1800, 1050);
        shadow_layer(&mut nine, b, 37.0, (24.0, 48.0, -12.0), col, t);
        let (sigma, spread, dy) = (24.0 * k, -12.0 * k, 24.0 * k);
        let pad = (3.0 * sigma).ceil() as usize;
        let sb = Box2 { x: b.x - spread, y: b.y - spread + dy, w: b.w + 2.0 * spread, h: b.h + 2.0 * spread };
        let full = blurred(sb.w.round() as usize, sb.h.round() as usize, 37.0 + spread, sigma, pad);
        let small = sprite(sb.w.round() as usize, sb.h.round() as usize, 37.0 + spread, sigma, pad);
        assert!(small.w < full.w, "the sprite is capped ({} < {})", small.w, full.w);
        let (ax, ay) = (sb.x.round() as i32 - pad as i32, sb.y.round() as i32 - pad as i32);
        let mut worst = 0u32;
        for (x, y) in [(900, 880), (900, 905), (150, 500), (1650, 500), (190, 900), (1600, 880), (400, 850), (130, 160), (1000, 160)] {
            let want = (full.cov[(y - ay) as usize * full.w + (x - ax) as usize] * 0.8 * 255.0).round() as u32;
            let got = nine.pixel(x, y) >> 24;
            worst = worst.max(got.abs_diff(want));
        }
        assert!(worst <= 3, "nine-patch differs from the exact blur by {worst}/255");
        // Darkest just below the box, nothing inside it, fading out with distance.
        assert_eq!(nine.pixel(900, 500) >> 24, 0, "inside the box stays clear");
        let below = |d: i32| nine.pixel(900, 800 + d) >> 24;
        assert!(below(10) > below(60) && below(60) > below(120) && below(10) > 60, "{} {} {}", below(10), below(60), below(120));
        // A small box still shades below it.
        let mut little = Canvas::new(300, 300);
        shadow_layer(&mut little, Box2 { x: 100.0, y: 100.0, w: 80.0, h: 40.0 }, 10.0, (12.0, 32.0, -12.0), col, t);
        assert!(little.pixel(140, 160) >> 24 > 0);
    }

    #[test]
    fn the_soft_glow_follows_theme_glow() {
        let strong = |g: &str| {
            let mut v = crate::settings::Values::default();
            v.set("theme.glow", g);
            let t = Theme::build(&v);
            let mut c = Canvas::new(300, 200);
            glow_soft(&mut c, Box2 { x: 100.0, y: 60.0, w: 100.0, h: 80.0 }, 12.0, &t);
            c.pixel(92, 100) >> 24
        };
        assert!(strong("100") > strong("60") && strong("60") > strong("20") && strong("0") == 0);
    }
}
