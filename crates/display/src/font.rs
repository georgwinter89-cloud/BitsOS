//! Bitmap fonts for the kernel console (G0: CP437 8x16, public domain; see font/README.md).

pub const W: u32 = 8;
pub const H: u32 = 16;
static CP437_8X16: &[u8; 4096] = include_bytes!("font/cp437-8x16.bin");

/// Maps a Unicode character to the CP437 glyph (German umlauts and common symbols; others become '?').
pub fn cp437(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        'Ä' => 0x8E,
        'Ö' => 0x99,
        'Ü' => 0x9A,
        'ä' => 0x84,
        'ö' => 0x94,
        'ü' => 0x81,
        'ß' => 0xE1,
        '°' => 0xF8,
        '§' => 0x15,
        '€' => b'E',
        '█' => 0xDB,
        '·' => 0xFA,
        _ => b'?',
    }
}

/// The 16 rows of a glyph, bit 7 = leftmost pixel.
pub fn glyph(code: u8) -> &'static [u8] {
    let i = code as usize * 16;
    &CP437_8X16[i..i + 16]
}

/// Draws `c` at pixel (x, y) scaled by `scale` into the frame.
pub fn draw(frame: &mut crate::RamFrame, x: u32, y: u32, c: char, scale: u32, fg: u32, bg: u32) {
    let g = glyph(cp437(c));
    for (row, bits) in g.iter().enumerate() {
        for col in 0..8u32 {
            let on = bits & (0x80 >> col) != 0;
            let color = if on { fg } else { bg };
            for sy in 0..scale {
                for sx in 0..scale {
                    frame.set(x + col * scale + sx, y + row as u32 * scale + sy, color);
                }
            }
        }
    }
    frame.mark(crate::Rect { x, y, w: 8 * scale, h: 16 * scale });
}
