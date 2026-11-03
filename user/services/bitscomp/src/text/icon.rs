//! The interface's glyphs no shipped family has (fonts/README): check, cross, the small triangles and the dot, drawn
//! as paths into the same coverage bitmaps as a font's glyphs, so they sit in the line, are measured and cached
//! like letters and look the same in every family.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Check,
    Cross,
    Down,
    Right,
    Dot,
}

pub fn of(c: char) -> Option<Icon> {
    Some(match c {
        '\u{2713}' => Icon::Check,
        '\u{2717}' => Icon::Cross,
        '\u{25BE}' => Icon::Down,
        '\u{25B8}' => Icon::Right,
        '\u{25CF}' => Icon::Dot,
        _ => return None,
    })
}

/// Pen advance in em.
pub fn advance(i: Icon) -> f32 {
    match i {
        Icon::Check => 0.78,
        Icon::Cross => 0.72,
        Icon::Down => 0.62,
        Icon::Right => 0.5,
        Icon::Dot => 0.7,
    }
}

/// Stroke width in em (about 1.5 board px at 13-16 px text, the boards' icon strokes).
const STROKE: f32 = 0.1;

/// Coordinates in em: x from the pen, y up from the baseline, centred on the lower-case middle (0.35 em).
enum Shape {
    Stroke(&'static [(f32, f32)]),
    Fill(&'static [(f32, f32)]),
    Disc((f32, f32), f32),
}

fn shapes(i: Icon) -> &'static [Shape] {
    match i {
        Icon::Check => &[Shape::Stroke(&[(0.14, 0.36), (0.32, 0.18), (0.64, 0.56)])],
        Icon::Cross => &[Shape::Stroke(&[(0.16, 0.15), (0.56, 0.55)]), Shape::Stroke(&[(0.16, 0.55), (0.56, 0.15)])],
        Icon::Down => &[Shape::Fill(&[(0.13, 0.47), (0.49, 0.47), (0.31, 0.21)])],
        Icon::Right => &[Shape::Fill(&[(0.15, 0.52), (0.41, 0.34), (0.15, 0.16)])],
        Icon::Dot => &[Shape::Disc((0.35, 0.35), 0.25)],
    }
}

fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy).max(1e-12)).clamp(0.0, 1.0);
    (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy)
}

/// Even-odd point in polygon.
fn inside(p: (f32, f32), poly: &[(f32, f32)]) -> bool {
    let mut on = false;
    for (k, &a) in poly.iter().enumerate() {
        let b = poly[(k + 1) % poly.len()];
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0) {
            on = !on;
        }
    }
    on
}

fn hit(s: &Shape, p: (f32, f32)) -> bool {
    match s {
        Shape::Stroke(pts) => pts.windows(2).any(|w| seg_dist(p, w[0], w[1]) <= STROKE / 2.0),
        Shape::Fill(pts) => inside(p, pts),
        Shape::Disc(c, r) => (p.0 - c.0).hypot(p.1 - c.1) <= *r,
    }
}

/// The icon at `px` as (xmin, ymin, width, height, coverage rows top first), in fontdue's convention (ymin: the
/// bitmap's bottom above the baseline).
pub fn raster(i: Icon, px: f32) -> (i32, i32, usize, usize, Vec<u8>) {
    const SS: usize = 4;
    let shapes = shapes(i);
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for s in shapes {
        let (pts, pad): (Vec<(f32, f32)>, f32) = match s {
            Shape::Stroke(p) => (p.to_vec(), STROKE / 2.0),
            Shape::Fill(p) => (p.to_vec(), 0.0),
            Shape::Disc(c, r) => (vec![*c], *r),
        };
        for (x, y) in pts {
            (x0, y0, x1, y1) = (x0.min(x - pad), y0.min(y - pad), x1.max(x + pad), y1.max(y + pad));
        }
    }
    let xmin = (x0 * px).floor() as i32 - 1;
    let ymin = (y0 * px).floor() as i32 - 1;
    let w = ((x1 * px).ceil() as i32 + 1 - xmin).max(1) as usize;
    let h = ((y1 * px).ceil() as i32 + 1 - ymin).max(1) as usize;
    let mut cov = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            let mut n = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (xmin as f32 + col as f32 + (sx as f32 + 0.5) / SS as f32) / px;
                    let y = (ymin as f32 + (h - 1 - row) as f32 + (sy as f32 + 0.5) / SS as f32) / px;
                    if shapes.iter().any(|s| hit(s, (x, y))) {
                        n += 1;
                    }
                }
            }
            cov[row * w + col] = (n * 255 / (SS * SS)) as u8;
        }
    }
    (xmin, ymin, w, h, cov)
}
