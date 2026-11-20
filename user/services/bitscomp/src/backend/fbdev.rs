//! V2's output (WY-1a): `/dev/fb0` (linux-7.3.1/drivers/video/fbdev/core/fbmem.c; V2 crates/drivers/gopfb/src/
//! fbdev). FBIOGET_VSCREENINFO/FSCREENINFO give size, stride and format (32-bit XRGB is the only one V2 has). The
//! renderer draws every frame straight into fb0's two windows (WY-17, `scanout`): with a display engine they are its
//! scanout buffers, FBIOPAN_DISPLAY flips to the one just drawn and returns once it landed; without one they are fb0's
//! RAM and the kernel copies the reported rectangles into its frame (FBIO_BITSOS_DAMAGE, DP-6). The bookkeeping of
//! which window is shown is `flip::Flip`. Pacing: FBIO_BITSOS_PACING (V2 only) gives the scanout's period and vblank
//! grid for the frame timer.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Duration;

use smithay::reexports::calloop::LoopHandle;
use smithay::utils::{Physical, Rectangle};

use super::flip::Flip;
use super::{evdev, scanout, vt};
use crate::state::App;

const FBIOGET_VSCREENINFO: u64 = 0x4600;
const FBIOGET_FSCREENINFO: u64 = 0x4602;
const FBIOPAN_DISPLAY: u64 = 0x4606;
/// V2's `_IOR('F', 0xB0, struct bitsos_fb_pacing)` (64 bytes; gopfb fbdev/pacing.rs).
const FBIO_BITSOS_PACING: u64 = 0x8040_46B0;
/// V2's `_IOW('F', 0xB3, struct bitsos_fb_damage)` (24 bytes; gopfb fbdev/damage.rs, DP-6): the rectangles of the
/// window just drawn, so a kernel without a display engine copies only those into its frame.
const FBIO_BITSOS_DAMAGE: u64 = 0x4018_46B3;
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
    size: (i32, i32),
    /// GX-3 CR 2: `var` is the screeninfo to pan with, `flip` which window is drawn and shown.
    var: [u8; VAR_WORDS * 4],
    flip: Flip,
    /// WY-17: the device has room for two windows.
    two_fit: bool,
    /// WY-17: the renderer draws into the windows of this hold; `left`: they ended since the main loop last asked.
    direct: bool,
    left: bool,
    /// DP-6: the kernel takes damage reports (false after the first refusal: ENOTTY on Linux, then never again).
    damage: bool,
    /// Dropped last (fields drop in order after `Drop::drop`): the console comes back last.
    _console: vt::Console,
}

fn ioctl_buf(fd: i32, req: u64, buf: &mut [u8]) -> std::io::Result<()> {
    // SAFETY: `buf` is valid for writes of the size the request encodes (or the fixed struct size it names).
    let r = unsafe { libc::ioctl(fd, req as libc::Ioctl, buf.as_mut_ptr()) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

impl Fbdev {
    /// Opens `path` (normally /dev/fb0), takes the console's screen and starts the evdev input.
    pub fn new(path: &str, handle: &LoopHandle<'static, App>) -> Result<Fbdev, String> {
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
        // fb_pan_display refuses yoffset > 0 without a ypanstep that divides it
        // (linux-7.3.1/drivers/video/fbdev/core/fbmem.c:180).
        let two_fit = var(YRES_VIRTUAL) as usize >= 2 * h && fix(24) >= 2 * line * h && ypanstep != 0 && h % ypanstep == 0;
        let yoffset = var(YOFFSET);
        if yoffset != 0 {
            vb[YOFFSET * 4..YOFFSET * 4 + 4].copy_from_slice(&0u32.to_le_bytes());
            if let Err(e) = ioctl_buf(fd, FBIOPAN_DISPLAY, &mut vb) {
                eprintln!("bitscomp: fbdev: pan from window {yoffset} to the first refused: {e}");
            }
        }
        let console = vt::Console::take();
        let mut fb = Fbdev { file, size: (w as i32, h as i32), var: vb, flip: Flip::new(), two_fit, direct: false, left: false, damage: true, _console: console };
        // DP-6: reports on before the first frame, so no page of fb0 faults more than once.
        fb.report(0, &[]);
        evdev::start(handle)?;
        eprintln!("bitscomp: fbdev: {path} {w}x{h}, {line} bytes per line");
        Ok(fb)
    }

    pub fn size(&self) -> (i32, i32) {
        self.size
    }

    /// DP-6: tells the kernel which rectangles of window `w` were just drawn (FBIO_BITSOS_DAMAGE before the pan).
    fn report(&mut self, w: usize, rects: &[Rectangle<i32, Physical>]) {
        if !self.damage {
            return;
        }
        let (sw, sh) = self.size;
        let mut list: Vec<[u32; 4]> = Vec::with_capacity(rects.len());
        for r in rects {
            let x0 = r.loc.x.clamp(0, sw);
            let y0 = r.loc.y.clamp(0, sh);
            let x1 = (r.loc.x + r.size.w).clamp(0, sw);
            let y1 = (r.loc.y + r.size.h).clamp(0, sh);
            if x1 > x0 && y1 > y0 {
                list.push([x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32]);
            }
        }
        let mut b = [0u8; 24];
        b[4..8].copy_from_slice(&((w * self.size.1 as usize) as u32).to_le_bytes());
        b[8..12].copy_from_slice(&(list.len() as u32).to_le_bytes());
        b[16..24].copy_from_slice(&(list.as_ptr() as u64).to_le_bytes());
        if let Err(e) = ioctl_buf(self.file.as_raw_fd(), FBIO_BITSOS_DAMAGE, &mut b) {
            self.damage = false;
            if e.raw_os_error() != Some(libc::ENOTTY) {
                eprintln!("bitscomp: fbdev: damage report refused: {e}; the kernel tracks the stores itself from now on");
            }
        }
    }

    /// WY-17: the windows of this hold are over (a refused pan, a screen that went back, a lost GPU): said once, and
    /// the main loop takes new ones (`scanout::start`) before the next frame.
    pub fn end_direct(&mut self, why: &str) {
        if !self.direct {
            return;
        }
        eprintln!("bitscomp: scanout: the windows end: {why}");
        match scanout::end(self.file.as_raw_fd()) {
            Ok(()) => {}
            // The screen went back: the hold's end put the view home.
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            // It went back and a pan took it again (nothing exported any more): the view is where that pan left it.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                let _ = self.pan(0);
            }
            Err(e) => eprintln!("bitscomp: scanout: end refused: {e}"),
        }
        self.direct = false;
        self.left = true;
        self.flip = Flip::new();
    }

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

    /// The frame is drawn and finished in the hidden window: the flip (without an engine the kernel copies the
    /// rectangles into its frame).
    pub fn present(&mut self, damage: &[Rectangle<i32, Physical>]) {
        if !self.direct {
            return;
        }
        let w = self.flip.back();
        self.report(w, damage);
        match self.pan(w) {
            Ok(()) => self.flip.landed(),
            Err(e) => self.end_direct(&format!("pan to window {w} refused: {e}")),
        }
    }

    pub fn hw_cursor(&mut self) -> Option<Box<dyn crate::hwcursor::HwCursor>> {
        super::fbcursor::FbCursor::new(&self.file).map(|c| Box::new(c) as Box<dyn crate::hwcursor::HwCursor>)
    }

    /// WY-17: the output's two windows as dma-bufs for the renderer to draw into; why not.
    pub fn scanout_windows(&mut self) -> Result<scanout::Windows, String> {
        if !self.two_fit {
            return Err("fb0 has room for one window".into());
        }
        scanout::export(self.file.as_raw_fd(), self.size)
    }

    /// WY-17: the renderer took them: from now on `present` flips.
    pub fn scanout_on(&mut self) {
        self.direct = true;
        self.flip = Flip::new();
    }

    /// WY-17, before a frame: the window the renderer draws it into (waits for the last flip). None: the windows of this
    /// hold ended (`end_direct`).
    pub fn scanout_frame(&mut self) -> Option<scanout::Aim> {
        if !self.direct {
            return None;
        }
        match scanout::begin(self.file.as_raw_fd()) {
            Ok((w, whole)) => {
                let (window, age) = self.flip.aim(w, whole);
                Some(scanout::Aim { window, age })
            }
            Err(e) => {
                self.end_direct(&format!("the kernel names no window: {e}"));
                None
            }
        }
    }

    /// WY-17, after a frame: the windows ended just now, so new ones are taken and the picture drawn again whole.
    pub fn scanout_left(&mut self) -> bool {
        std::mem::take(&mut self.left)
    }

    /// The scanout's frame period and one vblank time on CLOCK_MONOTONIC, when the output knows them.
    pub fn pacing(&self) -> Option<(Duration, Duration)> {
        let mut b = [0u8; 64];
        ioctl_buf(self.file.as_raw_fd(), FBIO_BITSOS_PACING, &mut b).ok()?;
        let at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        // period_ns at 16, next_vblank_ns at 40.
        let period = at(16);
        (period > 0).then(|| (Duration::from_nanos(period), Duration::from_nanos(at(40))))
    }
}

impl Drop for Fbdev {
    fn drop(&mut self) {
        // WY-17: the scanout buffers are the picture until the console draws: no pan to a window fb0 never held.
        if self.direct {
            let _ = scanout::end(self.file.as_raw_fd());
        }
        // The console draws into the first window: the screen goes back there (an error changes nothing now).
        else if self.flip.shown() != 0 {
            let _ = self.pan(0);
        }
    }
}
