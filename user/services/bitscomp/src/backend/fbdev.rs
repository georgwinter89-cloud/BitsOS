//! V2's output (WY-1a): `/dev/fb0` (linux-6.6/drivers/video/fbdev/core/fbmem.c; V2 crates/drivers/gopfb/src/
//! fbdev). FBIOGET_VSCREENINFO/FSCREENINFO give size, stride and format (32-bit XRGB is the only one V2 has);
//! the damaged rectangles of the shadow image are copied into the mapping: one window, whose dirty pages the kernel
//! copies at its next vblank estimate (deferred io). WY-8 (GX-3 change request 2), only with `--fb-pan` until the
//! Mini-PC has compared both: when yres_virtual and ypanstep allow, two windows are drawn alternately and shown by
//! FBIOPAN_DISPLAY (on V2 with GX-3 a real flip that returns once it landed; under QEMU the kernel copies the
//! whole window); the bookkeeping is `flip::Flip`. Either way the mapping is RAM,
//! so bitscomp's copy (timed in `stats.present`) never touches write-combined memory itself. Pacing:
//! FBIO_BITSOS_PACING (V2 only, ENOTTY on Linux) gives the scanout's period and vblank grid for the frame timer.
//! Later the DP-4 surface device replaces fbdev.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Duration;

use smithay::reexports::calloop::LoopHandle;
use smithay::utils::{Physical, Rectangle};

use super::flip::{Flip, Refused};
use super::{Backend, copy_rects, evdev, vt};
use crate::render::Compositor;
use crate::state::App;

const FBIOGET_VSCREENINFO: u64 = 0x4600;
const FBIOGET_FSCREENINFO: u64 = 0x4602;
const FBIOPAN_DISPLAY: u64 = 0x4606;
/// V2's `_IOR('F', 0xB0, struct bitsos_fb_pacing)` (64 bytes; gopfb fbdev/pacing.rs).
const FBIO_BITSOS_PACING: u64 = 0x8040_46B0;
/// `struct fb_var_screeninfo`: 40 u32 (160 bytes); the indexes used here.
const VAR_WORDS: usize = 40;
const XRES: usize = 0;
const YRES: usize = 1;
const YRES_VIRTUAL: usize = 3;
const YOFFSET: usize = 5;
const BPP: usize = 6;
const RED_OFF: usize = 8;
const GREEN_OFF: usize = 11;
const BLUE_OFF: usize = 14;
/// `struct fb_fix_screeninfo` (80 bytes on x86-64): smem_len at 24, ypanstep (u16) at 42, line_length at 48.
const FIX_SIZE: usize = 80;

pub struct Fbdev {
    file: File,
    map: *mut u32,
    map_len: usize,
    size: (i32, i32),
    stride_px: usize,
    /// GX-3 CR 2: `var` is the screeninfo to pan with, `flip` which window is drawn and shown.
    var: [u8; VAR_WORDS * 4],
    flip: Flip,
    /// Dropped after the mapping (fields drop in order after `Drop::drop`): the console comes back last.
    _console: vt::Console,
}

fn ioctl_buf(fd: i32, req: u64, buf: &mut [u8]) -> std::io::Result<()> {
    // SAFETY: `buf` is valid for writes of the size the request encodes (or the fixed struct size it names).
    let r = unsafe { libc::ioctl(fd, req as libc::Ioctl, buf.as_mut_ptr()) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

impl Fbdev {
    /// Opens `path` (normally /dev/fb0), takes the console's screen and starts the evdev input. `pan`: two
    /// windows when the device has room for them (else, or without `pan`, one window and the kernel's damage copy).
    pub fn new(path: &str, pan: bool, handle: &LoopHandle<'static, App>) -> Result<Fbdev, String> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let fd = file.as_raw_fd();
        let mut vb = [0u8; VAR_WORDS * 4];
        ioctl_buf(fd, FBIOGET_VSCREENINFO, &mut vb).map_err(|e| format!("{path}: FBIOGET_VSCREENINFO: {e}"))?;
        let var = |i: usize| u32::from_le_bytes([vb[i * 4], vb[i * 4 + 1], vb[i * 4 + 2], vb[i * 4 + 3]]);
        let mut fb = [0u8; FIX_SIZE];
        ioctl_buf(fd, FBIOGET_FSCREENINFO, &mut fb).map_err(|e| format!("{path}: FBIOGET_FSCREENINFO: {e}"))?;
        let fix = |o: usize| u32::from_le_bytes([fb[o], fb[o + 1], fb[o + 2], fb[o + 3]]) as usize;
        let (w, h, line) = (var(XRES) as usize, var(YRES) as usize, fix(48));
        if var(BPP) != 32 || var(RED_OFF) != 16 || var(GREEN_OFF) != 8 || var(BLUE_OFF) != 0 {
            return Err(format!(
                "{path}: {} bits per pixel, red/green/blue at {}/{}/{}: only 32-bit XRGB is drawn",
                var(BPP),
                var(RED_OFF),
                var(GREEN_OFF),
                var(BLUE_OFF)
            ));
        }
        if w == 0 || h == 0 || line < w * 4 || line % 4 != 0 {
            return Err(format!("{path}: odd mode {w}x{h}, line length {line}"));
        }
        let ypanstep = u16::from_le_bytes([fb[42], fb[43]]) as usize;
        // fb_pan_display refuses yoffset > 0 without a ypanstep that divides it (linux-6.6 fbmem.c:713).
        let halves = pan && var(YRES_VIRTUAL) as usize >= 2 * h && fix(24) >= 2 * line * h && ypanstep != 0 && h % ypanstep == 0;
        // Two windows when yres_virtual allows: drawn alternately, shown by FBIOPAN_DISPLAY (a real flip on V2 with
        // GX-3); else only the first. Starts on the first.
        let yoffset = var(YOFFSET);
        if yoffset != 0 {
            vb[YOFFSET * 4..YOFFSET * 4 + 4].copy_from_slice(&0u32.to_le_bytes());
            if let Err(e) = ioctl_buf(fd, FBIOPAN_DISPLAY, &mut vb) {
                eprintln!("bitscomp: fbdev: pan from window {yoffset} to the first refused: {e}");
            }
        }
        let map_len = line * h * if halves { 2 } else { 1 };
        if fix(24) < map_len {
            return Err(format!("{path}: {} bytes of memory for a {map_len}-byte frame", fix(24)));
        }
        // SAFETY: a fresh shared mapping of the device's first `map_len` bytes; checked for failure below.
        let map = unsafe { libc::mmap(std::ptr::null_mut(), map_len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        if map == libc::MAP_FAILED {
            return Err(format!("{path}: mmap {map_len} bytes: {}", std::io::Error::last_os_error()));
        }
        let console = vt::Console::take();
        let fb = Fbdev {
            file,
            map: map.cast(),
            map_len,
            size: (w as i32, h as i32),
            stride_px: line / 4,
            var: vb,
            flip: Flip::new(halves),
            _console: console,
        };
        evdev::start(handle)?;
        eprintln!("bitscomp: fbdev: {path} {w}x{h}, {line} bytes per line, {}", if halves { "two windows (pan)" } else { "one window" });
        Ok(fb)
    }
}

impl Fbdev {
    /// FBIOPAN_DISPLAY to window `w`; once more when a signal interrupted the wait for the flip.
    fn pan(&mut self, w: usize) -> std::io::Result<()> {
        let yoffset = (w * self.size.1 as usize) as u32;
        self.var[YOFFSET * 4..YOFFSET * 4 + 4].copy_from_slice(&yoffset.to_le_bytes());
        let fd = self.file.as_raw_fd();
        match ioctl_buf(fd, FBIOPAN_DISPLAY, &mut self.var) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => ioctl_buf(fd, FBIOPAN_DISPLAY, &mut self.var),
            r => r,
        }
    }
}

impl Drop for Fbdev {
    fn drop(&mut self) {
        // The console draws into the first window: the screen goes back there (an error changes nothing now).
        if self.flip.shown() != 0 {
            let _ = self.pan(0);
        }
        // SAFETY: our mapping from `new`, unmapped once; nothing points into it any more.
        unsafe { libc::munmap(self.map.cast(), self.map_len) };
    }
}

impl Backend for Fbdev {
    fn size(&self) -> (i32, i32) {
        self.size
    }

    /// Input arrives through the evdev sources in the event loop.
    fn dispatch(&mut self, _app: &mut App) {}

    fn present(&mut self, comp: &Compositor, damage: &[Rectangle<i32, Physical>]) {
        if comp.size() != self.size {
            return;
        }
        // SAFETY: the mapping is `map_len` bytes, page-aligned, ours until drop; only this thread writes it.
        let all = unsafe { std::slice::from_raw_parts_mut(self.map, self.map_len / 4) };
        let two = self.flip.halves();
        let (w, rects) = self.flip.frame(damage, Rectangle::from_size(self.size.into()));
        let half = self.stride_px * self.size.1 as usize;
        copy_rects(comp, &mut all[w * half..(w + 1) * half], self.stride_px, &rects);
        if !two {
            return;
        }
        // V2 with a display engine: returns once the flip landed (Linux: drm_fb_helper_pan_display commits).
        let Err(e) = self.pan(w) else {
            self.flip.landed(damage);
            return;
        };
        match self.flip.refused() {
            Refused::Retry { first: true } => {
                eprintln!("bitscomp: fbdev: pan to window {w} refused: {e}; both windows are copied whole again")
            }
            Refused::Retry { first: false } => {}
            Refused::OneWindow => {
                // Back to the first window if the device lets us, else on in the one it shows.
                let to = if self.pan(0).is_ok() { 0 } else { self.flip.shown() };
                self.flip.moved_to(to);
                eprintln!("bitscomp: fbdev: pan refused again: {e}; one window (window {to}) from now on");
            }
        }
    }

    fn draws_pointer(&self) -> bool {
        true
    }

    fn hw_cursor(&mut self) -> Option<Box<dyn crate::hwcursor::HwCursor>> {
        super::fbcursor::FbCursor::new(&self.file).map(|c| Box::new(c) as Box<dyn crate::hwcursor::HwCursor>)
    }

    fn pacing(&self) -> Option<(Duration, Duration)> {
        let mut b = [0u8; 64];
        ioctl_buf(self.file.as_raw_fd(), FBIO_BITSOS_PACING, &mut b).ok()?;
        let at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        // period_ns at 16, next_vblank_ns at 40.
        let period = at(16);
        (period > 0).then(|| (Duration::from_nanos(period), Duration::from_nanos(at(40))))
    }
}
