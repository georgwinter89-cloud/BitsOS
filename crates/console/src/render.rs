//! Drawing the text model into the display's RAM frame (`C: RamFrame`): cells blended with the font's coverage,
//! an underline cursor, scrolling by moving pixels, and the mouse pointer drawn over everything (the pixels under
//! it are kept and put back before the console draws there). Runs under `display.frame`: no allocation except
//! the pointer's saved pixels (reserved once per sprite size), no lock, no log.

use crate::font::{Font, Glyph, blend};
use crate::screen::{self, Cell, Screen};
use alloc::vec::Vec;
use display::{RamFrame, Rect};

/// Cursor height in pixels for a font (an underline of 1/8 of the cell, at least 2 pixels).
pub fn cursor_height(font: &Font) -> u32 {
    (font.h / 8).max(2).min(font.h)
}

/// The colour of one pixel of a cell (the host tests' model uses the same rule independently of `draw_cell`).
pub fn cell_pixel(font: &Font, cell: Cell, cursor: bool, x: u32, y: u32) -> u32 {
    let (fg, bg) = (screen::fg_of(cell.attr), screen::bg_of(cell.attr));
    if cursor && y >= font.h - cursor_height(font) {
        return fg;
    }
    blend(fg, bg, font.coverage(cell.ch, x, y))
}

/// One attribute's 256 blends of its foreground over its background: a pixel is then one table look-up
/// (GP-START: the blend per pixel was most of a whole screen's 9 ms). Made again only when the attribute changes.
pub struct Shade {
    attr: Option<u8>,
    lut: [u32; 256],
}

impl Shade {
    pub const fn new() -> Shade {
        Shade { attr: None, lut: [0; 256] }
    }

    fn table(&mut self, attr: u8) -> &[u32; 256] {
        if self.attr != Some(attr) {
            let (fg, bg) = (screen::fg_of(attr), screen::bg_of(attr));
            for (a, v) in self.lut.iter_mut().enumerate() {
                *v = blend(fg, bg, a as u8);
            }
            self.attr = Some(attr);
        }
        &self.lut
    }
}

impl Default for Shade {
    fn default() -> Self {
        Self::new()
    }
}

/// Draws one cell at grid position (`col`, `row`) and marks it. Builds a blend table for this one cell: for a
/// single cell (tests); every drawing path keeps a `Shade` and calls `draw_cell_shaded`.
pub fn draw_cell(f: &mut RamFrame, font: &Font, col: u32, row: u32, cell: Cell, cursor: bool) {
    draw_cell_shaded(f, font, col, row, cell, cursor, &mut Shade::new());
}

/// `draw_cell` with a table kept over many cells: the same pixels as `cell_pixel`, with the glyph looked up once
/// per cell and a pixel row written as one slice.
pub fn draw_cell_shaded(
    f: &mut RamFrame, font: &Font, col: u32, row: u32, cell: Cell, cursor: bool, shade: &mut Shade,
) {
    let (x0, y0) = (col * font.w, row * font.h);
    if x0 + font.w > f.width || y0 + font.h > f.height {
        return;
    }
    let lut = shade.table(cell.attr);
    let glyph = font.glyph(cell.ch);
    let underline = if cursor { font.h - cursor_height(font) } else { font.h };
    let (stride, w) = (f.width as usize, font.w as usize);
    let px = f.pixels_mut();
    for y in 0..font.h {
        let at = (y0 + y) as usize * stride + x0 as usize;
        let dst = &mut px[at..at + w];
        if y >= underline {
            dst.fill(lut[255]);
            continue;
        }
        match glyph {
            Glyph::Raster(r) => {
                let src = r.get(y as usize).copied().unwrap_or(&[]);
                let n = src.len().min(w);
                for (d, &a) in dst[..n].iter_mut().zip(src) {
                    *d = lut[a as usize];
                }
                dst[n..].fill(lut[0]);
            }
            g => {
                for (x, d) in dst.iter_mut().enumerate() {
                    *d = lut[g.at(font, x as u32, y) as usize];
                }
            }
        }
    }
    f.mark(Rect::new(x0, y0, font.w, font.h));
}

/// Draws screen row `r` as the view shows it (one blend table for this row; see `draw_cell`).
pub fn draw_row(f: &mut RamFrame, font: &Font, s: &Screen, r: u32) {
    draw_row_shaded(f, font, s, r, &mut Shade::new());
}

/// `draw_row` with the caller's table.
pub fn draw_row_shaded(f: &mut RamFrame, font: &Font, s: &Screen, r: u32, shade: &mut Shade) {
    let cur = s.cursor();
    let cells = s.visible_row(r);
    for (c, cell) in cells.iter().enumerate() {
        draw_cell_shaded(f, font, c as u32, r, *cell, cur == Some((c as u32, r)), shade);
    }
}

/// Brings the frame up to date with the model. `last_cursor` is where the cursor was drawn last (that cell is
/// redrawn without it).
pub fn render(f: &mut RamFrame, font: &Font, s: &mut Screen, last_cursor: &mut Option<(u32, u32)>) {
    let (mut full, scrolled, rows) = s.take_dirty();
    let mut redo: Vec<u32> = rows;
    if !full && scrolled > 0 {
        if scrolled < s.rows {
            f.scroll_up(scrolled * font.h, screen::PALETTE[screen::BG_INDEX as usize]);
            if let Some((_, r)) = last_cursor.as_mut() {
                *r = r.saturating_sub(scrolled);
            }
            // The frame's bottom margin moved up with the picture: those rows are among the dirty ones.
        } else {
            full = true;
        }
    }
    if full {
        f.fill(f.full(), screen::PALETTE[screen::BG_INDEX as usize]);
        redo = (0..s.rows).collect();
    } else if let Some((_, r)) = *last_cursor {
        redo.push(r);
    }
    let cur = s.cursor();
    if let Some((_, r)) = cur {
        redo.push(r);
    }
    redo.sort_unstable();
    redo.dedup();
    let mut shade = Shade::new();
    for r in redo {
        if r < s.rows {
            draw_row_shaded(f, font, s, r, &mut shade);
        }
    }
    *last_cursor = cur;
}

/// Arrow pointer, 12x19 font pixels: `X` outline (black), `W` fill (white).
const ARROW: [&str; 19] = [
    "X...........",
    "XX..........",
    "XWX.........",
    "XWWX........",
    "XWWWX.......",
    "XWWWWX......",
    "XWWWWWX.....",
    "XWWWWWWX....",
    "XWWWWWWWX...",
    "XWWWWWWWWX..",
    "XWWWWWWWWWX.",
    "XWWWWWWXXXXX",
    "XWWWXWWX....",
    "XWWX.XWWX...",
    "XWX..XWWX...",
    "XX....XWWX..",
    "X.....XWWX..",
    ".......XWWX.",
    ".......XXX..",
];
pub const ARROW_W: u32 = 12;
pub const ARROW_H: u32 = 19;
pub const POINTER_OUTLINE: u32 = 0x0000_0000;
pub const POINTER_FILL: u32 = 0x00FF_FFFF;

/// The sprite pixel at (x, y) of a pointer drawn at `scale` (None = transparent).
pub fn arrow_pixel(scale: u32, x: u32, y: u32) -> Option<u32> {
    let (fx, fy) = (x / scale.max(1), y / scale.max(1));
    match ARROW.get(fy as usize)?.as_bytes().get(fx as usize)? {
        b'X' => Some(POINTER_OUTLINE),
        b'W' => Some(POINTER_FILL),
        _ => None,
    }
}

/// The mouse pointer over the console. Its hot spot is the arrow's tip at (`x`, `y`) in frame pixels.
pub struct Pointer {
    pub x: u32,
    pub y: u32,
    /// Shown after the first pointer event (no pointer until a mouse is used).
    pub visible: bool,
    pub scale: u32,
    saved: Vec<u32>,
    saved_at: Option<Rect>,
}

impl Pointer {
    pub const fn new() -> Pointer {
        Pointer { x: 0, y: 0, visible: false, scale: 1, saved: Vec::new(), saved_at: None }
    }

    /// The sprite's rectangle in the frame (clipped).
    fn rect(&self, f: &RamFrame) -> Rect {
        Rect::new(self.x, self.y, ARROW_W * self.scale, ARROW_H * self.scale).clip(f.width, f.height)
    }

    /// Where the arrow is drawn in the frame now (None = not drawn).
    pub fn drawn_at(&self) -> Option<Rect> {
        self.saved_at
    }

    /// Puts back the pixels under the pointer (before anything draws there).
    pub fn hide(&mut self, f: &mut RamFrame) {
        let Some(r) = self.saved_at.take() else { return };
        let stride = f.width as usize;
        let px = f.pixels_mut();
        for y in 0..r.h as usize {
            let at = (r.y as usize + y) * stride + r.x as usize;
            let from = y * r.w as usize;
            if let (Some(dst), Some(src)) = (px.get_mut(at..at + r.w as usize), self.saved.get(from..from + r.w as usize)) {
                dst.copy_from_slice(src);
            }
        }
        f.mark(r);
    }

    /// Keeps the pixels under the pointer and draws it (after the console drew).
    pub fn show(&mut self, f: &mut RamFrame) {
        if !self.visible || self.saved_at.is_some() {
            return;
        }
        let r = self.rect(f);
        if r.is_empty() {
            return;
        }
        let n = (r.w * r.h) as usize;
        self.saved.clear();
        // Reserved once for the sprite size; a failed reservation shows no pointer rather than allocating later.
        if self.saved.try_reserve(n).is_err() {
            return;
        }
        let stride = f.width as usize;
        let px = f.pixels_mut();
        for y in 0..r.h {
            let at = (r.y + y) as usize * stride + r.x as usize;
            self.saved.extend_from_slice(&px[at..at + r.w as usize]);
            for x in 0..r.w {
                if let Some(c) = arrow_pixel(self.scale, x, y) {
                    px[at + x as usize] = c;
                }
            }
        }
        self.saved_at = Some(r);
        f.mark(r);
    }
}

impl Default for Pointer {
    fn default() -> Self {
        Self::new()
    }
}
