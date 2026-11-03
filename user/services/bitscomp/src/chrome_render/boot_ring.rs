//! DS-5: the boot scene's HUD ring (the design's 520 px ring of 7 circles at rest, x 4/3), made with the maths of
//! crates/console/build.rs `ring` so bitscomp's first frame shows the kernel's ring pixel for pixel. Each layer is
//! only computed inside its own square (outside it the coverage is 0, so the result is the same), which keeps the
//! one-time cost at a fraction of the whole sprite's.

/// Sprite side and centre: the ring's 520 design pixels at 4/3, centred on a whole pixel corner.
const N: usize = 694;
const C: f32 = 347.0;
const S: f32 = 4.0 / 3.0;
const ACCENT: u32 = 0x7fcfa0;

type Rgba = [f32; 4];

/// The ring as premultiplied RGBA bytes, `n` x `n`.
pub struct Ring {
    pub n: usize,
    pub px: Vec<[u8; 4]>,
}

/// A square of the sprite, [lo, hi) on both axes.
#[derive(Clone, Copy)]
struct Area {
    lo: usize,
    hi: usize,
}

impl Area {
    fn around(reach: f32) -> Area {
        let r = reach.ceil() as i32 + 2;
        Area { lo: (C as i32 - r).max(0) as usize, hi: (C as i32 + r).min(N as i32) as usize }
    }

    fn grow(self, by: usize) -> Area {
        Area { lo: self.lo.saturating_sub(by), hi: (self.hi + by).min(N) }
    }
}

struct Layer {
    area: Area,
    px: Vec<Rgba>,
}

fn rgb(hex: u32, alpha: f32) -> Rgba {
    let c = |s: u32| ((hex >> s) & 0xFF) as f32 / 255.0 * alpha;
    [c(16), c(8), c(0), alpha]
}

/// Coverage of a circle stroke (radius `r`, width `w`, design pixels) with an optional SVG dash pattern, painted.
fn stroke(r: f32, w: f32, dash: &[f32], color: Rgba) -> Layer {
    let (r, hw) = (r * S, w * S / 2.0);
    let dash: Vec<f32> = dash.iter().map(|d| d * S).collect();
    let period: f32 = dash.iter().sum();
    let area = Area::around(r + hw + 1.0);
    let mut px = vec![[0f32; 4]; N * N];
    for y in area.lo..area.hi {
        for x in area.lo..area.hi {
            let (dx, dy) = (x as f32 + 0.5 - C, y as f32 + 0.5 - C);
            let d = (dx * dx + dy * dy).sqrt();
            let radial = (hw + 0.5 - (d - r).abs()).clamp(0.0, 1.0);
            if radial == 0.0 {
                continue;
            }
            let along = if dash.is_empty() {
                1.0
            } else {
                // Arc length from 3 o'clock, clockwise on screen (SVG's start of a circle), at the stroke's radius.
                let mut a = dy.atan2(dx);
                if a < 0.0 {
                    a += std::f32::consts::TAU;
                }
                let pos = (a * r) % period;
                let mut best = f32::MIN;
                for shift in [-period, 0.0, period] {
                    let mut at = shift;
                    for (i, len) in dash.iter().enumerate() {
                        if i % 2 == 0 {
                            best = best.max((pos - at).min(at + len - pos));
                        }
                        at += len;
                    }
                }
                (best + 0.5).clamp(0.0, 1.0)
            };
            let a = radial * along;
            px[y * N + x] = [color[0] * a, color[1] * a, color[2] * a, color[3] * a];
        }
    }
    Layer { area, px }
}

fn disc(r: f32, color: Rgba) -> Layer {
    let r = r * S;
    let area = Area::around(r + 1.0);
    let mut px = vec![[0f32; 4]; N * N];
    for y in area.lo..area.hi {
        for x in area.lo..area.hi {
            let (dx, dy) = (x as f32 + 0.5 - C, y as f32 + 0.5 - C);
            let a = (r + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            px[y * N + x] = [color[0] * a, color[1] * a, color[2] * a, color[3] * a];
        }
    }
    Layer { area, px }
}

/// The four ticks: axis-aligned rectangles in design coordinates of the 520 box (box-filter coverage).
fn ticks(color: Rgba) -> Layer {
    let mut cov = vec![0f32; N * N];
    for (x0, y0, x1, y1) in [(259.0, 0.0, 261.0, 26.0), (259.0, 494.0, 261.0, 520.0), (0.0, 259.0, 26.0, 261.0), (494.0, 259.0, 520.0, 261.0)] {
        let map = |u: f32| C + (u - 260.0) * S;
        let (x0, x1, y0, y1) = (map(x0), map(x1), map(y0), map(y1));
        for y in 0..N {
            let oy = ((y as f32 + 1.0).min(y1) - (y as f32).max(y0)).max(0.0);
            if oy == 0.0 {
                continue;
            }
            for x in 0..N {
                let ox = ((x as f32 + 1.0).min(x1) - (x as f32).max(x0)).max(0.0);
                cov[y * N + x] = (cov[y * N + x] + ox * oy).min(1.0);
            }
        }
    }
    let px = cov.iter().map(|&a| [color[0] * a, color[1] * a, color[2] * a, color[3] * a]).collect();
    Layer { area: Area { lo: 0, hi: N }, px }
}

fn over(dst: &mut [Rgba], src: &Layer) {
    let a = src.area;
    for y in a.lo..a.hi {
        for x in a.lo..a.hi {
            let (d, s) = (&mut dst[y * N + x], src.px[y * N + x]);
            let k = 1.0 - s[3];
            for i in 0..4 {
                d[i] = s[i] + d[i] * k;
            }
        }
    }
}

/// feGaussianBlur (sigma in design pixels) as three box blurs (Kovesi's boxes-for-gauss), inside the layer's
/// square grown by the blur's reach.
fn glow(layer: &Layer, sigma: f32) -> Layer {
    let sigma = sigma * S;
    let wi = ((12.0 * sigma * sigma / 3.0 + 1.0).sqrt()).floor() as i32;
    let wl = if wi % 2 == 0 { wi - 1 } else { wi };
    let m = ((12.0 * sigma * sigma - 3.0 * (wl * wl) as f32 - 12.0 * wl as f32 - 9.0) / (-4.0 * wl as f32 - 4.0)).round() as i32;
    let radii: Vec<usize> = (0..3).map(|i| ((if i < m { wl } else { wl + 2 } - 1) / 2) as usize).collect();
    let area = layer.area.grow(radii.iter().sum::<usize>() + 1);
    let mut b = layer.px.clone();
    for &r in &radii {
        b = box_pass(&b, r, true, area);
        b = box_pass(&b, r, false, area);
    }
    Layer { area, px: b }
}

fn box_pass(src: &[Rgba], r: usize, horizontal: bool, area: Area) -> Vec<Rgba> {
    let mut out = vec![[0f32; 4]; N * N];
    let norm = 1.0 / (2 * r + 1) as f32;
    for line in area.lo..area.hi {
        let at = |i: usize| if horizontal { line * N + i } else { i * N + line };
        let mut acc = [0f32; 4];
        for i in area.lo..=(area.lo + r).min(N - 1) {
            for (k, a) in acc.iter_mut().enumerate() {
                *a += src[at(i)][k];
            }
        }
        for i in area.lo..area.hi {
            for k in 0..4 {
                out[at(i)][k] = acc[k] * norm;
            }
            let add = i + r + 1;
            if add < N {
                for (k, a) in acc.iter_mut().enumerate() {
                    *a += src[at(add)][k];
                }
            }
            if i >= r {
                for (k, a) in acc.iter_mut().enumerate() {
                    *a -= src[at(i - r)][k];
                }
            }
        }
    }
    out
}

impl Ring {
    pub fn new() -> Ring {
        let mut acc = vec![[0f32; 4]; N * N];
        // Outer to inner in the design's paint order.
        over(&mut acc, &stroke(246.0, 10.0, &[1.2, 8.0], rgb(0x6b7a71, 1.0)));
        over(&mut acc, &stroke(228.0, 1.0, &[], rgb(ACCENT, 0.25)));
        let dashed = stroke(208.0, 4.0, &[260.0, 40.0, 90.0, 40.0, 210.0, 667.0], rgb(ACCENT, 1.0));
        over(&mut acc, &glow(&dashed, 6.0));
        over(&mut acc, &dashed);
        over(&mut acc, &stroke(180.0, 2.0, &[3.0, 7.0], rgb(0x86afd9, 0.6)));
        over(&mut acc, &stroke(156.0, 18.0, &[330.0, 160.0], rgb(ACCENT, 0.2)));
        over(&mut acc, &stroke(126.0, 1.4, &[], rgb(ACCENT, 0.65)));
        let core = stroke(62.0, 12.0, &[], rgb(0xe8eee9, 1.0));
        over(&mut acc, &glow(&core, 6.0));
        over(&mut acc, &core);
        let dot = disc(18.0, rgb(ACCENT, 1.0));
        over(&mut acc, &glow(&dot, 6.0));
        over(&mut acc, &dot);
        over(&mut acc, &ticks(rgb(ACCENT, 1.0)));
        let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        Ring { n: N, px: acc.iter().map(|p| [q(p[0]), q(p[1]), q(p[2]), q(p[3])]).collect() }
    }
}
