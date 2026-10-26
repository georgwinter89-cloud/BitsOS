//! GX-5a (change request of lane GX, plan/notes/gx-GX-5a.md): the display engine's hardware cursor for the
//! screen's holder (the compositor), one BitsOS ioctl on /dev/fb0 in DP-3's private 'F' range (ENOTTY on Linux,
//! where a compositor uses the DRM cursor plane instead):
//!
//! ```c
//! struct bitsos_fb_cursor {
//!     __u32 op;            /* 0 hide, 1 image (and show), 2 move (and show) */
//!     __u32 flags;         /* op 1: bit 0 = straight alpha (default: pre-multiplied ARGB8888) */
//!     __s32 x, y;          /* op 1, 2: where the hotspot goes, screen pixels; any value (all off = hidden) */
//!     __u32 width, height; /* op 1: 1..256 each */
//!     __u32 hot_x, hot_y;  /* op 1: inside the image */
//!     __u64 pixels;        /* op 1: width * height ARGB8888 pixels, row by row, no padding */
//! };
//! #define FBIO_BITSOS_CURSOR _IOW('F', 0xB2, struct bitsos_fb_cursor)   /* 0x402846B2 */
//! ```
//!
//! Errors, in this order: EFAULT the struct is unreadable; EINVAL op or flags unknown; ENODEV no hardware cursor
//! (no display engine, e.g. QEMU): draw the pointer in software; EPERM the caller is not fb0's screen holder (a
//! kernel caller included); EINVAL size or hotspot; EFAULT the pixels are unreadable; EBUSY another image is
//! being written; EIO a register write failed. A move is a few register writes taken together at the next
//! VUPDATE (no frame, no plane programmed again). The cursor hides when the screen goes back to the console
//! (`handed_back`).

use super::abi::UserIo;
use super::Fb;
use super::flip::{self, CursorError};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use proc::Process;
use uapi::errno::{EBUSY, EFAULT, EINVAL, EIO, ENODEV, EPERM};
use vfs::Errno;

/// _IOW('F', 0xB2, 40 bytes).
pub const FBIO_BITSOS_CURSOR: u64 = 0x4028_46B2;
pub const OP_HIDE: u32 = 0;
pub const OP_IMAGE: u32 = 1;
pub const OP_MOVE: u32 = 2;
pub const FLAG_STRAIGHT: u32 = 1;
/// The struct's size.
pub const ARG_BYTES: usize = 40;
/// The display engine's largest cursor (DCN 3.1.4 `max_cursor_size`).
pub const MAX: u32 = 256;

/// The process whose cursor shows (0: none): it hides when that process gives the screen back.
static OWNER: AtomicU32 = AtomicU32::new(0);

klog::counter!(MOVES, "fbdev.cursor.moves");
klog::counter!(REFUSED, "fbdev.cursor.refused");

/// The ioctl's argument, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arg {
    pub op: u32,
    pub flags: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub pixels: u64,
}

impl Arg {
    pub fn from_bytes(b: &[u8; ARG_BYTES]) -> Arg {
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Arg {
            op: u(0),
            flags: u(4),
            x: u(8) as i32,
            y: u(12) as i32,
            width: u(16),
            height: u(20),
            hot_x: u(24),
            hot_y: u(28),
            pixels: u64::from(u(32)) | u64::from(u(36)) << 32,
        }
    }
    pub fn to_bytes(&self) -> [u8; ARG_BYTES] {
        let mut b = [0u8; ARG_BYTES];
        for (i, v) in [self.op, self.flags, self.x as u32, self.y as u32, self.width, self.height, self.hot_x, self.hot_y].iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[32..].copy_from_slice(&self.pixels.to_le_bytes());
        b
    }
}

fn errno(e: CursorError) -> Errno {
    match e {
        CursorError::NoDevice => ENODEV,
        CursorError::Invalid => EINVAL,
        CursorError::Busy => EBUSY,
        CursorError::Failed => EIO,
    }
}

/// The pid of the last logged refusal: a program looping on a refused call is counted, logged once.
static LAST_REFUSED: AtomicU32 = AtomicU32::new(u32::MAX);

fn refuse(pid: u32, op: u32, why: &'static str, e: Errno) -> Result<Vec<u8>, Errno> {
    REFUSED.inc();
    if LAST_REFUSED.swap(pid, Ordering::AcqRel) != pid {
        klog::event!(Info, "fbdev", "cursor.refused", pid = pid, op = op, why = why);
    }
    Err(e)
}

/// `FBIO_BITSOS_CURSOR` from `who` (None: a kernel caller, refused with EPERM where a cursor exists).
pub(super) fn ioctl(fb: &Fb, arg: u64, io: &dyn UserIo, who: Option<&Arc<Process>>) -> Result<Vec<u8>, Errno> {
    let mut b = [0u8; ARG_BYTES];
    if !io.read(arg, &mut b) {
        return Err(EFAULT);
    }
    let a = Arg::from_bytes(&b);
    if a.op > OP_MOVE || a.flags & !FLAG_STRAIGHT != 0 {
        return Err(EINVAL);
    }
    let Some(engine) = flip::get().filter(|f| f.cursor_ready()) else {
        return Err(ENODEV);
    };
    let pid = who.map_or(0, |p| p.pid);
    if pid == 0 || fb.holder_pid() != pid {
        return refuse(pid, a.op, "not the screen's holder", EPERM);
    }
    // Owner before the engine call: a hand-back racing with it finds this pid and hides the cursor.
    if a.op != OP_HIDE {
        OWNER.store(pid, Ordering::Release);
    }
    let r = match a.op {
        OP_HIDE => engine.cursor_hide(),
        OP_MOVE => {
            MOVES.inc();
            engine.cursor_move((a.x, a.y))
        }
        _ => {
            if a.width == 0 || a.height == 0 || a.width > MAX || a.height > MAX || a.hot_x >= a.width || a.hot_y >= a.height {
                return refuse(pid, a.op, "size or hotspot", EINVAL);
            }
            let mut raw = vec![0u8; (a.width * a.height * 4) as usize];
            if !io.read(a.pixels, &mut raw) {
                return Err(EFAULT);
            }
            let px: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            engine.cursor_image(a.width, a.height, (a.hot_x, a.hot_y), a.flags & FLAG_STRAIGHT != 0, &px, (a.x, a.y))
        }
    };
    match r {
        Ok(()) => {
            if a.op == OP_HIDE {
                let _ = OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire);
            } else if fb.holder_pid() != pid && OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                // The screen changed hands during the call (its hand-back may have run before it): hide.
                let _ = engine.cursor_hide();
                return refuse(pid, a.op, "screen handed back during the call", EPERM);
            }
            Ok(Vec::new())
        }
        Err(e) => refuse(pid, a.op, "engine refused", errno(e)),
    }
}

/// The screen went back to the console from `pid` (`Fb::settle`, outside `fbdev.screen`): its cursor hides.
pub(super) fn handed_back(pid: u32) {
    if pid != 0 && OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        let r = flip::get().map(|f| f.cursor_hide());
        klog::event!(Info, "fbdev", "cursor.handed_back", pid = pid, hidden = matches!(r, Some(Ok(()))));
    }
}

/// The process whose cursor shows (0: none), for the ktests.
pub fn owner() -> u32 {
    OWNER.load(Ordering::Acquire)
}
