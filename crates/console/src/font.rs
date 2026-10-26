//! Console fonts, sized from the display mode (spec/gpu.md §5.1).
//!
//! Two faces: `Vga`, the CP437 8x16 VGA font (display's copy, public domain) scaled by a whole number (2 from
//! 1920 pixels wide: 16x32 cells at 2560x1440), and `Noto`, Noto Sans Mono pre-rasterised with grey levels
//! (`noto-sans-mono-bitmap` 0.3.2, OFL; 14x32 cells at 1440 lines, 10x24 at 1080, 7x16 below). A glyph is a
//! coverage function 0..=255 per cell pixel; the renderer blends foreground over background with it, so both
//! faces draw through the same code and the host tests check either against the same model.
//! Characters a face lacks: €, ³ and ´ are drawn from small bitmaps here (the VGA font has none of them, Noto's
//! Latin-1 range has no €); anything else becomes '?'.

use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Vga,
    Noto,
}

impl Face {
    pub fn name(self) -> &'static str {
        match self {
            Face::Vga => "vga",
            Face::Noto => "noto",
        }
    }
    pub fn parse(s: &str) -> Option<Face> {
        match s {
            "vga" => Some(Face::Vga),
            "noto" => Some(Face::Noto),
            _ => None,
        }
    }
}

/// A face at the size chosen for one mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Font {
    pub face: Face,
    /// Cell size in pixels.
    pub w: u32,
    pub h: u32,
    /// VGA: pixels per font pixel. Noto: 1.
    pub scale: u32,
    /// Noto's raster height (16, 24, 32); 0 for VGA.
    noto: u8,
}

impl Font {
    /// The font for a `width` x `height` mode: about 45 rows on the Mini-PC's 2560x1440 for either face.
    pub fn for_mode(face: Face, width: u32, height: u32) -> Font {
        match face {
            Face::Vga => {
                // Same rule as the G0 console (MCP `screen` text mode reads cells back at this scale).
                let scale = if width >= 1920 { 2 } else { 1 };
                Font { face, w: 8 * scale, h: 16 * scale, scale, noto: 0 }
            }
            Face::Noto => {
                let size = if height >= 1400 {
                    RasterHeight::Size32
                } else if height >= 1000 {
                    RasterHeight::Size24
                } else {
                    RasterHeight::Size16
                };
                let w = get_raster_width(FontWeight::Regular, size) as u32;
                Font { face, w, h: size.val() as u32, scale: 1, noto: size.val() as u8 }
            }
        }
    }

    /// Coverage of pixel (`x`, `y`) of the cell for `c`: 0 = background, 255 = foreground.
    pub fn coverage(&self, c: char, x: u32, y: u32) -> u8 {
        if x >= self.w || y >= self.h {
            return 0;
        }
        let size = match self.noto {
            32 => Some(RasterHeight::Size32),
            24 => Some(RasterHeight::Size24),
            16 => Some(RasterHeight::Size16),
            _ => None,
        };
        match size {
            Some(size) => {
                if let Some(r) = get_raster(c, FontWeight::Regular, size) {
                    return r.raster().get(y as usize).and_then(|row| row.get(x as usize)).copied().unwrap_or(0);
                }
                if let Some(bits) = extra_glyph(c) {
                    // The 8x16 bitmap stretched over the cell (nearest pixel).
                    let (fx, fy) = (x * 8 / self.w, y * 16 / self.h);
                    return if bits[fy as usize] & (0x80 >> fx) != 0 { 255 } else { 0 };
                }
                let q = get_raster('?', FontWeight::Regular, size);
                q.and_then(|r| r.raster().get(y as usize).and_then(|row| row.get(x as usize)).copied()).unwrap_or(0)
            }
            None => {
                let bits = vga_bits(c);
                let (fx, fy) = (x / self.scale, y / self.scale);
                if bits[fy as usize] & (0x80 >> fx) != 0 { 255 } else { 0 }
            }
        }
    }
}

/// The 16 rows of the VGA glyph for `c` (bit 7 = left).
pub fn vga_bits(c: char) -> &'static [u8] {
    if let Some(g) = extra_glyph(c) {
        return g;
    }
    display::font::glyph(cp437(c).unwrap_or(b'?'))
}

/// Unicode -> CP437 for what the console can show with the VGA font (`None`: not in CP437).
pub fn cp437(c: char) -> Option<u8> {
    if (' '..='~').contains(&c) {
        return Some(c as u8);
    }
    const HIGH: &[(char, u8)] = &[
        ('Ç', 0x80), ('ü', 0x81), ('é', 0x82), ('â', 0x83), ('ä', 0x84), ('à', 0x85), ('å', 0x86), ('ç', 0x87),
        ('ê', 0x88), ('ë', 0x89), ('è', 0x8A), ('ï', 0x8B), ('î', 0x8C), ('ì', 0x8D), ('Ä', 0x8E), ('Å', 0x8F),
        ('É', 0x90), ('æ', 0x91), ('Æ', 0x92), ('ô', 0x93), ('ö', 0x94), ('ò', 0x95), ('û', 0x96), ('ù', 0x97),
        ('ÿ', 0x98), ('Ö', 0x99), ('Ü', 0x9A), ('¢', 0x9B), ('£', 0x9C), ('¥', 0x9D), ('á', 0xA0), ('í', 0xA1),
        ('ó', 0xA2), ('ú', 0xA3), ('ñ', 0xA4), ('Ñ', 0xA5), ('ª', 0xA6), ('º', 0xA7), ('¿', 0xA8), ('¬', 0xAA),
        ('½', 0xAB), ('¼', 0xAC), ('¡', 0xAD), ('«', 0xAE), ('»', 0xAF), ('█', 0xDB), ('ß', 0xE1), ('µ', 0xE6),
        ('±', 0xF1), ('÷', 0xF6), ('°', 0xF8), ('·', 0xFA), ('²', 0xFD), ('§', 0x15), ('¶', 0x14),
    ];
    HIGH.iter().find(|(u, _)| *u == c).map(|&(_, b)| b)
}

/// Glyphs neither face has (8x16, bit 7 = left), drawn in the VGA font's style.
fn extra_glyph(c: char) -> Option<&'static [u8]> {
    static EURO: [u8; 16] = [0, 0, 0x3C, 0x66, 0xC0, 0xF8, 0xC0, 0xF8, 0xC0, 0xC0, 0x66, 0x3C, 0, 0, 0, 0];
    static CUBE: [u8; 16] = [0, 0x70, 0xD8, 0x30, 0x18, 0xD8, 0x70, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    static ACUTE: [u8; 16] = [0, 0x0C, 0x18, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    match c {
        '€' => Some(&EURO),
        '³' => Some(&CUBE),
        '´' => Some(&ACUTE),
        _ => None,
    }
}

/// `fg` over `bg` with coverage `a` (0..=255), per 8-bit channel, rounded.
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
