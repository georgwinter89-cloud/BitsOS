//! GP-START: `draw_cell` (glyph looked up once per cell) gives exactly `cell_pixel`'s colours (the lookup per pixel),
//! for every character the console can be asked to show, both faces at every size, with and without the cursor.

extern crate std;

use crate::font::{Face, Font};
use crate::render;
use crate::screen::{self, Cell};
use display::RamFrame;

#[test]
fn draw_cell_equals_the_per_pixel_rule() {
    let chars = (' '..='\u{17f}').chain(['€', '³', '´', '█', '\u{2603}', '\u{23fa}', '\u{276f}']);
    for face in [Face::Vga, Face::Noto] {
        for (w, h) in [(2560, 1440), (1920, 1080), (1024, 768)] {
            let font = Font::for_mode(face, w, h);
            let mut f = RamFrame::new(font.w * 2, font.h);
            for ch in chars.clone() {
                for (attr, cursor) in [(screen::DEFAULT_ATTR, false), (screen::attr(3, 4), true)] {
                    let cell = Cell { ch, attr };
                    render::draw_cell(&mut f, &font, 1, 0, cell, cursor);
                    for y in 0..font.h {
                        for x in 0..font.w {
                            let got = f.pixels()[(y * f.width + font.w + x) as usize];
                            let want = render::cell_pixel(&font, cell, cursor, x, y);
                            assert_eq!(got, want, "{face:?} {w}x{h} {ch:?} cursor={cursor} pixel ({x}, {y})");
                        }
                    }
                }
            }
        }
    }
}
