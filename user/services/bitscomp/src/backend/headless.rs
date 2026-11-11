//! No window: the frame goes into a memory buffer the size of a framebuffer mapping.
//!
//! WY-17 `--fake-scanout ok|badfd|nostride|refuse`, the host's stand-in for fbdev's windows (`Fake`): two memfds that
//! llvmpipe imports like dma-bufs. `badfd` hands out what does not map, `nostride` what EGL refuses (a zero pitch),
//! `refuse` answers the third flip with a refusal. WY-20 `vsync` is `ok` with a screen behind it: a 60 Hz grid the
//! back end names through `pacing`, and each flip returns once it landed, just past the next vblank, as V2's
//! FBIOPAN_DISPLAY does (`AFTER_VBLANK` below; the kernel's direct_flip waits so).

use std::time::Duration;

use smithay::utils::{Physical, Rectangle};

use super::flip::Flip;
use super::{Backend, copy_rects, scanout};
use crate::render::Picture;
use crate::state::App;

pub struct Headless {
    size: (i32, i32),
    fb: Vec<u32>,
    /// WY-11 `--pointer none|soft|fake`: no pointer (the reference pictures), the software pointer (fbdev's
    /// ENODEV path), or a fake hardware cursor that records its calls.
    pointer: String,
    fake: Option<Fake>,
}

/// WY-17: the host's stand-in for the two windows and the flips.
struct Fake {
    kind: String,
    on: bool,
    left: bool,
    flip: Flip,
    flips: u32,
}

impl Fake {
    fn windows(&self, size: (i32, i32)) -> Result<scanout::Windows, String> {
        let stride = size.0 as u32 * 4;
        let mut list = Vec::new();
        for i in 0..2 {
            let fd = if self.kind == "badfd" {
                std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?.into()
            } else {
                let name = format!("bitscomp-fake-window-{i}");
                let f = memfd(&name)?;
                f.set_len(u64::from(stride) * size.1 as u64).map_err(|e| format!("{name}: {e}"))?;
                f.into()
            };
            list.push(scanout::Window { fd, offset: 0, stride: if self.kind == "nostride" { 0 } else { stride } });
        }
        Ok(scanout::Windows { list, size, fourcc: scanout::XRGB8888, engine: false })
    }
}

/// The simulated refresh of `--fake-scanout vsync`, on the grid `pacing` names (phase zero).
const VSYNC_PERIOD: Duration = Duration::from_nanos(16_666_667);
/// How far past the vblank a pan returns on V2: amdgpu's `AFTER_VBLANK_NS` (scanout.rs).
const AFTER_VBLANK: Duration = Duration::from_micros(500);

/// A flip ordered at `now` lands at the next vblank and its pan returns just past that.
fn pan_returns(now: Duration) -> Duration {
    crate::pacer::on_grid(now + Duration::from_nanos(1), Duration::ZERO, VSYNC_PERIOD) + AFTER_VBLANK
}

fn memfd(name: &str) -> Result<std::fs::File, String> {
    use std::os::fd::FromRawFd;
    let c = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: `c` is a NUL-terminated name alive for the call; a fresh fd is ours to wrap.
    let fd = unsafe { libc::memfd_create(c.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just made and nothing else owns it.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

impl Headless {
    pub fn new(size: (i32, i32), pointer: &str, fake_scanout: Option<&str>) -> Self {
        let fake = fake_scanout.map(|kind| Fake { kind: kind.to_string(), on: false, left: false, flip: Flip::new(true), flips: 0 });
        Self { size, fb: vec![0; (size.0 * size.1) as usize], pointer: pointer.to_string(), fake }
    }
}

impl Backend for Headless {
    fn size(&self) -> (i32, i32) {
        self.size
    }

    fn dispatch(&mut self, app: &mut App) {
        if let Some(size) = app.requested_size.take() {
            self.size = size;
            self.fb = vec![0; (size.0 * size.1) as usize];
            self.scanout_lost("the mode changed");
            app.set_mode(size);
        }
    }

    fn draws_pointer(&self) -> bool {
        self.pointer != "none"
    }

    fn hw_cursor(&mut self) -> Option<Box<dyn crate::hwcursor::HwCursor>> {
        (self.pointer == "fake").then(|| Box::new(crate::hwcursor::Fake::default()) as Box<dyn crate::hwcursor::HwCursor>)
    }

    fn pacing(&self) -> Option<(Duration, Duration)> {
        self.fake.as_ref().filter(|f| f.kind == "vsync").map(|_| (VSYNC_PERIOD, Duration::ZERO))
    }

    fn scanout_windows(&mut self) -> Result<scanout::Windows, String> {
        let size = self.size;
        self.fake.as_ref().ok_or("this back end has no scanout buffers".to_string())?.windows(size)
    }

    fn scanout_on(&mut self) {
        if let Some(f) = &mut self.fake {
            f.on = true;
        }
    }

    fn scanout_frame(&mut self) -> Option<scanout::Aim> {
        let f = self.fake.as_mut().filter(|f| f.on)?;
        let next = f.flip.back();
        let (window, age) = f.flip.aim(next, false);
        Some(scanout::Aim { window, age })
    }

    fn scanout_lost(&mut self, why: &str) {
        if let Some(f) = self.fake.as_mut().filter(|f| f.on) {
            eprintln!("bitscomp: scanout: back to readback: {why}");
            f.on = false;
            f.left = true;
        }
    }

    fn scanout_left(&mut self) -> bool {
        self.fake.as_mut().is_some_and(|f| std::mem::take(&mut f.left))
    }

    fn present(&mut self, comp: &dyn Picture, damage: &[Rectangle<i32, Physical>]) {
        if let Some(f) = self.fake.as_mut().filter(|f| f.on) {
            f.flips += 1;
            if f.kind == "refuse" && f.flips == 3 {
                self.scanout_lost("pan to the window refused: the fake refuses its third flip");
            } else {
                if f.kind == "vsync" {
                    let now = crate::pacer::now();
                    std::thread::sleep(pan_returns(now).saturating_sub(now));
                }
                f.flip.landed(&[]);
            }
            return;
        }
        if comp.size() == self.size {
            copy_rects(comp, &mut self.fb, self.size.0 as usize, damage);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pan_returns_just_past_the_next_vblank() {
        let (p, ms) = (VSYNC_PERIOD, Duration::from_millis);
        assert_eq!(pan_returns(ms(1)), p + AFTER_VBLANK);
        assert_eq!(pan_returns(p), p * 2 + AFTER_VBLANK, "ordered on a vblank: the next one");
        assert_eq!(pan_returns(p * 2 + AFTER_VBLANK), p * 3 + AFTER_VBLANK);
    }
}
