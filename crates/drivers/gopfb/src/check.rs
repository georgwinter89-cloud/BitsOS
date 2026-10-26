//! Damage-copy checks against a RAM target, shared by the host tests and the ktests (`dp.gopfb.*`): every
//! layout lands byte-exact at the right place, stride padding and the bytes after the frame stay untouched,
//! only damaged rectangles are copied (an unmarked change between two damaged rectangles stays off screen,
//! and the byte count equals the damaged area), and bad rectangles are refused without a write.

use crate::{GopFb, Layout, RamTarget};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use devmodel::display::DisplayDevice;
use display::{RamFrame, Rect};

const SENTINEL: u8 = 0xA5;

/// A pixel value with all three channels varying over the frame.
pub fn pattern(x: u32, y: u32) -> u32 {
    let r = (x.wrapping_mul(31) ^ y.wrapping_mul(17)) & 0xFF;
    let g = (x.wrapping_mul(3).wrapping_add(y.wrapping_mul(5)) ^ 0x5A) & 0xFF;
    let b = (x ^ y.wrapping_mul(7)).wrapping_add(x >> 3) & 0xFF;
    (r << 16) | (g << 8) | b
}

fn expect_pixel(t: &RamTarget, l: Layout, stride: u32, x: u32, y: u32, p: u32, what: &str) -> Result<(), String> {
    let bpp = l.bytes_per_pixel();
    let off = (y as usize * stride as usize + x as usize) * bpp;
    let mut got = [0u8; 4];
    if !t.read(off, &mut got[..bpp]) {
        return Err(format!("{what}: ({x},{y}) outside the target"));
    }
    let want = l.encode(p).to_le_bytes();
    if got[..bpp] != want[..bpp] {
        return Err(format!("{what}: {l:?} pixel ({x},{y}) is {:02x?}, want {:02x?} (frame {p:#08x})", &got[..bpp], &want[..bpp]));
    }
    Ok(())
}

/// The whole frame as presented, plus the untouched padding and tail.
fn expect_frame(t: &RamTarget, l: Layout, f: &RamFrame, stride: u32, what: &str) -> Result<(), String> {
    let bpp = l.bytes_per_pixel();
    let bytes = t.to_vec();
    let (w, h) = (f.width as usize, f.height as usize);
    for y in 0..h {
        for x in 0..w {
            let p = f.pixels()[y * w + x];
            let off = (y * stride as usize + x) * bpp;
            let want = l.encode(p).to_le_bytes();
            if bytes[off..off + bpp] != want[..bpp] {
                return Err(format!("{what}: {l:?} pixel ({x},{y}) is {:02x?}, want {:02x?}", &bytes[off..off + bpp], &want[..bpp]));
            }
        }
        let pad = (y * stride as usize + w) * bpp..((y + 1) * stride as usize) * bpp;
        if bytes.get(pad.clone()).is_some_and(|s| s.iter().any(|&b| b != SENTINEL)) {
            return Err(format!("{what}: {l:?} stride padding of row {y} written"));
        }
    }
    let end = ((h - 1) * stride as usize + w) * bpp;
    if bytes[end..].iter().any(|&b| b != SENTINEL) {
        return Err(format!("{what}: {l:?} bytes after the frame written"));
    }
    Ok(())
}

/// Full present, then two far-apart damaged rectangles with an unmarked change between them.
pub fn layout_roundtrip(l: Layout, w: u32, h: u32, stride: u32) -> Result<(), String> {
    let bpp = l.bytes_per_pixel();
    let t = Arc::new(RamTarget::new(stride as usize * h as usize * bpp + 64, SENTINEL));
    let dev = GopFb::new(t.clone(), w, h, stride, l, "ram").map_err(|e| format!("{l:?} {w}x{h}/{stride}: {e:?}"))?;
    let mut f = RamFrame::new(w, h);
    for y in 0..h {
        for x in 0..w {
            f.pixels_mut()[(y * w + x) as usize] = pattern(x, y);
        }
    }
    f.mark_all();
    let st = display::present_frame(&dev, &mut f);
    if st.errors != 0 || st.pixels != w as u64 * h as u64 {
        return Err(format!("{l:?}: full present {st:?}"));
    }
    expect_frame(&t, l, &f, stride, "full present")?;

    let a = Rect::new(1, 1, (w / 5).max(1), (h / 5).max(1));
    let b = Rect::new(w - (w / 6).max(1) - 1, h - (h / 6).max(1) - 1, (w / 6).max(1), (h / 6).max(1));
    let (hx, hy) = (w / 2, h / 2);
    let hidden_old = f.pixels()[(hy * w + hx) as usize];
    f.fill(a, 0x00FF_8000);
    f.fill(b, 0x0000_80FF);
    f.set(hx, hy, 0x00FF_FFFF);
    if f.damage_rects().len() != 2 {
        return Err(format!("{l:?}: two far rectangles became {:?}", f.damage_rects()));
    }
    let before = t.written();
    display::present_frame(&dev, &mut f);
    let copied = t.written() - before;
    let want = (a.area() + b.area()) * bpp as u64;
    if copied != want {
        return Err(format!("{l:?}: damage present wrote {copied} bytes, the damage is {want}"));
    }
    expect_pixel(&t, l, stride, a.x, a.y, 0x00FF_8000, "damage a")?;
    expect_pixel(&t, l, stride, b.x + b.w - 1, b.y + b.h - 1, 0x0000_80FF, "damage b")?;
    expect_pixel(&t, l, stride, hx, hy, hidden_old, "unmarked pixel between the rectangles")?;
    // Marked now: everything equals the frame again.
    f.mark(Rect::new(hx, hy, 1, 1));
    display::present_frame(&dev, &mut f);
    expect_frame(&t, l, &f, stride, "after marking")
}

/// Rectangles outside the screen or the source are refused and write nothing.
pub fn bad_damage_refused() -> Result<(), String> {
    let t = Arc::new(RamTarget::new(16 * 8 * 4, SENTINEL));
    let dev = GopFb::new(t.clone(), 16, 8, 16, Layout::Bgrx, "ram").map_err(|e| format!("{e:?}"))?;
    let src = alloc::vec![0x0012_3456u32; 16 * 8];
    for d in [(0, 0, 17, 1), (15, 0, 2, 1), (0, 8, 1, 1), (0, 7, 1, 2), (u32::MAX, 0, 2, 1), (0, u32::MAX, 1, 2)] {
        if dev.present(&src, 16, d).is_ok() {
            return Err(format!("damage {d:?} accepted"));
        }
    }
    if dev.present(&src[..16 * 7], 16, (0, 6, 16, 2)).is_ok() || dev.present(&src, 8, (4, 0, 8, 1)).is_ok() {
        return Err("a source too short or too narrow was accepted".into());
    }
    if t.written() != 0 {
        return Err(format!("refused presents wrote {} bytes", t.written()));
    }
    if dev.present(&src, 16, (3, 3, 0, 5)).is_err() || t.written() != 0 {
        return Err("an empty rectangle must succeed and write nothing".into());
    }
    Ok(())
}

/// Geometries that do not fit their target are refused.
pub fn geometry_refused() -> Result<(), String> {
    let t: Arc<dyn crate::FbTarget> = Arc::new(RamTarget::new(100 * 10 * 4, 0));
    let ok = |w, h, s, l| GopFb::new(t.clone(), w, h, s, l, "ram").is_ok();
    let cases = [
        (100, 10, 100, Layout::Bgrx, true),
        (100, 11, 100, Layout::Bgrx, false),
        (100, 10, 99, Layout::Bgrx, false),
        (0, 10, 100, Layout::Bgrx, false),
        (100, 0, 100, Layout::Bgrx, false),
        (90, 10, 100, Layout::Bgrx, true),
        (100, 10, u32::MAX, Layout::Bgrx, false),
        (u32::MAX, u32::MAX, u32::MAX, Layout::Bgrx, false),
    ];
    for (w, h, s, l, want) in cases {
        if ok(w, h, s, l) != want {
            return Err(format!("{w}x{h} stride {s}: accepted={}, want {want}", !want));
        }
    }
    Ok(())
}

/// The layouts every check runs: the two byte orders, a channel permutation, 10-bit, 24-bit and 16-bit masks.
pub fn layouts() -> [Layout; 6] {
    let m = |v| Layout::from_masks(v).unwrap_or(Layout::Bgrx);
    [
        Layout::Bgrx,
        Layout::Rgbx,
        m([0xFF, 0xFF_0000, 0xFF00, 0xFF00_0000]),
        m([0x3FF0_0000, 0xF_FC00, 0x3FF, 0xC000_0000]),
        m([0xFF_0000, 0xFF00, 0xFF, 0]),
        m([0xF800, 0x07E0, 0x001F, 0]),
    ]
}

/// Every layout at `w` x `h`, once with stride == width and once with padding.
pub fn all_layouts(w: u32, h: u32) -> Result<(), String> {
    for l in layouts() {
        layout_roundtrip(l, w, h, w)?;
        layout_roundtrip(l, w, h, w + 13)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_land_exactly() {
        all_layouts(37, 23).unwrap();
        all_layouts(130, 7).unwrap();
    }
    #[test]
    fn bad_damage() {
        bad_damage_refused().unwrap();
    }
    #[test]
    fn geometry() {
        geometry_refused().unwrap();
    }
    #[test]
    fn mask_layouts_are_distinct() {
        let l = layouts();
        assert!(matches!(l[2], Layout::Mask { bytes: 4, .. }));
        assert_eq!(l[4].bytes_per_pixel(), 3);
        assert_eq!(l[5].bytes_per_pixel(), 2);
    }
}
