//! Back ends: where the composed frame goes and where input comes from.
//! - `winit`: a window in the host's Wayland session (WY-0 on CachyOS), the frame copied into wl_shm
//!   buffers (`hostshm`), so the nested path is as CPU-only as the Mini-PC's.
//! - `headless`: no window; the frame is copied into a plain memory buffer like an fbdev mapping
//!   (measurements at 2560x1440 and scripted tests).
//! - `fbdev`: V2 itself (WY-1a): `/dev/fb0` for the picture, `/dev/input/event*` read directly for input
//!   (`evdev`, no udev or libinput), the console handed over by KD_GRAPHICS (`vt`).

use std::os::fd::BorrowedFd;
use std::time::Duration;

use smithay::utils::{Physical, Rectangle};

use crate::render::Picture;
use crate::state::App;

pub mod evdev;
pub mod evdev_map;
#[cfg(test)]
mod evdev_tests;
pub mod fbcursor;
pub mod fbdev;
mod flip;
pub mod headless;
pub mod scanout;
pub mod hotplug;
#[cfg(feature = "nested")]
pub mod hostshm;
pub mod vt;
#[cfg(feature = "nested")]
pub mod winit;

pub trait Backend {
    /// Physical size of the output.
    fn size(&self) -> (i32, i32);
    /// Moves pending input and window events into the app (never blocks).
    fn dispatch(&mut self, app: &mut App);
    /// Copies the damaged parts of the composed frame out.
    fn present(&mut self, comp: &dyn Picture, damage: &[Rectangle<i32, Physical>]);
    /// Shows damage a previous `present` had to hold back (called every loop turn).
    fn flush(&mut self, _comp: &dyn Picture) {}
    /// An fd that becomes readable when `dispatch` has work, so the loop can sleep instead of polling.
    fn wake_fd(&self) -> Option<BorrowedFd<'static>> {
        None
    }
    /// bitscomp draws the pointer into its frames (no host cursor to use).
    fn draws_pointer(&self) -> bool {
        false
    }
    /// WY-11: the hardware cursor door, taken once at start (None: the pointer is drawn into the frames).
    fn hw_cursor(&mut self) -> Option<Box<dyn crate::hwcursor::HwCursor>> {
        None
    }
    /// The scanout's frame period and one vblank time on CLOCK_MONOTONIC, when the output knows them.
    fn pacing(&self) -> Option<(Duration, Duration)> {
        None
    }
    /// WY-17: the output's two windows as dma-bufs for the renderer to draw into; why not.
    fn scanout_windows(&mut self) -> Result<scanout::Windows, String> {
        Err("this back end has no scanout buffers".into())
    }
    /// WY-17: the renderer took them: from now on `present` flips and copies nothing.
    fn scanout_on(&mut self) {}
    /// WY-17, before a frame: where the renderer draws it (None: into its picture, for `present` to copy). May wait
    /// for the last flip.
    fn scanout_frame(&mut self) -> Option<scanout::Aim> {
        None
    }
    /// WY-17: the renderer cannot draw there any more: back to the copy.
    fn scanout_lost(&mut self, _why: &str) {}
    /// WY-17, after a frame: the direct path ended just now, so the picture must be drawn again.
    fn scanout_left(&mut self) -> bool {
        false
    }
}

/// Copies rectangles of the shadow image into `dst` (same format, `dst_stride` in pixels).
pub fn copy_rects(comp: &dyn Picture, dst: &mut [u32], dst_stride: usize, rects: &[Rectangle<i32, Physical>]) {
    let (w, h) = comp.size();
    let (src, stride) = comp.pixels();
    for r in rects {
        let x0 = r.loc.x.clamp(0, w) as usize;
        let x1 = (r.loc.x + r.size.w).clamp(0, w) as usize;
        let y0 = r.loc.y.clamp(0, h) as usize;
        let y1 = (r.loc.y + r.size.h).clamp(0, h) as usize;
        for y in y0..y1 {
            let d = y * dst_stride;
            if d + x1 > dst.len() {
                break;
            }
            dst[d + x0..d + x1].copy_from_slice(&src[y * stride + x0..y * stride + x1]);
        }
    }
}
