//! WY-15: the frame on the screen is read back from GL into a CPU-side XRGB8888 buffer on request (`ctl shot`,
//! Durchblick's picture). The arithmetic of that copy lives here, without GL, so it is tested on the host.
//! Rows: Smithay's projection for a framebuffer target (its flip180, gles/mod.rs:2088) puts the picture's top row
//! into row 0, so a glReadPixels rectangle at picture coordinates comes back top row first, as the picture. Bytes:
//! B G R A when GL can read BGRA, else R G B A (then red and blue are swapped): the format choice that adapts to the
//! driver.

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
/// XRGB8888 rows `dst_stride` u32 apart, at `rect` (picture coordinates, inside the picture). Short input copies
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
