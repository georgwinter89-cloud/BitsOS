//! WY-11: GX-5a's hardware cursor door on `/dev/fb0` (gopfb fbdev/cursor.rs, change request 1 of
//! plan/notes/gx-GX-5a.md): `FBIO_BITSOS_CURSOR = _IOW('F', 0xB2, struct bitsos_fb_cursor)`, 40 bytes
//! `{u32 op (0 hide, 1 image, 2 move); u32 flags (bit 0 straight alpha); s32 x, y (hotspot, screen pixels);
//! u32 width, height (1..256); u32 hot_x, hot_y; u64 pixels}`. Linux (and a V2 kernel without it) answers
//! ENOTTY, QEMU's V2 ENODEV: both mean the pointer stays in the frames.

use std::fs::File;
use std::os::fd::AsRawFd;

use crate::cursor_theme::CursorImage;
use crate::hwcursor::HwCursor;

pub const FBIO_BITSOS_CURSOR: u64 = 0x4028_46B2;
const OP_HIDE: u32 = 0;
const OP_IMAGE: u32 = 1;
const OP_MOVE: u32 = 2;

/// The ioctl's argument, as the kernel reads it.
#[repr(C)]
#[derive(Default)]
struct Arg {
    op: u32,
    flags: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    hot_x: u32,
    hot_y: u32,
    pixels: u64,
}

const _: () = assert!(std::mem::size_of::<Arg>() == 40);

/// The cursor on its own duplicate of fb0's fd (the screen holder is the process, not the fd).
pub struct FbCursor {
    file: File,
}

impl FbCursor {
    pub fn new(fb: &File) -> Option<FbCursor> {
        fb.try_clone().ok().map(|file| FbCursor { file })
    }

    fn call(&self, a: &Arg) -> Result<(), i32> {
        // SAFETY: `a` is a live 40-byte struct of the layout the request encodes; the kernel only reads it, and
        // `pixels` (if any) points to width*height u32 that outlive the call.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), FBIO_BITSOS_CURSOR as libc::Ioctl, a as *const Arg) };
        if r < 0 { Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)) } else { Ok(()) }
    }
}

impl HwCursor for FbCursor {
    fn image(&mut self, img: &CursorImage, at: (i32, i32)) -> Result<(), i32> {
        if img.px.len() != (img.w as usize) * (img.h as usize) {
            return Err(libc::EINVAL);
        }
        let a = Arg {
            op: OP_IMAGE,
            x: at.0,
            y: at.1,
            width: img.w,
            height: img.h,
            hot_x: img.hot.0,
            hot_y: img.hot.1,
            pixels: img.px.as_ptr() as u64,
            ..Arg::default()
        };
        self.call(&a)
    }

    fn moved(&mut self, at: (i32, i32)) -> Result<(), i32> {
        self.call(&Arg { op: OP_MOVE, x: at.0, y: at.1, ..Arg::default() })
    }

    fn hide(&mut self) -> Result<(), i32> {
        self.call(&Arg { op: OP_HIDE, ..Arg::default() })
    }
}

impl Drop for FbCursor {
    fn drop(&mut self) {
        // The kernel hides it too when the screen goes back to the console; this covers a plain exit first.
        let _ = self.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's decoder, copied from gopfb fbdev/cursor.rs in plan/notes/gx-GX-5a-cr.patch (`Arg::from_bytes`):
    /// (op, flags, x, y, width, height, hot_x, hot_y, pixels).
    fn kernel_reads(b: &[u8; 40]) -> (u32, u32, i32, i32, u32, u32, u32, u32, u64) {
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        (u(0), u(4), u(8) as i32, u(12) as i32, u(16), u(20), u(24), u(28), u64::from(u(32)) | u64::from(u(36)) << 32)
    }

    fn bytes(a: &Arg) -> [u8; 40] {
        // SAFETY: Arg is repr(C), 40 bytes (asserted above) of plain integers without padding; reading it as bytes
        // is what the kernel's copy_from_user does.
        unsafe { std::ptr::read_unaligned((a as *const Arg).cast::<[u8; 40]>()) }
    }

    #[test]
    fn request_number_is_iow_f_b2_40() {
        // _IOW(type, nr, size) = 1 << 30 | size << 16 | type << 8 | nr (linux-6.6/include/uapi/asm-generic/ioctl.h).
        assert_eq!(FBIO_BITSOS_CURSOR, (1 << 30) | (40 << 16) | ((b'F' as u64) << 8) | 0xB2);
        assert_eq!((OP_HIDE, OP_IMAGE, OP_MOVE), (0, 1, 2));
    }

    #[test]
    fn the_kernel_reads_what_we_write() {
        let a = Arg { op: OP_IMAGE, flags: 0, x: -7, y: 1439, width: 48, height: 40, hot_x: 22, hot_y: 24, pixels: 0x1122_3344_5566_7788 };
        assert_eq!(kernel_reads(&bytes(&a)), (1, 0, -7, 1439, 48, 40, 22, 24, 0x1122_3344_5566_7788));
        let m = Arg { op: OP_MOVE, x: 2559, y: -1, ..Arg::default() };
        assert_eq!(kernel_reads(&bytes(&m)), (2, 0, 2559, -1, 0, 0, 0, 0, 0));
        // Pre-multiplied (flags 0) is what the theme files and wl_shm Argb8888 carry.
        assert_eq!(Arg::default().flags, 0);
    }
}
