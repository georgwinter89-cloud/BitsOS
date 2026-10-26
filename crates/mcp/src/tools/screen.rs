//! `screen` (MC-3, V2-PLAN §6, `spec/net.md` §5.2): the picture as a PNG from the display's RAM frame (never the
//! scanout, which is slow to read: V1 needed 0.4 s per 1440p shot), optionally scaled down and/or cut to a
//! region; or, with `format: "text"`, the kernel console's text grid.
//!
//! The frame is copied under the display's lock (one memcpy of the region) and encoded outside it, so drawing
//! waits for the copy only, never for the encoder.

use super::{bad, number, opt_str};
use crate::png::{self, PngError};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool, rpc};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use display::font;

/// Largest scale-down factor (a bound on one request's argument, not a count of things).
pub const SCALE_MAX: u64 = 16;
/// Bytes left for the PNG in an answer: base64 makes 4 of every 3 bytes; the rest is the text and JSON around it.
pub const PNG_LIMIT: usize = (rpc::ANSWER_MAX - 8 * 1024) / 4 * 3;

/// The part of the frame a call asked for, in frame pixels, and the scale-down factor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shot {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub scale: u32,
}

impl Shot {
    /// Size of the picture sent (a partial block at the right/bottom edge becomes one pixel).
    pub fn out_size(&self) -> (u32, u32) {
        (self.w.div_ceil(self.scale), self.h.div_ceil(self.scale))
    }
}

/// Reads `x`, `y`, `width`, `height`, `scale` against a `fw` x `fh` frame (pure, host-tested).
pub fn shot_args(args: &Json, fw: u32, fh: u32) -> Result<Shot, ToolError> {
    let scale = number(args, "scale")?.unwrap_or(1);
    if !(1..=SCALE_MAX).contains(&scale) {
        return Err(bad("scale"));
    }
    let x = number(args, "x")?.unwrap_or(0);
    let y = number(args, "y")?.unwrap_or(0);
    if x >= fw as u64 {
        return Err(ToolError::BadArgument(format!("x: {x} is outside the frame (width {fw})")));
    }
    if y >= fh as u64 {
        return Err(ToolError::BadArgument(format!("y: {y} is outside the frame (height {fh})")));
    }
    let w = number(args, "width")?.unwrap_or(fw as u64 - x);
    let h = number(args, "height")?.unwrap_or(fh as u64 - y);
    if w == 0 || x + w > fw as u64 {
        return Err(ToolError::BadArgument(format!("width: x {x} + width {w} must be 1 to {fw}")));
    }
    if h == 0 || y + h > fh as u64 {
        return Err(ToolError::BadArgument(format!("height: y {y} + height {h} must be 1 to {fh}")));
    }
    Ok(Shot { x: x as u32, y: y as u32, w: w as u32, h: h as u32, scale: scale as u32 })
}

/// Copies the shot's region of the frame (`pixels` is `fw` wide) into a new buffer, `w * h` pixels.
pub fn copy_region(pixels: &[u32], fw: u32, s: &Shot, dst: &mut Vec<u32>) {
    for row in s.y..s.y + s.h {
        let at = (row * fw + s.x) as usize;
        dst.extend_from_slice(&pixels[at..at + s.w as usize]);
    }
}

/// Row `oy` of the scaled picture as RGB: each output pixel is the average of its `scale` x `scale` block
/// (fewer at the right/bottom edge). `region` is `w` x `h`.
pub fn scaled_row(region: &[u32], s: &Shot, oy: u32, out: &mut [u8]) {
    if s.scale == 1 {
        let at = (oy * s.w) as usize;
        png::xrgb_to_rgb(&region[at..at + s.w as usize], out);
        return;
    }
    let (ow, _) = s.out_size();
    let y0 = oy * s.scale;
    let y1 = (y0 + s.scale).min(s.h);
    for ox in 0..ow {
        let x0 = ox * s.scale;
        let x1 = (x0 + s.scale).min(s.w);
        let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
        for y in y0..y1 {
            for &p in &region[(y * s.w + x0) as usize..(y * s.w + x1) as usize] {
                r += (p >> 16) & 0xFF;
                g += (p >> 8) & 0xFF;
                b += p & 0xFF;
            }
        }
        let n = (y1 - y0) * (x1 - x0);
        let o = ox as usize * 3;
        out[o] = ((r + n / 2) / n) as u8;
        out[o + 1] = ((g + n / 2) / n) as u8;
        out[o + 2] = ((b + n / 2) / n) as u8;
    }
}

/// Encodes a copied region as PNG (pure, host-tested together with `png`).
pub fn encode(region: &[u32], s: &Shot, limit: usize) -> Result<Vec<u8>, PngError> {
    let (ow, oh) = s.out_size();
    png::encode_rgb(ow, oh, limit, |oy, buf| scaled_row(region, s, oy, buf))
}

/// What the PNG answer says about itself (the "size/time" record of MC-3).
pub struct Stats {
    pub frame: (u32, u32),
    pub copy_ms: u64,
    pub encode_ms: u64,
    pub bytes: usize,
}

/// Takes the shot from the live RAM frame. Err: the tool's error (no display, too big, no memory).
pub fn take(args: &Json, now_ns: &dyn Fn() -> u64) -> Result<(Shot, Vec<u8>, Stats), ToolError> {
    let (fw, fh) = display::with_frame(|f| (f.width, f.height)).ok_or_else(no_display)?;
    let s = shot_args(args, fw, fh)?;
    let t0 = now_ns();
    let mut region: Vec<u32> = Vec::new();
    if region.try_reserve_exact(s.w as usize * s.h as usize).is_err() {
        klog::event!(Warn, "mcp", "screen.no_memory", width = s.w, height = s.h; "no memory for the region copy");
        return Err(ToolError::Failed(format!("no memory for a copy of {}x{} pixels; ask again with a smaller region", s.w, s.h)));
    }
    // DP-2: the picture the screen shows (a fullscreen fbdev program's while it has the screen), not only the
    // console's frame.
    let same = display::with_screen(|px, w, h| {
        // The frame can only change size through set_device; then the arguments no longer fit.
        if (w, h) != (fw, fh) {
            return false;
        }
        copy_region(px, fw, &s, &mut region);
        true
    });
    if same != Some(true) {
        klog::event!(Warn, "mcp", "screen.display_changed", width = fw, height = fh; "the frame changed size or went away during the copy");
        return Err(ToolError::Failed(String::from("the display changed while the picture was taken; ask again")));
    }
    let t1 = now_ns();
    let png = encode(&region, &s, PNG_LIMIT).map_err(|e| too_big(e, &s))?;
    let t2 = now_ns();
    let st = Stats { frame: (fw, fh), copy_ms: (t1 - t0) / 1_000_000, encode_ms: (t2 - t1) / 1_000_000, bytes: png.len() };
    Ok((s, png, st))
}

fn no_display() -> ToolError {
    ToolError::Failed(String::from("no display: the kernel found no picture to show (no GOP framebuffer)"))
}

fn too_big(e: PngError, s: &Shot) -> ToolError {
    let (ow, oh) = s.out_size();
    match e {
        PngError::TooBig { rows, bytes } => {
            klog::event!(Info, "mcp", "screen.too_big", width = ow, height = oh, rows = rows, bytes = bytes, limit = PNG_LIMIT);
            ToolError::Refused(format!(
                "the PNG of {ow}x{oh} grows past {PNG_LIMIT} bytes ({bytes} bytes after {rows} of {oh} rows), which does not fit into the 4 MB MCP answer; ask again with scale 2 (or more) or a smaller region (x, y, width, height)"
            ))
        }
        PngError::BadSize => ToolError::Failed(format!("cannot encode a {ow}x{oh} picture")),
        PngError::NoMemory => {
            klog::event!(Warn, "mcp", "screen.no_memory", width = ow, height = oh; "no memory for the PNG encoder or its output");
            ToolError::Failed(format!("no memory to encode a {ow}x{oh} PNG; ask again with a scale or a smaller region"))
        }
    }
}

fn screen(ctx: &ToolContext, args: &Json) -> ToolResult {
    let fmt = opt_str(args, "format")?.unwrap_or("png");
    match fmt {
        "png" => {}
        "text" => {
            if ["scale", "x", "y", "width", "height"].iter().any(|k| args.get(k).is_some_and(|v| !v.is_null())) {
                return Err(ToolError::BadArgument(String::from("format: scale and region apply to png only")));
            }
            return text_grid().map(|t| alloc::vec![ToolOutput::Text(t)]);
        }
        _ => return Err(bad("format")),
    }
    let (s, png, st) = take(args, &|| ctx.now_ns())?;
    let (ow, oh) = s.out_size();
    klog::event!(Debug, "mcp", "screen.png", width = ow, height = oh, bytes = st.bytes, copy_ms = st.copy_ms, encode_ms = st.encode_ms);
    let info = format!(
        "screen png {ow}x{oh} (frame {}x{}, region {},{} {}x{}, scale {}): {} bytes, copy {} ms, encode {} ms",
        st.frame.0, st.frame.1, s.x, s.y, s.w, s.h, s.scale, st.bytes, st.copy_ms, st.encode_ms
    );
    Ok(alloc::vec![ToolOutput::Text(info), ToolOutput::Png(png)])
}

mcp_tool!(
    name: "screen",
    owner: "MC",
    safety: Read,
    description: "The picture on the screen. PNG (default) of the composed RAM frame at its real size (2560x1440 on the Mini-PC), or scaled down (scale n: each pixel the average of n x n) and/or cut to a region (x, y, width, height in frame pixels). format text: the kernel console's text grid instead (rows as drawn; a cell that is no font glyph shows as U+FFFD). The first text item names size, bytes and the time taken. A PNG that would not fit into the 4 MB answer is refused with a hint to scale or cut.",
    schema: r#"{"type":"object","properties":{"format":{"type":"string","enum":["png","text"],"description":"png (default) or text"},"scale":{"type":"integer","minimum":1,"maximum":16,"description":"scale down by n (default 1)"},"x":{"type":"integer","minimum":0,"description":"region left edge (default 0)"},"y":{"type":"integer","minimum":0,"description":"region top edge (default 0)"},"width":{"type":"integer","minimum":1,"description":"region width (default: to the right edge)"},"height":{"type":"integer","minimum":1,"description":"region height (default: to the bottom edge)"}}}"#,
    handler: screen,
);

// ---- text mode: the console grid read back from the frame ----

/// Glyph bitmaps (16 rows, bit 7 = left) -> character, for the characters the console draws (`font::cp437`).
/// Printable ASCII first, so a glyph two characters share (none today) reads as the ASCII one.
pub fn glyph_table() -> BTreeMap<[u8; 16], char> {
    let mut t = BTreeMap::new();
    let extra = ['Ä', 'Ö', 'Ü', 'ä', 'ö', 'ü', 'ß', '°', '§', '█', '·'];
    for c in (' '..='~').chain(extra) {
        let mut g = [0u8; 16];
        g.copy_from_slice(font::glyph(font::cp437(c)));
        t.entry(g).or_insert(c);
    }
    t
}

/// One console cell as sampled from the frame: 16 rows of 8 ink bits (bit 7 = left), the font's glyph format.
pub type Cell = [u8; 16];

/// Samples the `cols` x `rows` cells (8x16 glyphs drawn at `scale`) out of `pixels` (`fw` wide) into `out`: a
/// pixel is ink when it is not the console background. One read per font pixel, no allocation as long as `out`
/// has room for `cols * rows` cells, so it can run under the frame lock (it is the only part that does).
pub fn sample_cells(pixels: &[u32], fw: u32, cols: u32, rows: u32, scale: u32, bg: u32, out: &mut Vec<Cell>) {
    for r in 0..rows {
        for c in 0..cols {
            let mut g = [0u8; 16];
            for (gy, bits) in g.iter_mut().enumerate() {
                let y = (r * font::H + gy as u32) * scale;
                for gx in 0..font::W {
                    let x = (c * font::W + gx) * scale;
                    if pixels.get((y * fw + x) as usize).is_some_and(|&p| p != bg) {
                        *bits |= 0x80 >> gx;
                    }
                }
            }
            out.push(g);
        }
    }
}

/// Sampled cells (`cols` per row) to text lines, trailing blanks and empty last rows trimmed; a cell that is no
/// glyph is U+FFFD. Runs outside the frame lock (the glyph table and the strings allocate).
pub fn cells_to_lines(cells: &[Cell], cols: u32) -> Vec<String> {
    let table = glyph_table();
    let mut lines: Vec<String> = cells
        .chunks(cols.max(1) as usize)
        .map(|row| {
            let line: String = row.iter().map(|g| table.get(g).copied().unwrap_or('\u{FFFD}')).collect();
            String::from(line.trim_end())
        })
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// `sample_cells` + `cells_to_lines` on a whole frame (host tests).
pub fn read_cells(pixels: &[u32], fw: u32, cols: u32, rows: u32, scale: u32, bg: u32) -> Vec<String> {
    let mut cells = Vec::with_capacity((cols * rows) as usize);
    sample_cells(pixels, fw, cols, rows, scale, bg, &mut cells);
    cells_to_lines(&cells, cols)
}

/// The console's text grid: IN-3's VT model (`console::text_grid`, IN-3 change request 2 applied by INT-A1: every
/// character as typed, € µ ² ³ included, whatever the font), else read back from the RAM frame with the 8x16 font.
fn text_grid() -> Result<String, ToolError> {
    if let Some((cols, rows, lines, (cc, cr))) = console::text_grid() {
        let mut lines: Vec<String> = lines.into_iter().map(|l| String::from(l.trim_end())).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let face = console::vt::font().map_or("?", |f| f.face.name());
        return Ok(format!("console {cols}x{rows} cells ({face} font), cursor at column {cc} row {cr}, {} rows with text:\n{}", lines.len(), lines.join("\n")));
    }
    text_grid_from_pixels()
}

/// The text grid read back from the RAM frame (VGA 8x16 glyphs only).
fn text_grid_from_pixels() -> Result<String, ToolError> {
    use console::Console;
    let (cols, rows) = console::console().size();
    if cols == 0 || rows == 0 {
        return Err(ToolError::Failed(String::from("the console has not drawn anything yet (no display, or no line written)")));
    }
    let (fw, fh) = display::with_frame(|f| (f.width, f.height)).ok_or_else(no_display)?;
    let scale = fw / (cols * font::W);
    if scale == 0 || rows * font::H * scale > fh {
        return Err(ToolError::Failed(format!("the console grid {cols}x{rows} does not fit the {fw}x{fh} frame")));
    }
    let mut cells: Vec<Cell> = Vec::new();
    if cells.try_reserve_exact(cols as usize * rows as usize).is_err() {
        klog::event!(Warn, "mcp", "screen.no_memory", cols = cols, rows = rows; "no memory for the console cells");
        return Err(ToolError::Failed(format!("no memory for {cols}x{rows} console cells")));
    }
    // Under the frame lock only the sampling (16 bytes per cell, no allocation, as short as the PNG path's copy);
    // matching glyphs and building strings happen after the lock is released.
    let same = display::with_frame(|f| {
        if (f.width, f.height) != (fw, fh) {
            return false;
        }
        sample_cells(f.pixels(), fw, cols, rows, scale, console::BG, &mut cells);
        true
    });
    if same != Some(true) {
        klog::event!(Warn, "mcp", "screen.display_changed", width = fw, height = fh; "the frame changed size or went away while the cells were read");
        return Err(ToolError::Failed(String::from("the display changed while the text was read; ask again")));
    }
    let lines = cells_to_lines(&cells, cols);
    Ok(format!("console {cols}x{rows} cells (8x16 font at scale {scale}), {} rows with text:\n{}", lines.len(), lines.join("\n")))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;

    fn args(s: &str) -> Json {
        Json::parse(s).unwrap()
    }

    #[test]
    fn region_and_scale_arguments() {
        assert_eq!(shot_args(&args("{}"), 2560, 1440), Ok(Shot { x: 0, y: 0, w: 2560, h: 1440, scale: 1 }));
        let s = shot_args(&args(r#"{"x":100,"y":"0x10","width":7,"scale":3}"#), 2560, 1440).unwrap();
        assert_eq!(s, Shot { x: 100, y: 16, w: 7, h: 1424, scale: 3 });
        assert_eq!(s.out_size(), (3, 475));
        for bad_args in [
            r#"{"scale":0}"#, r#"{"scale":17}"#, r#"{"scale":-1}"#, r#"{"x":2560}"#, r#"{"y":1440}"#, r#"{"x":2000,"width":561}"#,
            r#"{"width":0}"#, r#"{"height":0}"#, r#"{"y":1,"height":1440}"#, r#"{"x":"zz"}"#, r#"{"width":18446744073709551615}"#,
        ] {
            assert!(matches!(shot_args(&args(bad_args), 2560, 1440), Err(ToolError::BadArgument(_))), "{bad_args}");
        }
    }

    #[test]
    fn region_scale_and_png_agree_with_the_frame() {
        let (fw, fh) = (37u32, 23u32);
        let px: Vec<u32> = (0..fw * fh).map(|i| i.wrapping_mul(2_654_435_761) & 0x00FF_FFFF).collect();
        let s = shot_args(&args(r#"{"x":3,"y":4,"width":30,"height":17,"scale":4}"#), fw, fh).unwrap();
        let mut region = Vec::new();
        copy_region(&px, fw, &s, &mut region);
        assert_eq!(region.len(), 30 * 17);
        let png = encode(&region, &s, usize::MAX).unwrap();
        let (w, h, rgb) = crate::png::tests::decode(&png);
        assert_eq!((w, h), (8, 5));
        // The last column averages a 2-wide block, the last row a 1-high block.
        for (oy, ox) in [(0u32, 0u32), (4, 7), (2, 3)] {
            let (mut sum, mut n) = ([0u32; 3], 0);
            for y in (oy * 4)..((oy * 4 + 4).min(17)) {
                for x in (ox * 4)..((ox * 4 + 4).min(30)) {
                    let p = px[((y + 4) * fw + x + 3) as usize];
                    sum[0] += (p >> 16) & 0xFF;
                    sum[1] += (p >> 8) & 0xFF;
                    sum[2] += p & 0xFF;
                    n += 1;
                }
            }
            let at = ((oy * w + ox) * 3) as usize;
            for k in 0..3 {
                assert_eq!(rgb[at + k] as u32, (sum[k] + n / 2) / n, "pixel {ox},{oy} channel {k}");
            }
        }
    }

    #[test]
    fn console_cells_read_back() {
        let (cols, rows, scale) = (12u32, 3u32, 2u32);
        let mut f = display::RamFrame::new(cols * 8 * scale + 5, rows * 16 * scale + 3);
        let fw = f.width;
        let bg = 0x0010_1418;
        f.fill(display::Rect { x: 0, y: 0, w: f.width, h: f.height }, bg);
        for (r, text) in ["Hallo {x}", "", "äöü ß°§ ~"].iter().enumerate() {
            for (c, ch) in text.chars().enumerate() {
                font::draw(&mut f, c as u32 * 8 * scale, r as u32 * 16 * scale, ch, scale, if r == 0 { 0x00C8_C8C8 } else { 0x00FF_5050 }, bg);
            }
        }
        // A cell that is not a glyph (a picture under the console).
        f.fill(display::Rect { x: 11 * 16, y: 0, w: 3, h: 3 }, 0x00FF_FFFF);
        let lines = read_cells(f.pixels(), fw, cols, rows, scale, bg);
        assert_eq!(lines, vec![String::from("Hallo {x}  \u{FFFD}"), String::new(), String::from("äöü ß°§ ~")]);
    }

    #[test]
    fn screen_without_a_display() {
        let a = args(r#"{"format":"text"}"#);
        let ctx = ToolContext::detached(u64::MAX);
        assert!(matches!(screen(&ctx, &a), Err(ToolError::Failed(_))));
        assert!(matches!(screen(&ctx, &args("{}")), Err(ToolError::Failed(_))));
        assert_eq!(screen(&ctx, &args(r#"{"format":"jpeg"}"#)).err(), Some(bad("format")));
        assert!(matches!(screen(&ctx, &args(r#"{"format":"text","scale":2}"#)), Err(ToolError::BadArgument(_))));
    }

    /// A full 1440p frame of noise (the worst picture) at scale 1 is refused readably and early, not answered past
    /// 4 MB; scale 4 of the same frame is answered and exact. Generous time bound (debug host build).
    #[test]
    fn noise_frame_is_refused_at_scale_1_and_answered_scaled() {
        let (fw, fh) = (2560u32, 1440u32);
        let mut seed = 0x0BAD_5EEDu32;
        let px: Vec<u32> = (0..fw * fh).map(|_| { seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); seed >> 8 }).collect();
        let t = std::time::Instant::now();
        let s = shot_args(&args("{}"), fw, fh).unwrap();
        match encode(&px, &s, PNG_LIMIT).map_err(|e| too_big(e, &s)) {
            Err(ToolError::Refused(m)) => assert!(m.contains("4 MB") && m.contains("scale 2"), "{m}"),
            other => panic!("scale 1 of noise: {:?}", other.map(|p| p.len())),
        }
        let s = shot_args(&args(r#"{"scale":4}"#), fw, fh).unwrap();
        let png = encode(&px, &s, PNG_LIMIT).map_err(|e| too_big(e, &s)).unwrap();
        assert!(png.len() <= PNG_LIMIT);
        let (w, h, rgb) = crate::png::tests::decode(&png);
        assert_eq!((w, h), (640, 360));
        let mut want = vec![0u8; 640 * 3];
        scaled_row(&px, &s, 359, &mut want);
        assert_eq!(&rgb[359 * 640 * 3..], &want[..]);
        assert!(t.elapsed() < std::time::Duration::from_secs(60), "{:?}", t.elapsed());
    }

    #[test]
    fn png_limit_fits_the_answer() {
        assert!(crate::b64::encoded_len(PNG_LIMIT) + 8 * 1024 <= rpc::ANSWER_MAX);
        const { assert!(PNG_LIMIT > 3_000_000) };
    }
}
