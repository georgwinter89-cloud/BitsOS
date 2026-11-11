//! WY-17 (gx-scout-mesa.md C3): the renderer draws straight into what the screen shows. V2's fbdev hands its two
//! windows out as dma-bufs (FBIO_BITSOS_SCANOUT, gopfb fbdev/scanout.rs; ENOTTY on Linux and on a kernel without it):
//! with a flipping display engine they are the scanout buffers themselves, without one fb0's RAM. The renderer
//! imports them as render targets and draws into the hidden one; FBIOPAN_DISPLAY flips (same mode, same resolution,
//! the flips the driver already does: GX-D9), and no CPU copy of the picture exists any more. Damage tracking stays:
//! a window is two frames old when it comes back, so the tracker draws at age 2 (`Flip::aim`).
//!
//! `start` chooses at start, once, and logs it. Whatever is missing leaves today's readback path exactly as it is:
//! the renderer draws by CPU, no render node, a kernel without the ioctl, an import the EGL refuses. Later, a refused
//! pan or a screen that went back ends the direct path for good (`Fbdev::leave_direct`): readback from then on.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::Backend;
use crate::render::Composer;

/// V2's `_IOWR('F', 0xB4, struct bitsos_fb_scanout)` (48 bytes).
const FBIO_BITSOS_SCANOUT: u64 = 0xC030_46B4;
const SIZE: usize = 48;
const OP_EXPORT: u32 = 1;
const OP_BEGIN: u32 = 2;
const OP_END: u32 = 3;
const FLAG_ENGINE: u32 = 1;
const FLAG_WHOLE: u32 = 2;
/// DRM_FORMAT_XRGB8888: fb0's only format, and what the windows hold.
pub const XRGB8888: u32 = 0x3432_5258;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Args {
    op: u32,
    window: u32,
    fd: i32,
    flags: u32,
    width: u32,
    height: u32,
    stride: u32,
    fourcc: u32,
    offset: u64,
    size: u64,
}

impl Args {
    fn to_bytes(self) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        for (i, v) in [self.op, self.window, self.fd as u32, self.flags, self.width, self.height, self.stride, self.fourcc].into_iter().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[32..40].copy_from_slice(&self.offset.to_le_bytes());
        b[40..48].copy_from_slice(&self.size.to_le_bytes());
        b
    }
    fn from_bytes(b: &[u8; SIZE]) -> Args {
        let u = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let q = |o: usize| u64::from(u(o)) | u64::from(u(o + 4)) << 32;
        Args { op: u(0), window: u(4), fd: u(8) as i32, flags: u(12), width: u(16), height: u(20), stride: u(24), fourcc: u(28), offset: q(32), size: q(40) }
    }
}

/// `f` again while a signal interrupted it (a client's SIGCHLD during BEGIN's wait for the last flip): the kernel did
/// nothing yet, so this is no refusal; ending the direct path for it would cost a whole picture and the path for good.
fn retrying<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    loop {
        match f() {
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
            r => return r,
        }
    }
}

fn call(fd: RawFd, op: u32, window: u32) -> std::io::Result<Args> {
    retrying(|| {
        let mut b = Args { op, window, fd: -1, ..Args::default() }.to_bytes();
        // SAFETY: `b` is valid for reads and writes of the 48 bytes the request encodes.
        let r = unsafe { libc::ioctl(fd, FBIO_BITSOS_SCANOUT as libc::Ioctl, b.as_mut_ptr()) };
        if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(Args::from_bytes(&b)) }
    })
}

/// One window as the renderer imports it (only the gpu build has a renderer that does).
#[derive(Debug)]
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub struct Window {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

impl Window {
    /// Whether the window maps for writing, which a software rasteriser's import takes for granted (llvmpipe crashes
    /// on a dma-buf whose mmap fails, instead of refusing it: seen with wy/88's `badfd`).
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    pub fn maps(&self, height: i32) -> Result<(), String> {
        let len = (self.offset as usize + self.stride as usize * height as usize).max(1);
        // SAFETY: a fresh shared mapping of `len` bytes of an fd we hold, unmapped below; nothing touches it.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, std::os::fd::AsRawFd::as_raw_fd(&self.fd), 0) };
        if p == libc::MAP_FAILED {
            return Err(format!("the window does not map: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: the mapping just made, whole.
        unsafe { libc::munmap(p, len) };
        Ok(())
    }
}

/// The two windows, with what they are.
#[derive(Debug)]
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub struct Windows {
    pub list: Vec<Window>,
    pub size: (i32, i32),
    pub fourcc: u32,
    /// They are the scanout buffers (write-combined device memory), not fb0's RAM.
    pub engine: bool,
}

/// Where the next frame goes: `window`, last drawn `age` frames ago (0: its content counts for nothing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aim {
    pub window: usize,
    pub age: usize,
}

/// Both windows of fb0 with the mode the back end runs.
pub fn export(fd: RawFd, size: (i32, i32)) -> Result<Windows, String> {
    let mut list = Vec::with_capacity(2);
    let mut engine = false;
    for w in 0..2 {
        let a = call(fd, OP_EXPORT, w).map_err(|e| match e.raw_os_error() {
            Some(libc::ENOTTY) => "this kernel hands out no scanout buffers (ENOTTY)".to_string(),
            _ => format!("window {w} refused: {e}"),
        })?;
        // SAFETY: the kernel just installed this fd for this process and nothing else knows it; we own it now.
        let owned = unsafe { OwnedFd::from_raw_fd(a.fd) };
        if (a.width as i32, a.height as i32) != size || a.fourcc != XRGB8888 {
            return Err(format!("window {w} is {}x{} in format {:#x}, the mode {}x{}", a.width, a.height, a.fourcc, size.0, size.1));
        }
        let offset = u32::try_from(a.offset).map_err(|_| format!("window {w} starts at byte {} of its buffer", a.offset))?;
        let end = u64::from(a.stride).checked_mul(a.height as u64).and_then(|n| n.checked_add(a.offset));
        if end.is_none_or(|e| e > a.size) || a.stride < a.width.saturating_mul(4) {
            return Err(format!("window {w}: {} bytes per line at byte {} do not fit its {} bytes", a.stride, a.offset, a.size));
        }
        engine |= a.flags & FLAG_ENGINE != 0;
        list.push(Window { fd: owned, offset, stride: a.stride });
    }
    Ok(Windows { list, size, fourcc: XRGB8888, engine })
}

/// BEGIN: waits for the last flip; the window to draw next and whether it must be redrawn whole.
pub fn begin(fd: RawFd) -> std::io::Result<(usize, bool)> {
    let a = call(fd, OP_BEGIN, 0)?;
    Ok(((a.window & 1) as usize, a.flags & FLAG_WHOLE != 0))
}

/// END: fb0's memory is the picture again (the next store is whole).
pub fn end(fd: RawFd) -> std::io::Result<()> {
    call(fd, OP_END, 0).map(|_| ())
}

/// How many frames still draw all of their window whatever its age: after a frame that failed or outlasted its wait,
/// the damage tracker counted a frame that neither window got whole (Smithay records the damage before it draws), so
/// both are redrawn. A frame counts as failed until `finished` says otherwise, so an early return cannot forget it.
#[derive(Debug, Default)]
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub struct Redraw {
    owed: u8,
    after: u8,
}

#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
impl Redraw {
    /// A frame starts at `age`: the age it draws at.
    pub fn start(&mut self, age: usize) -> usize {
        let age = if self.owed > 0 { 0 } else { age };
        self.after = self.owed.saturating_sub(1);
        self.owed = 2;
        age
    }
    /// The frame reached its window whole and may be flipped.
    pub fn finished(&mut self) {
        self.owed = self.after;
    }
}

/// `--scanout`: what the start may choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// Direct when everything is there, a render node included (the default).
    Auto,
    /// Direct without asking for a node (a host test's fake, a QEMU comparison).
    Direct,
    /// Today's path, whatever the machine could do.
    Readback,
}

impl Want {
    pub fn parse(v: &str) -> Option<Want> {
        match v {
            "auto" => Some(Want::Auto),
            "direct" => Some(Want::Direct),
            "readback" => Some(Want::Readback),
            _ => None,
        }
    }
}

/// Why the start does not even try; Ok: it asks the back end and the renderer.
pub fn plan(want: Want, renderer: &str, node: Option<&str>) -> Result<(), String> {
    if want == Want::Readback {
        return Err("--scanout readback".into());
    }
    if renderer != "gles" {
        return Err(format!("renderer {renderer} draws by CPU"));
    }
    if want == Want::Auto && node.is_none() {
        return Err("no render node".into());
    }
    Ok(())
}

/// The choice, made once at start and said once.
pub fn start(backend: &mut dyn Backend, comp: &mut dyn Composer, want: Want, node: Option<&str>) {
    let tried = plan(want, comp.renderer(), node).and_then(|()| {
        let w = backend.scanout_windows()?;
        let engine = w.engine;
        comp.adopt_scanout(w)?;
        Ok(engine)
    });
    match tried {
        Ok(engine) => {
            backend.scanout_on();
            let what = if engine { "the scanout buffers" } else { "fb0's memory" };
            eprintln!("bitscomp: scanout: direct: {} draws into two dma-buf windows of {what} and flips, no copy of the picture", comp.renderer());
        }
        Err(why) => eprintln!("bitscomp: scanout: readback: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_is_the_kernels() {
        let a = Args { op: 2, window: 1, fd: -1, flags: 3, width: 4, height: 5, stride: 6, fourcc: XRGB8888, offset: 1 << 40, size: 9 };
        let b = a.to_bytes();
        assert_eq!(Args::from_bytes(&b), a);
        assert_eq!(&b[8..12], &[0xFF; 4], "fd at byte 8");
        assert_eq!(&b[28..32], b"XR24", "fourcc at byte 28");
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 1 << 40, "offset at byte 32");
        // _IOWR('F', 0xB4, 48): gopfb's FBIO_BITSOS_SCANOUT.
        assert_eq!(FBIO_BITSOS_SCANOUT, 3 << 30 | (SIZE as u64) << 16 | (b'F' as u64) << 8 | 0xB4);
    }

    #[test]
    fn frames_keep_their_age_until_one_fails() {
        let mut r = Redraw::default();
        for _ in 0..3 {
            assert_eq!(r.start(2), 2, "a frame that worked leaves the next one its damage");
            r.finished();
        }
        let _ = r.start(2);
        // No `finished`: it failed or came late. Both windows are drawn whole, then the damage counts again.
        assert_eq!(r.start(2), 0);
        r.finished();
        assert_eq!(r.start(2), 0);
        r.finished();
        assert_eq!(r.start(2), 2);
        r.finished();
        // One that fails while whole frames are owed starts the two over.
        let _ = r.start(2);
        assert_eq!(r.start(2), 0);
        r.finished();
        let _ = r.start(2);
        assert_eq!(r.start(2), 0);
        r.finished();
        assert_eq!(r.start(2), 0);
        r.finished();
        assert_eq!(r.start(2), 2);
    }

    #[test]
    fn a_signal_is_no_refusal() {
        let mut left = 3;
        let r = retrying(|| {
            if left > 0 {
                left -= 1;
                Err(std::io::Error::from_raw_os_error(libc::EINTR))
            } else {
                Ok(7)
            }
        });
        assert_eq!(r.unwrap(), 7);
        let e = retrying(|| -> std::io::Result<()> { Err(std::io::Error::from_raw_os_error(libc::EIO)) }).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EIO), "any other error is one");
    }

    #[test]
    fn the_start_tries_only_with_a_gpu_renderer_and_a_node() {
        let n = Some("/dev/dri/renderD128");
        assert_eq!(plan(Want::Auto, "gles", n), Ok(()));
        assert_eq!(plan(Want::Auto, "gles", None), Err("no render node".into()));
        assert_eq!(plan(Want::Direct, "gles", None), Ok(()), "a test's fake needs no node");
        assert_eq!(plan(Want::Auto, "pixman", n), Err("renderer pixman draws by CPU".into()));
        assert_eq!(plan(Want::Direct, "pixman", n), Err("renderer pixman draws by CPU".into()));
        assert_eq!(plan(Want::Readback, "gles", n), Err("--scanout readback".into()));
    }

    #[test]
    fn a_device_without_the_ioctl_is_named() {
        // An ordinary file answers ENOTTY as a kernel without FBIO_BITSOS_SCANOUT does.
        let f = std::fs::File::open("/proc/self/exe").unwrap();
        let e = export(std::os::fd::AsRawFd::as_raw_fd(&f), (4, 4)).unwrap_err();
        assert!(e.contains("ENOTTY"), "{e}");
    }
}
