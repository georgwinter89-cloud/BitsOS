//! SHOT: copying a rectangle out of a picture (MCP `screen` reads only the region it was asked for) and why a copy
//! of the screen's picture failed.

use crate::Rect;
use alloc::vec::Vec;

/// Why `copy_screen` / `ScanoutSource::copy_rect` gave no picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureError {
    /// No display, or the owner has no picture now.
    NoPicture,
    /// The rectangle does not lie in the picture, which is this size (it changed size meanwhile).
    Size(u32, u32),
    /// The owner's picture could not be held still for the whole copy (bounded hold): ask again.
    NotWhole,
    /// No memory for the copy.
    NoMemory,
}

/// `r` lies inside a `w` x `h` picture (an empty `r` too, when its corner does).
pub fn rect_fits(r: Rect, w: u32, h: u32) -> bool {
    r.x.checked_add(r.w).is_some_and(|e| e <= w) && r.y.checked_add(r.h).is_some_and(|e| e <= h)
}

/// Copies rectangle `r` of a `w` x `h` picture (`px`, row by row) into `dst`, cleared first, `r.w * r.h` pixels row
/// by row. Allocates only when `dst` has less room (callers under a lock reserve it before).
pub fn copy_rect(px: &[u32], w: u32, h: u32, r: Rect, dst: &mut Vec<u32>) -> Result<(u32, u32), PictureError> {
    if !rect_fits(r, w, h) || px.len() < w as usize * h as usize {
        return Err(PictureError::Size(w, h));
    }
    dst.clear();
    for row in r.y..r.y + r.h {
        let at = row as usize * w as usize + r.x as usize;
        dst.extend_from_slice(&px[at..at + r.w as usize]);
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_region_is_cut_row_by_row() {
        let (w, h) = (7u32, 5u32);
        let px: Vec<u32> = (0..w * h).collect();
        let mut dst = vec![99; 3];
        assert_eq!(copy_rect(&px, w, h, Rect::new(2, 1, 3, 2), &mut dst), Ok((7, 5)));
        assert_eq!(dst, vec![9, 10, 11, 16, 17, 18], "the old content is gone");
        assert_eq!(copy_rect(&px, w, h, Rect::new(0, 0, 7, 5), &mut dst), Ok((7, 5)));
        assert_eq!(dst, px, "the whole picture");
        assert_eq!(copy_rect(&px, w, h, Rect::new(6, 4, 1, 1), &mut dst), Ok((7, 5)));
        assert_eq!(dst, vec![34], "the last pixel");
        assert_eq!(copy_rect(&px, w, h, Rect::new(7, 0, 0, 5), &mut dst), Ok((7, 5)));
        assert!(dst.is_empty(), "an empty rectangle on the edge");
    }

    #[test]
    fn a_region_outside_is_refused_with_the_size() {
        let px = vec![0u32; 35];
        let mut dst = Vec::new();
        for r in [Rect::new(5, 0, 3, 1), Rect::new(0, 4, 1, 2), Rect::new(u32::MAX, 0, 2, 1), Rect::new(0, 1, 1, u32::MAX)] {
            assert_eq!(copy_rect(&px, 7, 5, r, &mut dst), Err(PictureError::Size(7, 5)), "{r:?}");
        }
        assert_eq!(copy_rect(&px[..30], 7, 5, Rect::new(0, 0, 1, 1), &mut dst), Err(PictureError::Size(7, 5)), "a short picture");
        assert!(rect_fits(Rect::new(0, 0, 7, 5), 7, 5) && !rect_fits(Rect::new(1, 0, 7, 5), 7, 5));
    }
}
