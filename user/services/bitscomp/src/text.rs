//! Text of the interface (WY-5): Exo 2 (OFL, fonts/OFL.txt) rendered by fontdue with grey-level antialiasing. The
//! four weights the mock-up uses are built into the binary, so bitscomp needs no font files on V2 and no fontconfig.
//! One glyph cache for all sizes (key: weight, pixel size, character); it is a cache, emptied when it grows large.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use fontdue::{Font as Face, FontSettings, Metrics};

use crate::canvas::Canvas;

pub const LICENCE: &str = include_str!("../fonts/OFL.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Weight {
    Light,
    Regular,
    Medium,
    SemiBold,
}

fn faces() -> &'static [Face; 4] {
    static FACES: OnceLock<[Face; 4]> = OnceLock::new();
    FACES.get_or_init(|| {
        let load = |b: &'static [u8]| Face::from_bytes(b, FontSettings::default()).expect("built-in Exo 2 parses");
        [
            load(include_bytes!("../fonts/Exo2-300.ttf")),
            load(include_bytes!("../fonts/Exo2-400.ttf")),
            load(include_bytes!("../fonts/Exo2-500.ttf")),
            load(include_bytes!("../fonts/Exo2-600.ttf")),
        ]
    })
}

fn face(w: Weight) -> &'static Face {
    &faces()[w as usize]
}

struct Glyph {
    m: Metrics,
    cov: Vec<u8>,
}

/// Entries kept before the cache starts over (a cache bound, not a limit on anything shown).
const CACHE_KEEP: usize = 8192;

thread_local! {
    static CACHE: RefCell<HashMap<(Weight, u32, char), Rc<Glyph>>> = RefCell::new(HashMap::new());
}

fn glyph(w: Weight, px: f32, c: char) -> Rc<Glyph> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = (w, px.to_bits(), c);
        if let Some(g) = cache.get(&key) {
            return g.clone();
        }
        if cache.len() >= CACHE_KEEP {
            cache.clear();
        }
        let (m, cov) = face(w).rasterize(c, px);
        let g = Rc::new(Glyph { m, cov });
        cache.insert(key, g.clone());
        g
    })
}

/// A text style at output resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Font {
    weight: Weight,
    /// Size in physical pixels.
    px: f32,
    /// Letter spacing in physical pixels.
    track: f32,
}

impl Font {
    /// `logical` px (CSS px of the mock-up) at output scale `scale`.
    pub fn new(logical: f32, weight: Weight, scale: f64) -> Font {
        Font { weight, px: (logical as f64 * scale) as f32, track: 0.0 }
    }

    /// Letter spacing in em (the mock-up's `letter-spacing: 0.12em`).
    pub fn spaced(self, em: f32) -> Font {
        Font { track: em * self.px, ..self }
    }

    fn line(&self) -> (f32, f32) {
        face(self.weight).horizontal_line_metrics(self.px).map_or((self.px * 0.8, -self.px * 0.2), |l| (l.ascent, l.descent))
    }

    /// The line's height in pixels (ascent to descent).
    pub fn height(&self) -> i32 {
        let (a, d) = self.line();
        (a - d).round() as i32
    }

    /// Pen positions: each character with its x (pixels from the start), and the total advance.
    fn layout(&self, s: &str) -> (Vec<(char, f32)>, f32) {
        let f = face(self.weight);
        let mut x = 0.0;
        let mut out = Vec::with_capacity(s.len());
        let mut prev: Option<char> = None;
        for c in s.chars() {
            if let Some(p) = prev {
                x += f.horizontal_kern(p, c, self.px).unwrap_or(0.0);
            }
            out.push((c, x));
            x += f.metrics(c, self.px).advance_width + self.track;
            prev = Some(c);
        }
        (out, (x - if prev.is_some() { self.track } else { 0.0 }).max(0.0))
    }

    pub fn width(&self, s: &str) -> i32 {
        self.layout(s).1.ceil() as i32
    }

    /// Each character's box (x, width) in pixels from the start of the text (for `ctl hot`).
    pub fn boxes(&self, s: &str) -> Vec<(i32, i32)> {
        let f = face(self.weight);
        self.layout(s).0.iter().map(|&(c, x)| (x.round() as i32, f.metrics(c, self.px).advance_width.round() as i32)).collect()
    }

    /// `s`, cut with an ellipsis when wider than `max` pixels.
    pub fn fit(&self, s: &str, max: i32) -> String {
        if self.width(s) <= max {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        let mut n = chars.len();
        while n > 0 {
            n -= 1;
            let cut: String = chars[..n].iter().collect::<String>().trim_end().to_string() + "…";
            if self.width(&cut) <= max {
                return cut;
            }
        }
        if self.width("…") <= max { "…".into() } else { String::new() }
    }

    /// Whether Exo 2 has a glyph for `c` (else the font's empty box is drawn).
    #[cfg(test)]
    pub fn has(c: char) -> bool {
        face(Weight::Regular).has_glyph(c)
    }

    /// Draws `s` with its line's top at (x, y) in premultiplied colour `c`; returns the width.
    pub fn draw(&self, canvas: &mut Canvas, x: i32, y: i32, s: &str, c: u32) -> i32 {
        let (ascent, _) = self.line();
        let base = y as f32 + ascent;
        let (chars, w) = self.layout(s);
        for (ch, pen) in chars {
            if ch == ' ' {
                continue;
            }
            let g = glyph(self.weight, self.px, ch);
            let gx = x + (pen.round() as i32) + g.m.xmin;
            let gy = (base.round() as i32) - g.m.ymin - g.m.height as i32;
            for row in 0..g.m.height {
                for col in 0..g.m.width {
                    let v = g.cov[row * g.m.width + col];
                    canvas.put_cov(gx + col as i32, gy + row as i32, c, v);
                }
            }
        }
        w.ceil() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umlauts_the_euro_sign_and_the_ellipsis_are_in_exo2() {
        for c in ['ä', 'ö', 'ü', 'Ä', 'Ö', 'Ü', 'ß', '€', '…', '·', '°', '‹', '›', '„', '“'] {
            assert!(Font::has(c), "Exo 2 lacks {c}");
        }
    }

    #[test]
    fn widths_follow_size_scale_and_spacing() {
        let a = Font::new(15.0, Weight::Regular, 1.0);
        let b = Font::new(15.0, Weight::Regular, 1.25);
        let (wa, wb) = (a.width("Arbeitsräume"), b.width("Arbeitsräume"));
        assert!(wa > 60 && wb > wa && (wb as f32 / wa as f32 - 1.25).abs() < 0.1, "{wa} {wb}");
        assert!(a.spaced(0.12).width("ARBEITSRÄUME") > a.width("ARBEITSRÄUME"));
        assert!(b.height() > a.height());
        assert_eq!(a.width(""), 0);
    }

    #[test]
    fn fitting_cuts_with_an_ellipsis() {
        let f = Font::new(13.0, Weight::Regular, 1.25);
        assert_eq!(f.fit("Grüße", 1000), "Grüße");
        let cut = f.fit("Langer_Fenstertitel_mit_viel_Text", 80);
        assert!(cut.ends_with('…') && f.width(&cut) <= 80, "{cut}");
        assert_eq!(f.fit("Grüße", 0), "");
    }

    #[test]
    fn drawing_puts_ink_inside_the_line() {
        let f = Font::new(15.0, Weight::SemiBold, 1.25);
        let mut c = Canvas::new(120, 30);
        let w = c.text(&f, 2, 2, "Grüße €", [1.0; 4]);
        assert!(w > 40);
        let ink = (0..c.h).flat_map(|y| (0..c.w).map(move |x| (x, y))).filter(|&(x, y)| c.pixel(x, y) >> 24 > 128).count();
        assert!(ink > 80, "{ink} pixels of ink");
    }
}
