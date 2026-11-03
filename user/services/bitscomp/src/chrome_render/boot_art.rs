//! DS-5: the boot scene's pieces, made at run time the way crates/console/build.rs makes the kernel's (same font
//! files, sizes, fontdue calls, ring maths), and the integer blitters of crates/console/src/bootart.rs. bitscomp is
//! its own workspace without the kernel crates, so this is a re-implementation; `boot_tests` pins its numbers and
//! host/tests/ds/60-boot.sh compares it with the kernel's picture.

use std::collections::HashMap;

use crate::text::{Typeface, Weight, face_of};

/// Design pixels to screen pixels (build.rs's `S`, as f32 so the sizes round the same).
const S: f32 = 4.0 / 3.0;

/// A rectangle of the screen (x, y, w, h), in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w: w.max(0), h: h.max(0) }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// The part of `self` inside `b` (empty when they do not meet).
    pub fn cut(self, b: Rect) -> Rect {
        let (x0, y0) = (self.x.max(b.x), self.y.max(b.y));
        let (x1, y1) = ((self.x + self.w).min(b.x + b.w), (self.y + self.h).min(b.y + b.h));
        if x1 <= x0 || y1 <= y0 { Rect::new(x0, y0, 0, 0) } else { Rect::new(x0, y0, x1 - x0, y1 - y0) }
    }

    fn holds(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// XRGB pixels of one screen rectangle (`at`): the whole first picture, or one row drawn again.
pub struct Frame {
    pub at: Rect,
    pub px: Vec<u32>,
}

impl Frame {
    pub fn new(at: Rect) -> Frame {
        Frame { at, px: vec![0; (at.w * at.h) as usize] }
    }

    pub fn get_mut(&mut self, x: i32, y: i32) -> Option<&mut u32> {
        if !self.at.holds(x, y) {
            return None;
        }
        self.px.get_mut(((y - self.at.y) * self.at.w + (x - self.at.x)) as usize)
    }

    /// Blends `cov` of `color` into (x, y) if it lies in `clip` (and in the frame).
    pub fn put(&mut self, clip: Rect, x: i32, y: i32, color: u32, cov: u8) {
        if cov == 0 || !clip.holds(x, y) {
            return;
        }
        if let Some(p) = self.get_mut(x, y) {
            *p = blend(color, *p, cov);
        }
    }
}

/// `fg` over `bg` at `a`/255 per channel, rounded (crates/console/src/font.rs `blend`).
pub fn blend(fg: u32, bg: u32, a: u8) -> u32 {
    match a {
        0 => bg,
        255 => fg,
        a => {
            let a = a as u32;
            let mut out = 0;
            for shift in [0, 8, 16] {
                let f = (fg >> shift) & 0xFF;
                let b = (bg >> shift) & 0xFF;
                out |= ((f * a + b * (255 - a) + 127) / 255) << shift;
            }
            out
        }
    }
}

/// Premultiplied `src` over `dst`, per 8-bit channel, rounded.
fn over(dst: u32, [r, g, b, a]: [u8; 4]) -> u32 {
    let k = 255 - a as u32;
    let ch = |s: u8, shift: u32| ((s as u32 + (((dst >> shift) & 0xFF) * k + 127) / 255).min(255)) << shift;
    ch(r, 16) | ch(g, 8) | ch(b, 0)
}

struct Glyph {
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    adv: i32,
    cov: Vec<u8>,
}

/// One font at one size, glyphs made as build.rs's atlases (printable ASCII, Latin-1, – … →; '?' for the rest).
pub struct Face {
    pub ascent: i32,
    pub descent: i32,
    glyphs: HashMap<char, Glyph>,
}

fn charset() -> Vec<char> {
    let mut v: Vec<char> = (0x20u32..=0x7E).chain(0xA0..=0xFF).filter_map(char::from_u32).collect();
    v.extend(['–', '…', '→']);
    v
}

impl Face {
    pub fn new(tf: Typeface, w: Weight, design_px: f32) -> Face {
        let f = face_of(tf, w);
        let px = design_px * S;
        let (mut ascent, mut descent) = (0, 0);
        if let Some(m) = f.horizontal_line_metrics(px) {
            ascent = (m.ascent * 256.0).round() as i32;
            descent = (m.descent * 256.0).round() as i32;
        }
        let mut glyphs = HashMap::new();
        for ch in charset() {
            if f.lookup_glyph_index(ch) == 0 && ch != ' ' {
                continue;
            }
            let (m, cov) = f.rasterize(ch, px);
            let y = -(m.ymin + m.height as i32);
            glyphs.insert(ch, Glyph { w: m.width, h: m.height, x: m.xmin, y, adv: (m.advance_width * 256.0).round() as i32, cov });
        }
        Face { ascent, descent, glyphs }
    }

    fn glyph(&self, c: char) -> Option<&Glyph> {
        self.glyphs.get(&c).or_else(|| self.glyphs.get(&'?'))
    }

    /// Width of `text` in 1/256 pixels with `spacing` (1/256 pixels) after every character.
    pub fn width256(&self, text: &str, spacing: i32) -> i32 {
        text.chars().map(|c| self.glyph(c).map_or(0, |g| g.adv) + spacing).sum()
    }

    pub fn width(&self, text: &str, spacing: i32) -> i32 {
        (self.width256(text, spacing) + 128) >> 8
    }

    /// The baseline of a CSS line box starting at `top` with `line` pixels height (half-leading above).
    pub fn baseline(&self, top: i32, line: i32) -> i32 {
        let content = self.ascent - self.descent;
        top + ((line * 256 - content) / 2 + self.ascent + 128) / 256
    }

    /// The longest start of `text` that fits in `max` pixels, and whether it was cut (room left for "…").
    pub fn fit<'a>(&self, text: &'a str, max: i32, spacing: i32) -> (&'a str, bool) {
        if self.width(text, spacing) <= max {
            return (text, false);
        }
        let room = max * 256 - self.width256("…", spacing);
        let mut used = 0;
        for (i, c) in text.char_indices() {
            used += self.glyph(c).map_or(0, |g| g.adv) + spacing;
            if used > room {
                return (&text[..i], true);
            }
        }
        (text, false)
    }

    /// Draws `s` with its first pen position at `x` on `baseline`, only inside `clip`.
    pub fn text(&self, f: &mut Frame, x: i32, baseline: i32, s: &str, color: u32, spacing: i32, clip: Rect) {
        let mut pen = x * 256;
        for c in s.chars() {
            let Some(g) = self.glyph(c) else { continue };
            let gx = ((pen + 128) >> 8) + g.x;
            let gy = baseline + g.y;
            if gy + g.h as i32 > clip.y && gy < clip.y + clip.h {
                for yy in 0..g.h {
                    for xx in 0..g.w {
                        f.put(clip, gx + xx as i32, gy + yy as i32, color, g.cov[yy * g.w + xx]);
                    }
                }
            }
            pen += g.adv + spacing;
        }
    }
}

/// A whole word as one coverage picture (the title: kerning and the design's letter spacing included).
pub struct Word {
    pub w: usize,
    pub h: usize,
    pub ascent: i32,
    pub descent: i32,
    bits: Vec<u8>,
}

impl Word {
    pub fn new(tf: Typeface, wt: Weight, design_px: f32, text: &str, spacing_em: f32) -> Word {
        let f = face_of(tf, wt);
        let px = design_px * S;
        let Some(m) = f.horizontal_line_metrics(px) else { return Word { w: 0, h: 0, ascent: 0, descent: 0, bits: Vec::new() } };
        let (asc, desc) = (m.ascent.round() as i32, m.descent.round() as i32);
        let mut pen = 0.0f32;
        let mut prev: Option<char> = None;
        let mut glyphs = Vec::new();
        for ch in text.chars() {
            if let Some(p) = prev {
                pen += f.horizontal_kern(p, ch, px).unwrap_or(0.0);
            }
            let (gm, cov) = f.rasterize(ch, px);
            glyphs.push((gm, cov, pen));
            pen += gm.advance_width + spacing_em * px;
            prev = Some(ch);
        }
        let right = glyphs.iter().map(|(m, _, pen)| pen.round() as i32 + m.xmin + m.width as i32).max().unwrap_or(0);
        let w = right.max(1) as usize;
        let h = (asc - desc).max(0) as usize;
        let mut bits = vec![0u8; w * h];
        for (m, cov, pen) in &glyphs {
            let x0 = pen.round() as i32 + m.xmin;
            let y0 = asc - (m.ymin + m.height as i32);
            for gy in 0..m.height {
                for gx in 0..m.width {
                    let (x, y) = (x0 + gx as i32, y0 + gy as i32);
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let p = &mut bits[y as usize * w + x as usize];
                        *p = p.saturating_add(cov[gy * m.width + gx]);
                    }
                }
            }
        }
        Word { w, h, ascent: asc, descent: desc, bits }
    }

    /// The word with its top-left corner at (x, y).
    pub fn draw(&self, f: &mut Frame, x: i32, y: i32, color: u32, clip: Rect) {
        let y0 = (clip.y - y).clamp(0, self.h as i32) as usize;
        let y1 = (clip.y + clip.h - y).clamp(0, self.h as i32) as usize;
        for yy in y0..y1 {
            for xx in 0..self.w {
                f.put(clip, x + xx as i32, y + yy as i32, color, self.bits[yy * self.w + xx]);
            }
        }
    }
}

pub use super::boot_ring::Ring;

/// Premultiplied RGBA of the ring sprite blended over the frame, top-left corner at (x, y).
pub fn ring(f: &mut Frame, r: &Ring, x: i32, y: i32, clip: Rect) {
    let n = r.n as i32;
    let area = Rect::new(x, y, n, n).cut(clip).cut(f.at);
    for yy in area.y..area.y + area.h {
        let row = ((yy - y) * n) as usize;
        for xx in area.x..area.x + area.w {
            let p = r.px[row + (xx - x) as usize];
            if p != [0; 4]
                && let Some(d) = f.get_mut(xx, yy)
            {
                *d = over(*d, p);
            }
        }
    }
}
