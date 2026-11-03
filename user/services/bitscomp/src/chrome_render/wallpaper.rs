//! The ground under everything (DS-2, Main.dc.html / Hell.dc.html): the dotted grid every 24 board px, the big
//! radial glow behind the stage and a second one low right, the vignette (dark), and the scanlines (dark,
//! theme.scanlines), all baked into one opaque image per theme and output size and drawn as the bottom element.
//! It is never redrawn while the theme and the output stay, so a still desktop costs nothing; the glows are
//! dithered so their 4-13 % alpha shows no bands. Only the GPU level (a later wave) would move anything here.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use crate::win::Window;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::layout::Placement;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::theme::{Rgba, Theme};

/// A radial gradient on an ellipse: centre and radii (px), colour (straight, 0..=1), alpha stops (at, alpha).
#[derive(Clone, Debug, PartialEq)]
struct Glow {
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    rgb: [f32; 3],
    stops: Vec<(f32, f32)>,
}

impl Glow {
    fn alpha(&self, x: f32, y: f32) -> f32 {
        let d = ((x - self.cx) / self.rx).hypot((y - self.cy) / self.ry);
        let mut prev = self.stops[0];
        if d <= prev.0 {
            return prev.1;
        }
        for &s in &self.stops[1..] {
            if d <= s.0 {
                return prev.1 + (s.1 - prev.1) * (d - prev.0) / (s.0 - prev.0);
            }
            prev = s;
        }
        prev.1
    }
}

/// Everything the wallpaper's pixels follow from: a theme at an output size (physical px).
#[derive(Clone, Debug, PartialEq)]
pub struct Paper {
    pub w: i32,
    pub h: i32,
    base: [f32; 3],
    dot: [f32; 3],
    period: f32,
    /// The dot's full radius and where it has faded out (radial-gradient(dot 1px, transparent 1.3px)).
    dot_in: f32,
    dot_out: f32,
    glows: Vec<Glow>,
    /// (period px, colour, alpha) of the scanlines.
    scan: Option<(i32, [f32; 3], f32)>,
    /// DS-3: the Bühne's stage glass over the ground (box px, radius px, colour, alpha), for what samples `pixel`
    /// (the tile corners); the stage itself is its own image above the wallpaper (`scene::stage_element`).
    stage: Option<(crate::canvas::Box2, f32, [f32; 3], f32)>,
}

fn rgb(c: Rgba) -> [f32; 3] {
    // Theme colours are premultiplied; the palette's are opaque, so this is the straight colour.
    let a = c[3].max(1e-6);
    [c[0] / a, c[1] / a, c[2] / a]
}

/// Ordered dither, 4x4 Bayer, in 1/16 steps.
const BAYER: [f32; 16] = [0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0];

impl Paper {
    pub fn new(t: &Theme, w: i32, h: i32) -> Paper {
        let k = t.board as f32;
        // The boards are 1920x1080: the glows keep their place relative to the output.
        let (fx, fy) = (w as f32 / 1920.0, h as f32 / 1080.0);
        let glow = |l: f32, top: f32, gw: f32, gh: f32, c: Rgba, stops: Vec<(f32, f32)>| Glow {
            cx: (l + gw / 2.0) * fx,
            cy: (top + gh / 2.0) * fy,
            rx: gw / 2.0 * fx,
            ry: gh / 2.0 * fy,
            rgb: rgb(c),
            stops,
        };
        let glows = if t.dark {
            vec![
                glow(260.0, -180.0, 1400.0, 1100.0, t.accent, vec![(0.0, 0.13), (0.55, 0.04), (1.0, 0.0)]),
                glow(1380.0, 520.0, 700.0, 700.0, t.kontext[2], vec![(0.0, 0.08), (1.0, 0.0)]),
                // The vignette: an ellipse at the centre to the farthest corner, dark from 55 %.
                Glow {
                    cx: w as f32 / 2.0,
                    cy: h as f32 / 2.0,
                    rx: w as f32 / 2.0 * std::f32::consts::SQRT_2,
                    ry: h as f32 / 2.0 * std::f32::consts::SQRT_2,
                    rgb: [4.0 / 255.0, 8.0 / 255.0, 6.0 / 255.0],
                    stops: vec![(0.55, 0.0), (1.0, 0.65)],
                },
            ]
        } else {
            vec![
                glow(260.0, -200.0, 1400.0, 1100.0, t.panel, vec![(0.0, 0.85), (0.55, 0.3), (1.0, 0.0)]),
                glow(900.0, -120.0, 600.0, 600.0, t.accent, vec![(0.0, 0.16), (1.0, 0.0)]),
            ]
        };
        let ink = rgb(t.text);
        Paper {
            w: w.max(0),
            h: h.max(0),
            base: rgb(t.bg),
            dot: rgb(t.line),
            period: (24.0 * k).max(2.0),
            dot_in: k,
            dot_out: 1.3 * k,
            glows,
            // 1 px every 3 board px at 2.8 %: on the physical grid one line every round(3 k) px, the same mean.
            scan: (t.dark && t.scanlines).then(|| {
                let p = (3.0 * k).round().max(2.0) as i32;
                (p, ink, 0.028 * p as f32 / 3.0)
            }),
            stage: None,
        }
    }

    /// The ground with the glows (and vignette) at the pixel centre (x, y): smooth, no dots or scanlines.
    fn ground(&self, px: f32, py: f32) -> [f32; 3] {
        let mut c = self.base;
        for g in &self.glows {
            let a = g.alpha(px, py);
            if a > 0.0 {
                over(&mut c, g.rgb, a);
            }
        }
        c
    }

    /// How much of the dot covers the pixel centre (x, y) (0..=1).
    fn dot_cov(&self, px: f32, py: f32) -> f32 {
        let half = self.period / 2.0;
        let d = (px.rem_euclid(self.period) - half).hypot(py.rem_euclid(self.period) - half);
        ((self.dot_out - d) / (self.dot_out - self.dot_in)).clamp(0.0, 1.0)
    }

    /// The straight colour (0..=1) at (x, y) from its ground: the dot (under the glows, as on the boards: the grid
    /// is the board's background, the glows lie over it) and the scanline.
    fn finish(&self, x: i32, y: i32, ground: [f32; 3], dot: f32) -> [f32; 3] {
        let mut c = ground;
        if dot > 0.0 {
            // The glows over the dot: the dot shows through them as over the plain ground.
            for i in 0..3 {
                c[i] += (self.dot[i] - self.base[i]) * dot * self.through(x, y);
            }
        }
        if let Some((p, ink, a)) = self.scan
            && y.rem_euclid(p) == 0
        {
            over(&mut c, ink, a);
        }
        c
    }

    /// The share of the ground that shows through the glows at (x, y): the product of (1 - alpha).
    fn through(&self, x: i32, y: i32) -> f32 {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        self.glows.iter().map(|g| 1.0 - g.alpha(px, py)).product()
    }

    fn quantise(c: [f32; 3], x: i32, y: i32) -> u32 {
        let d = (BAYER[((y & 3) * 4 + (x & 3)) as usize] + 0.5) / 16.0;
        // `as` truncates (= floor for the non-negative values here) and saturates; no libm call per channel.
        let q = |v: f32| ((v * 255.0 + d).max(0.0) as u32).min(255);
        0xff00_0000 | q(c[0]) << 16 | q(c[1]) << 8 | q(c[2])
    }

    /// The opaque pixel (a8r8g8b8) at (x, y), dithered: exact (the tile corners sample it).
    pub fn pixel(&self, x: i32, y: i32) -> u32 {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let mut c = self.finish(x, y, self.ground(px, py), self.dot_cov(px, py));
        if let Some((b, r, col, a)) = self.stage {
            over(&mut c, col, a * crate::deco::hud::rbox_cov(b, r, px, py));
        }
        Self::quantise(c, x, y)
    }

    /// DS-3: `pixel` with the stage's glass `glass` (premultiplied) over the ground inside `b` (px).
    pub fn with_stage(mut self, b: crate::canvas::Box2, r: f32, glass: Rgba) -> Paper {
        let a = glass[3];
        let col = if a > 0.0 { [glass[0] / a, glass[1] / a, glass[2] / a] } else { [0.0; 3] };
        self.stage = Some((b, r, col, a));
        self
    }

    /// The whole picture, as little-endian a8r8g8b8 bytes. The glows are smooth over hundreds of pixels, so a row
    /// evaluates them every `STEP` px and interpolates between (within one level of `pixel`, tested); dots are
    /// looked at only on the rows and columns they reach. About 8x cheaper than `pixel` for every pixel.
    pub fn bytes(&self) -> Vec<u8> {
        const STEP: i32 = 8;
        let (w, h) = (self.w, self.h);
        let mut out = vec![0u8; (w * h * 4) as usize];
        let knots = (w + STEP - 1) / STEP + 1;
        let mut row: Vec<[f32; 3]> = Vec::with_capacity(knots as usize);
        let reach = self.dot_out.ceil() + 1.0;
        let half = self.period / 2.0;
        // The columns a dot reaches, once (no remainder per pixel).
        let dot_col: Vec<bool> = (0..w).map(|x| ((x as f32 + 0.5).rem_euclid(self.period) - half).abs() < reach).collect();
        for y in 0..h {
            let py = y as f32 + 0.5;
            row.clear();
            row.extend((0..knots).map(|i| self.ground((i * STEP) as f32 + 0.5, py)));
            let dot_row = (py.rem_euclid(self.period) - half).abs() < reach;
            let line = &mut out[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
            let scan = self.scan.filter(|(p, _, _)| y.rem_euclid(*p) == 0);
            for (x, px) in line.chunks_exact_mut(4).enumerate() {
                let (i, f) = (x / STEP as usize, (x % STEP as usize) as f32 / STEP as f32);
                let (a, b) = (row[i], row[i + 1]);
                let mut g = [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f];
                if dot_row && dot_col[x] {
                    g = self.finish(x as i32, y, g, self.dot_cov(x as f32 + 0.5, py));
                } else if let Some((_, ink, a)) = scan {
                    over(&mut g, ink, a);
                }
                px.copy_from_slice(&Self::quantise(g, x as i32, y).to_le_bytes());
            }
        }
        out
    }
}

fn over(c: &mut [f32; 3], s: [f32; 3], a: f32) {
    for i in 0..3 {
        c[i] = c[i] * (1.0 - a) + s[i] * a;
    }
}

/// The cache key: the theme's generation and the output's physical size.
pub type Key = (u64, i32, i32);

/// The output's size in physical px.
pub fn output_px(app: &App) -> (i32, i32) {
    let r = crate::header::px_rect(app.area_full(), app.theme.scale);
    (r.w, r.h)
}

/// The wallpaper element: the bottom of the frame (render.rs pushes it after every layer, CR DS-2-1). Nothing under
/// a fullscreen window (it covers the output; the wallpaper would only be blended for nothing).
// Called by render.rs's build_elements after every layer.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R, placements: &[Placement<Window>]) -> Vec<CompElement<R>> {
    if placements.iter().any(|p| p.fullscreen && p.visible) {
        return Vec::new();
    }
    let (w, h) = output_px(app);
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let key: Key = (app.theme_gen, w, h);
    if app.images.wallpaper.as_ref().is_none_or(|(k, _)| *k != key) {
        let t0 = std::time::Instant::now();
        let paper = Paper::new(&app.theme, w, h);
        let opaque: Rectangle<i32, Buffer> = Rectangle::from_size(Size::from((w, h)));
        let buf = MemoryRenderBuffer::from_slice(&paper.bytes(), Fourcc::Argb8888, (w, h), 1, Transform::Normal, Some(vec![opaque]));
        // B6: one line per redraw (theme or output change), with its cost.
        eprintln!("bitscomp: wallpaper {w}x{h} {} drawn in {} ms", if app.theme.dark { "dark" } else { "light" }, t0.elapsed().as_millis());
        app.images.wallpaper = Some((key, buf));
    }
    // DS-3: the Bühne's stage lies on the wallpaper, under the tiles.
    let stage = super::scene::stage_element(app, renderer);
    let buf = &app.images.wallpaper.as_ref().unwrap().1;
    let full = app.area_full();
    let o = crate::header::px_rect(full, app.theme.scale);
    let src: Rectangle<f64, Logical> = Rectangle::from_size(Size::from((w as f64, h as f64)));
    let at: Point<f64, Physical> = Point::from((o.x as f64, o.y as f64));
    let size: Size<i32, Logical> = (full.w, full.h).into();
    let paper = MemoryRenderBufferRenderElement::from_buffer(renderer, at, buf, None, Some(src), Some(size), Kind::Unspecified)
        .ok()
        .map(CompElement::from);
    stage.into_iter().chain(paper).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Values;

    fn theme(kv: &[(&str, &str)]) -> Theme {
        let mut v = Values::default();
        for (k, x) in kv {
            v.set(k, x);
        }
        Theme::build(&v)
    }

    fn split(p: u32) -> [i32; 3] {
        [(p >> 16 & 255) as i32, (p >> 8 & 255) as i32, (p & 255) as i32]
    }

    #[test]
    fn dots_every_24_board_px_over_the_mode_ground() {
        for mode in ["dark", "light"] {
            let t = theme(&[("theme.mode", mode)]);
            let p = Paper::new(&t, 2560, 1440);
            assert_eq!(p.period, 32.0, "24 board px at 4/3");
            // Far right top, outside the glows: the ground and a dot in the line colour.
            let bg = split(crate::canvas::pack(t.bg));
            let dot = split(crate::canvas::pack(t.line));
            let lum = |c: [i32; 3]| c[0] + c[1] + c[2];
            let at_dot = split(p.pixel(2560 - 16, 16 + 32 * 10));
            let between = split(p.pixel(2560 - 32, 32 * 10));
            assert!((lum(at_dot) - lum(dot)).abs() < (lum(at_dot) - lum(bg)).abs(), "{mode}: dot {at_dot:?}");
            assert!((lum(between) - lum(bg)).abs() < 40, "{mode}: ground {between:?} vs {bg:?}");
        }
    }

    #[test]
    fn the_glow_lights_the_stage_and_fades_and_the_dither_hides_bands() {
        let t = theme(&[("theme.mode", "dark"), ("theme.scanlines", "false")]);
        let p = Paper::new(&t, 2560, 1440);
        let g = |x: i32, y: i32| split(p.pixel(x, y))[1];
        // Green rises toward the glow's centre (1280, 493) from the ground at the left edge.
        assert!(g(1281, 495) > g(600, 495) + 4 && g(600, 495) > g(10, 495), "{} {} {}", g(1281, 495), g(600, 495), g(10, 495));
        // A row through the glow's slope: neighbouring pixels differ by at most 1 level (dither), and the 16-px
        // mean falls smoothly (no step of a whole level over a long run = no visible band).
        let row: Vec<i32> = (900..1180).map(|x| g(x, 1001)).collect();
        assert!(row.windows(2).all(|w| (w[0] - w[1]).abs() <= 2), "jumps in {row:?}");
        let distinct: std::collections::HashSet<i32> = row.iter().copied().collect();
        assert!(distinct.len() >= 2, "the dither mixes neighbouring levels");
    }

    #[test]
    fn scanlines_only_dark_and_only_when_on() {
        let on = Paper::new(&theme(&[("theme.mode", "dark")]), 640, 360);
        let off = Paper::new(&theme(&[("theme.mode", "dark"), ("theme.scanlines", "false")]), 640, 360);
        let light = Paper::new(&theme(&[("theme.mode", "light")]), 640, 360);
        assert!(on.scan.is_some() && off.scan.is_none() && light.scan.is_none());
        let (p, _, a) = on.scan.unwrap();
        assert_eq!(p, 4);
        assert!((a - 0.028 * 4.0 / 3.0).abs() < 1e-6, "the design's mean strength");
        // A scan row is lighter than the next row, at the same place.
        let lum = |pp: &Paper, x: i32, y: i32| split(pp.pixel(x, y)).iter().sum::<i32>();
        let sum = |pp: &Paper, y: i32| (0..64).map(|x| lum(pp, x + 300, y)).sum::<i32>();
        assert!(sum(&on, 200) > sum(&on, 201) + 64 * 3, "scan row lighter");
        assert_eq!(sum(&off, 200) / 64, sum(&off, 201) / 64);
    }

    #[test]
    fn gpu_level_draws_exactly_the_cheap_wallpaper_and_the_key_follows_the_theme() {
        let cheap = Paper::new(&theme(&[("theme.effects", "cheap")]), 320, 200);
        let gpu = Paper::new(&theme(&[("theme.effects", "gpu")]), 320, 200);
        assert_eq!(cheap.bytes(), gpu.bytes());
        let other = Paper::new(&theme(&[("theme.accent", "#ff8800")]), 320, 200);
        assert_ne!(cheap.bytes(), other.bytes(), "the glow follows the accent");
    }

    #[test]
    fn the_fast_picture_is_the_exact_one_within_a_level() {
        for mode in ["dark", "light"] {
            let p = Paper::new(&theme(&[("theme.mode", mode)]), 900, 500);
            let b = p.bytes();
            let mut worst = 0;
            for y in (0..500).step_by(7) {
                for x in 0..900 {
                    let i = ((y * 900 + x) * 4) as usize;
                    let fast = u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
                    let (f, e) = (split(fast), split(p.pixel(x, y)));
                    worst = worst.max((0..3).map(|k| (f[k] - e[k]).abs()).max().unwrap());
                }
            }
            assert!(worst <= 1, "{mode}: {worst} levels apart");
        }
    }

    #[test]
    fn the_full_output_is_drawn_once_in_reasonable_time() {
        let t = theme(&[]);
        let t0 = std::time::Instant::now();
        let b = Paper::new(&t, 2560, 1440).bytes();
        let ms = t0.elapsed().as_millis();
        assert_eq!(b.len(), 2560 * 1440 * 4);
        assert!(b.chunks(4).all(|p| p[3] == 255), "opaque");
        eprintln!("wallpaper 2560x1440 drawn in {ms} ms");
    }
}
