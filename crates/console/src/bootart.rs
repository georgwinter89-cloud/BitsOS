//! DS-4: the boot screen's pre-made pictures (`build.rs`: glyph atlases, the title word, the ring sprite) and the
//! integer blitters that put them into the display's RAM frame. No allocation, no lock, no log: the callers run
//! under `display.frame`.

use crate::font::blend;
use display::{RamFrame, Rect};

/// One glyph of an atlas: coverage `w` x `h` at `bits[off..]`, drawn at (pen + `x`, baseline + `y`); the pen
/// moves by `adv` / 256 pixels.
pub struct Glyph {
    pub ch: u32,
    pub w: u16,
    pub h: u16,
    pub x: i16,
    pub y: i16,
    pub adv: u32,
    pub off: u32,
}

/// A font at one size. `ascent`/`descent` in 1/256 pixels (descent negative).
pub struct Face {
    pub px256: u32,
    pub ascent: i32,
    pub descent: i32,
    pub glyphs: &'static [Glyph],
    pub bits: &'static [u8],
}

/// A whole word as one coverage picture (the title).
pub struct Word {
    pub w: usize,
    pub h: usize,
    pub ascent: i32,
    pub descent: i32,
    pub bits: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/bootart.rs"));

/// The pictures were made (the fonts were there at build time).
pub fn ready() -> bool {
    READY && !MONO.glyphs.is_empty()
}

impl Face {
    /// The glyph for `c`, else '?'.
    pub fn glyph(&self, c: char) -> Option<&Glyph> {
        let find = |c: u32| self.glyphs.binary_search_by_key(&c, |g| g.ch).ok().map(|i| &self.glyphs[i]);
        find(c as u32).or_else(|| find('?' as u32))
    }

    /// Width of `text` in 1/256 pixels with `spacing` (1/256 pixels) after every character.
    pub fn width256(&self, text: &str, spacing: i32) -> i32 {
        text.chars().map(|c| self.glyph(c).map_or(0, |g| g.adv as i32) + spacing).sum()
    }

    pub fn width(&self, text: &str, spacing: i32) -> i32 {
        (self.width256(text, spacing) + 128) >> 8
    }

    /// The baseline of a CSS line box starting at `top` with `line` pixels height (half-leading above).
    pub fn baseline(&self, top: i32, line: i32) -> i32 {
        let content = self.ascent - self.descent;
        top + ((line * 256 - content) / 2 + self.ascent + 128) / 256
    }

    /// The longest start of `text` (cut at a character) that fits in `max` pixels, with "…" when cut.
    pub fn fit<'a>(&self, text: &'a str, max: i32, spacing: i32) -> (&'a str, bool) {
        if self.width(text, spacing) <= max {
            return (text, false);
        }
        let room = max * 256 - self.width256("…", spacing);
        let mut used = 0;
        for (i, c) in text.char_indices() {
            used += self.glyph(c).map_or(0, |g| g.adv as i32) + spacing;
            if used > room {
                return (&text[..i], true);
            }
        }
        (text, false)
    }
}

/// Blends `cov` (0..=255) of `color` into the pixel at (x, y) if it lies in `clip`.
fn put(px: &mut [u32], stride: usize, clip: Rect, x: i32, y: i32, color: u32, cov: u8) {
    if cov == 0 || x < clip.x as i32 || y < clip.y as i32 || x >= (clip.x + clip.w) as i32 || y >= (clip.y + clip.h) as i32 {
        return;
    }
    let i = y as usize * stride + x as usize;
    if let Some(p) = px.get_mut(i) {
        *p = blend(color, *p, cov);
    }
}

/// Draws `text` with its first pen position at `x` on `baseline`; returns the pen's end. Only pixels inside
/// `clip` (which must lie in the frame) change; the caller marks the damage.
pub fn text(f: &mut RamFrame, face: &Face, x: i32, baseline: i32, s: &str, color: u32, spacing: i32, clip: Rect) -> i32 {
    let stride = f.width as usize;
    let px = f.pixels_mut();
    let mut pen = x * 256;
    for c in s.chars() {
        let Some(g) = face.glyph(c) else { continue };
        let gx = ((pen + 128) >> 8) + g.x as i32;
        let gy = baseline + g.y as i32;
        let (w, h) = (g.w as usize, g.h as usize);
        // A glyph outside the clip's rows (another band of the first picture) costs nothing.
        let outside = gy + h as i32 <= clip.y as i32 || gy >= (clip.y + clip.h) as i32;
        if !outside && let Some(cov) = face.bits.get(g.off as usize..g.off as usize + w * h) {
            for yy in 0..h {
                for xx in 0..w {
                    put(px, stride, clip, gx + xx as i32, gy + yy as i32, color, cov[yy * w + xx]);
                }
            }
        }
        pen += g.adv as i32 + spacing;
    }
    (pen + 128) >> 8
}

/// The title word with its top-left corner at (x, y).
pub fn word(f: &mut RamFrame, wd: &Word, x: i32, y: i32, color: u32, clip: Rect) {
    let stride = f.width as usize;
    let px = f.pixels_mut();
    // Only the rows inside the clip (the first picture is drawn in bands).
    let y0 = (clip.y as i32 - y).clamp(0, wd.h as i32) as usize;
    let y1 = ((clip.y + clip.h) as i32 - y).clamp(0, wd.h as i32) as usize;
    for yy in y0..y1 {
        for xx in 0..wd.w {
            put(px, stride, clip, x + xx as i32, y + yy as i32, color, wd.bits[yy * wd.w + xx]);
        }
    }
}

/// The ring sprite (premultiplied RGBA spans) with its top-left corner at (x, y).
pub fn ring(f: &mut RamFrame, x: i32, y: i32, clip: Rect) {
    let stride = f.width as usize;
    let px = f.pixels_mut();
    for &(sy, sx, len, off) in RING_SPANS {
        let yy = y + sy as i32;
        if yy < clip.y as i32 || yy >= (clip.y + clip.h) as i32 {
            continue;
        }
        for i in 0..len as usize {
            let xx = x + sx as i32 + i as i32;
            if xx < clip.x as i32 || xx >= (clip.x + clip.w) as i32 {
                continue;
            }
            let o = (off as usize + i) * 4;
            let Some(&[r, g, b, a]) = RING_BITS.get(o..o + 4).and_then(|s| <&[u8; 4]>::try_from(s).ok()) else { continue };
            if let Some(p) = px.get_mut(yy as usize * stride + xx as usize) {
                *p = over(*p, r, g, b, a);
            }
        }
    }
}

/// Premultiplied `src` over `dst`, per 8-bit channel, rounded.
pub fn over(dst: u32, r: u8, g: u8, b: u8, a: u8) -> u32 {
    let k = 255 - a as u32;
    let ch = |s: u8, shift: u32| ((s as u32 + (((dst >> shift) & 0xFF) * k + 127) / 255).min(255)) << shift;
    ch(r, 16) | ch(g, 8) | ch(b, 0)
}
