//! WY-15: the GLES renderer's picture is read back from GL into a CPU-side XRGB8888 buffer, which the back ends copy
//! to `/dev/fb0` exactly as Pixman's shadow image. The arithmetic of that copy lives here, without GL, so it is
//! tested on every build. Rows: Smithay's projection for a texture target (its flip180, gles/mod.rs:2088) puts
//! the picture's top row into the texture's row 0, so a glReadPixels rectangle at picture coordinates comes back
//! top row first, as the picture (wy/98 compares the read-back picture with Pixman's). Bytes: B G R A when GL can
//! read BGRA, else R G B A (then red and blue are swapped).

/// A damaged rectangle `(x, y, w, h)` in picture coordinates (y down), cut to the `pic_w` x `pic_h` picture;
/// None when nothing of it is inside.
pub fn clamp(rect: (i32, i32, i32, i32), pic_w: i32, pic_h: i32) -> Option<(i32, i32, i32, i32)> {
    let (x, y, w, h) = rect;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = x.saturating_add(w).min(pic_w);
    let y1 = y.saturating_add(h).min(pic_h);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// Whether the next frame reads the whole picture back, not only its damage: true at first and after a frame whose
/// readback did not finish (the texture then holds more than the copy, and the damage tracker will not draw that
/// part again). `begin` at the start of a frame answers it and assumes the frame fails; `done` after the last
/// rectangle was copied.
#[derive(Debug)]
pub struct Full(bool);

impl Full {
    pub fn new() -> Self {
        Full(true)
    }

    pub fn begin(&mut self) -> bool {
        std::mem::replace(&mut self.0, true)
    }

    pub fn done(&mut self) {
        self.0 = false;
    }
}

/// The rectangles to read back for one frame and the damage to report: the tracker's damage cut to the picture,
/// or the whole picture when `full`. Rectangles wholly outside are dropped (nothing to read); the reported damage
/// is what was read, so the fb0 copy never takes a rectangle outside the picture.
pub fn plan(damage: &[(i32, i32, i32, i32)], full: bool, pic_w: i32, pic_h: i32) -> Vec<(i32, i32, i32, i32)> {
    if full {
        return clamp((0, 0, pic_w, pic_h), pic_w, pic_h).into_iter().collect();
    }
    damage.iter().filter_map(|r| clamp(*r, pic_w, pic_h)).collect()
}

/// One readback in `order`; when the GL refuses BGRA (`refused` says so for its error), once more in RGBA, which
/// every GLES can read, and `order` stays RGBA for the frames after.
pub fn read_with_fallback<T, E>(
    order: &mut Order,
    refused: impl Fn(&E) -> bool,
    mut read: impl FnMut(Order) -> Result<T, E>,
) -> Result<T, E> {
    match read(*order) {
        Err(e) if *order == Order::Bgra && refused(&e) => {
            eprintln!("bitscomp: renderer gles: BGRA readback refused, reading RGBA (red and blue swapped here)");
            *order = Order::Rgba;
            read(*order)
        }
        m => m,
    }
}

/// The order of the bytes GL gave back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// B G R A (GL_BGRA_EXT): as a little-endian u32 already 0xAARRGGBB, XRGB8888's layout.
    Bgra,
    /// R G B A (GL_RGBA, the format every GLES must read): red and blue swapped.
    Rgba,
}

/// Copies one read-back rectangle (`bytes`: `w * h * 4` bytes, its top row first) into `dst`, the picture's
/// XRGB8888 rows `dst_stride` u32 apart, at `rect` (picture coordinates, already clamped). Short input copies
/// what is there; nothing outside `rect` or `dst` is written.
pub fn copy_rows(dst: &mut [u32], dst_stride: usize, rect: (i32, i32, i32, i32), bytes: &[u8], order: Order) {
    let (x, y, w, h) = (rect.0.max(0) as usize, rect.1.max(0) as usize, rect.2.max(0) as usize, rect.3.max(0) as usize);
    if w == 0 || x + w > dst_stride {
        return;
    }
    let row_bytes = w * 4;
    for (i, src) in bytes.chunks_exact(row_bytes).take(h).enumerate() {
        let start = (y + i) * dst_stride + x;
        let Some(out) = dst.get_mut(start..start + w) else { continue };
        for (d, p) in out.iter_mut().zip(src.chunks_exact(4)) {
            *d = match order {
                Order::Bgra => u32::from_le_bytes([p[0], p[1], p[2], p[3]]),
                Order::Rgba => u32::from_le_bytes([p[2], p[1], p[0], p[3]]),
            };
        }
    }
}

/// Whether two XRGB8888 pixels agree within `tol` per colour channel (the check frame; the X byte is ignored).
pub fn near(a: u32, b: u32, tol: u8) -> bool {
    (0..3).all(|c| {
        let (pa, pb) = ((a >> (8 * c)) as u8, (b >> (8 * c)) as u8);
        pa.abs_diff(pb) <= tol
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_cuts_to_the_picture() {
        assert_eq!(clamp((10, 20, 30, 40), 100, 100), Some((10, 20, 30, 40)));
        assert_eq!(clamp((-5, -5, 10, 10), 100, 100), Some((0, 0, 5, 5)));
        assert_eq!(clamp((90, 95, 20, 20), 100, 100), Some((90, 95, 10, 5)));
        assert_eq!(clamp((100, 0, 5, 5), 100, 100), None);
        assert_eq!(clamp((0, 0, 0, 5), 100, 100), None);
        assert_eq!(clamp((i32::MAX - 1, 0, i32::MAX, 5), 100, 100), None);
    }

    #[test]
    fn copy_puts_rows_in_place_and_keeps_the_rest() {
        // A 4x3 picture; a 2x2 rectangle at (1,1) read back: its first row is the picture's row 1.
        let mut dst = vec![7u32; 4 * 3];
        let bytes = [
            1, 0, 0, 0xff, 2, 0, 0, 0xff, // -> picture row 1
            3, 0, 0, 0xff, 4, 0, 0, 0xff, // -> picture row 2
        ];
        copy_rows(&mut dst, 4, (1, 1, 2, 2), &bytes, Order::Bgra);
        assert_eq!(dst, vec![7, 7, 7, 7, 7, 0xff00_0001, 0xff00_0002, 7, 7, 0xff00_0003, 0xff00_0004, 7]);
    }

    #[test]
    fn rgba_swaps_red_and_blue() {
        let mut dst = vec![0u32; 1];
        copy_rows(&mut dst, 1, (0, 0, 1, 1), &[0x11, 0x22, 0x33, 0xff], Order::Rgba);
        assert_eq!(dst[0], 0xff11_2233);
        copy_rows(&mut dst, 1, (0, 0, 1, 1), &[0x11, 0x22, 0x33, 0xff], Order::Bgra);
        assert_eq!(dst[0], 0xff33_2211);
    }

    #[test]
    fn short_or_bad_input_writes_nothing_outside() {
        let mut dst = vec![0u32; 4];
        // Fewer bytes than the rectangle: only whole rows that came.
        copy_rows(&mut dst, 2, (0, 0, 2, 2), &[9, 0, 0, 0, 9, 0, 0, 0, 1], Order::Bgra);
        assert_eq!(dst, vec![9, 9, 0, 0]);
        // A rectangle wider than the stride is refused.
        copy_rows(&mut dst, 2, (1, 0, 2, 1), &[1; 8], Order::Bgra);
        assert_eq!(dst, vec![9, 9, 0, 0]);
        // Rows below the buffer are skipped.
        copy_rows(&mut dst, 2, (0, 1, 2, 3), &[5; 24], Order::Bgra);
        assert_eq!(dst[0..2], [9, 9]);
        assert_eq!(dst[2], 0x0505_0505);
    }

    #[test]
    fn full_is_answered_once_and_comes_back_after_a_failed_frame() {
        let mut f = Full::new();
        assert!(f.begin(), "the first frame reads everything");
        f.done();
        assert!(!f.begin(), "a good frame is followed by a damage-only one");
        f.done();
        assert!(!f.begin());
        // This frame fails before `done`: the next one reads everything again.
        assert!(f.begin());
        f.done();
        assert!(!f.begin());
    }

    #[test]
    fn plan_cuts_damage_and_full_means_the_whole_picture() {
        assert_eq!(plan(&[], false, 100, 50), vec![]);
        assert_eq!(plan(&[], true, 100, 50), vec![(0, 0, 100, 50)]);
        assert_eq!(plan(&[(10, 10, 5, 5)], true, 100, 50), vec![(0, 0, 100, 50)]);
        assert_eq!(
            plan(&[(-10, -10, 20, 20), (200, 0, 5, 5), (95, 45, 10, 10), (0, 0, 0, 3), (1, 2, 3, 4)], false, 100, 50),
            vec![(0, 0, 10, 10), (95, 45, 5, 5), (1, 2, 3, 4)]
        );
        assert_eq!(plan(&[(i32::MIN, i32::MIN, i32::MAX, i32::MAX)], false, 100, 50), vec![]);
        // A degenerate picture (never made: `target` makes at least 1x1) reads nothing.
        assert_eq!(plan(&[], true, 0, 0), vec![]);
    }

    #[test]
    fn bgra_refused_falls_back_to_rgba_and_stays() {
        let mut order = Order::Bgra;
        let mut asked = Vec::new();
        let r: Result<u8, &str> = read_with_fallback(&mut order, |e| *e == "unsupported", |o| {
            asked.push(o);
            if o == Order::Bgra { Err("unsupported") } else { Ok(1) }
        });
        assert_eq!(r, Ok(1));
        assert_eq!(asked, vec![Order::Bgra, Order::Rgba]);
        assert_eq!(order, Order::Rgba);
        // Later frames ask RGBA right away.
        asked.clear();
        let r: Result<u8, &str> = read_with_fallback(&mut order, |e| *e == "unsupported", |o| {
            asked.push(o);
            Ok(2)
        });
        assert_eq!((r, asked), (Ok(2), vec![Order::Rgba]));
    }

    #[test]
    fn other_readback_errors_are_not_retried() {
        let mut order = Order::Bgra;
        let mut n = 0;
        let r: Result<u8, &str> = read_with_fallback(&mut order, |e| *e == "unsupported", |_| {
            n += 1;
            Err("lost")
        });
        assert_eq!((r, n, order), (Err("lost"), 1, Order::Bgra));
        // RGBA refused as well: no third try, the error goes up (the frame is skipped, the next reads everything).
        let mut order = Order::Rgba;
        let r: Result<u8, &str> = read_with_fallback(&mut order, |_| true, |_| Err("unsupported"));
        assert_eq!((r, order), (Err("unsupported"), Order::Rgba));
    }

    #[test]
    fn near_ignores_the_x_byte() {
        assert!(near(0x0033_6699, 0xff33_6699, 0));
        assert!(near(0x0034_6698, 0x0033_6699, 1));
        assert!(!near(0x0036_6699, 0x0033_6699, 1));
    }
}
