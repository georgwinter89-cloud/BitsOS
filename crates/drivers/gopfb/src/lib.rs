//! GOP framebuffer back end of `DisplayDevice` (owner: DP, item DP-1; spec/gpu.md §5.1).
//!
//! The firmware's linear framebuffer, mapped write-combining through MM-2's vmap (`hal::drv_dp::FbWindow`;
//! bitsboot's early mapping only as a fallback). Write-only: the picture is composed in the RAM frame
//! (`display::RamFrame`) and `present` copies the damaged rectangles row by row (`rep movsb`, then one
//! `sfence`), converting 0x00RRGGBB to the scanout layout GOP reported (BGR as is, RGB, any bit mask). No
//! vblank exists on GOP: `last_vblank` is a timer estimate (60 Hz unless a refresh is known), for pacing only;
//! tearing stays possible until GX's flips.
//!
//! DP-2: `fbdev` is `/dev/fb0` for fullscreen Linux programs on this screen (deferred io on RAM pages).
//!
//! DP-3: the exact refresh: the monitor's EDID timing for the mode (`apply_edid`, exact period to the ns) or
//! CONFIG `display_refresh=` (wins over EDID: Georg's explicit setting); the live device's clock is shared with
//! fbdev (`live_vblank`) for FBIO_WAITFORVSYNC and the pacing ioctl.
//!
//! References: Linux `drivers/video/fbdev/efifb.c` and `drivers/gpu/drm/tiny/simpledrm.c` (firmware
//! framebuffer, WC mapping), `drivers/firmware/efi/libstub/gop.c` (mask decoding); V1 `P/gop.asm` (WC map,
//! the RGB "ponytail" this fixes), `P/fbspeed.asm` (WC write 0.10-0.13 ms/MB, reads 25x slower).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod check;
pub mod fbdev;
mod ktests;
pub mod layout;
mod scene;
pub mod setup;
pub mod target;

use alloc::sync::Arc;
use alloc::vec::Vec;
use bootinfo::Framebuffer;
use core::sync::atomic::{AtomicU64, Ordering};
use alloc::string::String;
use devmodel::display::{DisplayDevice, Fence, Mode, PixelFormat, VblankEvent};
use devmodel::DevError;
use display::{RefreshSource, VblankClock};
use ksync::SpinLock;
pub use layout::{Chan, FormatError, Layout};
pub use target::{BootWindow, FbTarget, RamTarget};

/// Pixels converted per chunk on the stack (256 bytes).
const CHUNK: usize = 64;

pub struct GopFb {
    target: Arc<dyn FbTarget>,
    width: u32,
    height: u32,
    /// Pixels per scan line of the framebuffer.
    stride: u32,
    layout: Layout,
    bpp: usize,
    via: &'static str,
    seq: AtomicU64,
    vblank: Arc<VblankClock>,
    /// The kernel's GopFb (`from_bootinfo`): its presents follow a flipping display engine to the buffer shown
    /// (GX-3, `fbdev::flip`). Other GopFbs (tests, the engine's own buffers) write their target only.
    live: bool,
    /// GX-4 (CR 1): the size shown, `w << 32 | h`, at most `width` x `height` (a mode switch scales a smaller
    /// part of the same framebuffer, same stride). The live GopFb shares it with `resize_live`.
    visible: Arc<AtomicU64>,
}

/// The live GopFb's visible size (`resize_live`).
static LIVE_VISIBLE: SpinLock<Option<Arc<AtomicU64>>> = SpinLock::named("gopfb.live_visible", None);

/// GX-4 (CR 1): the kernel's GopFb shows `w` x `h` of its framebuffer from now on (a display engine scales it to
/// the monitor's timing): presents are checked against it and `current_mode` reports it. Refused (nothing
/// changes) for a size outside the framebuffer or before `from_bootinfo`.
pub fn resize_live(w: u32, h: u32) -> Result<(), String> {
    let v = LIVE_VISIBLE.lock().clone().ok_or_else(|| String::from("no live GOP framebuffer"))?;
    let (fw, fh) = LIVE.lock().as_ref().map(|(fb, _)| (fb.width, fb.height)).ok_or_else(|| String::from("no live GOP framebuffer"))?;
    if w == 0 || h == 0 || w > fw || h > fh {
        return Err(alloc::format!("{w}x{h} does not fit the {fw}x{fh} framebuffer"));
    }
    v.store(pack_size(w, h), Ordering::Release);
    klog::event!(Info, "gopfb", "visible", width = w, height = h, fb_width = fw, fb_height = fh);
    Ok(())
}

fn pack_size(w: u32, h: u32) -> u64 {
    u64::from(w) << 32 | u64::from(h)
}

/// The boot framebuffer and its window, for tests that present other layouts over the same scanout.
static LIVE: SpinLock<Option<(Framebuffer, Arc<dyn FbTarget>)>> = SpinLock::named("gopfb.live", None);

/// The framebuffer record and window the kernel's GopFb uses (None before `from_bootinfo` or headless).
pub fn live() -> Option<(Framebuffer, Arc<dyn FbTarget>)> {
    LIVE.lock().clone()
}

/// The vblank clock of the kernel's GopFb (the one `from_bootinfo` made): fbdev paces by it, with the exact
/// period and its source, which the `DisplayDevice` trait cannot pass (CR DP-1/2).
static LIVE_VBLANK: SpinLock<Option<Arc<VblankClock>>> = SpinLock::named("gopfb.live_vblank", None);

/// `DisplayDevice::name` of every GopFb.
pub const NAME: &str = "gopfb";

/// The live GopFb's clock, only while the installed display device is that GopFb: once another device (GX's
/// display engine) is the scanout, its own vblank counts and this clock is stale. `from_bootinfo` is the only
/// maker of the live clock and `display::set_device` installs one device once, so a device named "gopfb" is
/// the GopFb whose clock this is (the trait offers no identity to compare; CR DP-1/2 would make this moot).
pub fn live_vblank() -> Option<Arc<VblankClock>> {
    if display::device().is_some_and(|d| d.name() != NAME) {
        return None;
    }
    LIVE_VBLANK.lock().clone()
}

impl GopFb {
    /// A back end over `target` for a `width` x `height` scanout with `stride` pixels per line. Refuses a
    /// geometry that does not fit in the target (a forged or broken record never leads to an out-of-range
    /// copy; the copies are bounds-checked again anyway).
    pub fn new(target: Arc<dyn FbTarget>, width: u32, height: u32, stride: u32, layout: Layout, via: &'static str) -> Result<GopFb, FormatError> {
        let bpp = layout.bytes_per_pixel();
        let need = (height as u64)
            .checked_sub(1)
            .and_then(|h1| h1.checked_mul(stride as u64))
            .and_then(|v| v.checked_add(width as u64))
            .and_then(|v| v.checked_mul(bpp as u64));
        if width == 0 || stride < width || need.is_none_or(|n| n > target.len() as u64) {
            return Err(FormatError::Geometry);
        }
        Ok(GopFb {
            target,
            width,
            height,
            stride,
            layout,
            bpp,
            via,
            seq: AtomicU64::new(0),
            vblank: Arc::new(VblankClock::new(time::now_ns(), 0)),
            live: false,
            visible: Arc::new(AtomicU64::new(pack_size(width, height))),
        })
    }

    /// From the BootInfo framebuffer record; None (with an event) if there is no usable linear framebuffer.
    /// Maps the framebuffer WC through the vmap; may wait for the vmap mutex (boot thread only).
    pub fn from_bootinfo(fb: &Framebuffer) -> Option<Self> {
        let layout = match Layout::from_bootinfo(fb) {
            Ok(l) => l,
            Err(e) => {
                klog::event!(Error, "gopfb", "format.refused", format = alloc::format!("{:?}", fb.format), masks = alloc::format!("{:x?}", fb.masks),
                    error = alloc::format!("{e:?}"); "no picture: the kernel runs headless (serial + MCP)");
                return None;
            }
        };
        let (target, via): (Arc<dyn FbTarget>, &'static str) = match hal::drv_dp::FbWindow::map_wc(fb.phys, fb.size) {
            Ok(w) => (Arc::new(w), "vmap"),
            Err(e) => match hal::mmio::framebuffer(fb) {
                Some(io) => {
                    klog::event!(Warn, "gopfb", "map.fallback", phys = klog::Hex(fb.phys), size = fb.size, error = alloc::format!("{e:?}");
                        "vmap refused the WC window; drawing through bitsboot's mapping");
                    (Arc::new(BootWindow(io)), "boot")
                }
                None => {
                    klog::event!(Error, "gopfb", "map.failed", phys = klog::Hex(fb.phys), size = fb.size, error = alloc::format!("{e:?}"));
                    return None;
                }
            },
        };
        let mut dev = match GopFb::new(target.clone(), fb.width, fb.height, fb.stride, layout, via) {
            Ok(d) => d,
            Err(e) => {
                klog::event!(Error, "gopfb", "geometry.refused", width = fb.width, height = fb.height, stride = fb.stride, size = fb.size,
                    bpp = layout.bytes_per_pixel(), error = alloc::format!("{e:?}"));
                return None;
            }
        };
        dev.live = true;
        *LIVE.lock() = Some((*fb, target.clone()));
        *LIVE_VISIBLE.lock() = Some(dev.visible.clone());
        *LIVE_VBLANK.lock() = Some(dev.vblank.clone());
        dev.check_speed();
        klog::event!(Info, "gopfb", "mapped", phys = klog::Hex(fb.phys), size = fb.size, via = via, width = fb.width, height = fb.height,
            stride = fb.stride, layout = layout.name(), bpp = layout.bytes_per_pixel(), masks = alloc::format!("{:x?}", layout.masks()),
            refresh_mhz = dev.vblank.refresh_mhz());
        Some(dev)
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }
    /// The size shown (GX-4: below the framebuffer's after a mode switch).
    pub fn visible(&self) -> (u32, u32) {
        let v = self.visible.load(Ordering::Acquire);
        ((v >> 32) as u32, v as u32)
    }
    /// "vmap" (WC window), "boot" (bitsboot's mapping) or what a test passed.
    pub fn via(&self) -> &'static str {
        self.via
    }
    pub fn target(&self) -> &Arc<dyn FbTarget> {
        &self.target
    }

    /// V1's fb_measure: one timed write through the window; a far-from-WC speed is a Warn event (B6) so the
    /// desk sees it before a present is measured.
    fn check_speed(&self) {
        let Some((bytes, ns)) = setup::probe(&*self.target, &time::now_ns) else { return };
        let Some(us) = setup::us_per_mb(bytes, ns) else { return };
        if us > setup::SLOW_US_PER_MB {
            klog::event!(Warn, "gopfb", "wc.slow", via = self.via, bytes = bytes, ns = ns, us_per_mb = us, limit_us_per_mb = setup::SLOW_US_PER_MB;
                "framebuffer writes far slower than write-combining: a full present will take tens of ms");
        } else {
            klog::event!(Info, "gopfb", "wc.speed", via = self.via, bytes = bytes, ns = ns, us_per_mb = us);
        }
    }

    /// CONFIG keys of the display (the kernel passes its CONFIG lookup, as for acpi::irq::init):
    /// `display_refresh=<Hz>` (e.g. 59.95) replaces the 60 Hz vblank estimate, and an EDID rate too (an
    /// explicit setting wins: a monitor whose EDID lies can be corrected).
    pub fn apply_config(&self, config: &dyn Fn(&str) -> Option<alloc::string::String>) {
        let Some(v) = config("display_refresh") else { return };
        match setup::parse_refresh_mhz(&v) {
            Some(mhz) => {
                let before = self.vblank.source();
                self.set_refresh_mhz(mhz);
                if before == RefreshSource::Edid {
                    klog::event!(Info, "gopfb", "refresh.override", refresh_mhz = mhz; "CONFIG display_refresh replaces the EDID rate");
                }
            }
            None => klog::event!(Warn, "gopfb", "config.refused", key = "display_refresh", value = v.as_str();
                "want a rate in Hz between 20 and 1000, e.g. 59.95; keeping the estimate"),
        }
    }

    /// A known refresh rate (CONFIG, a test) replaces the 60 Hz estimate; 0 goes back to it.
    pub fn set_refresh_mhz(&self, mhz: u32) {
        self.vblank.set_refresh(time::now_ns(), mhz);
        let (refresh_mhz, period_ns, source) = self.vblank.rate();
        klog::event!(Info, "gopfb", "vblank.refresh", refresh_mhz = refresh_mhz, period_ns = period_ns, source = source.name());
    }

    /// The monitor's EDID (base block and extensions as the firmware's EDID protocol gave them): the detailed
    /// timing of the scanout's resolution sets the exact period (`htotal * vtotal / clock`). False (with an
    /// event) when the EDID is broken or has no timing for this mode; the estimate stays as it was.
    /// An explicit CONFIG rate stays (EDID never replaces it, whatever order a boot or Reload applies them in);
    /// a resolution the EDID lists at several rates gives none (GOP does not say which one it set).
    pub fn apply_edid(&self, edid: &[u8]) -> bool {
        if self.vblank.source() == RefreshSource::Config {
            klog::event!(Info, "gopfb", "edid.ignored", reason = "config", refresh_mhz = self.vblank.refresh_mhz(); "CONFIG display_refresh stays over the EDID rate");
            return false;
        }
        match display::edid::timing_for(edid, self.width, self.height) {
            Ok(t) => {
                let (mhz, period) = (t.refresh_mhz(), t.period_ns());
                // A rate outside CONFIG's range (20..1000 Hz) is a broken descriptor, not a monitor.
                if !(20_000..=1_000_000).contains(&mhz) {
                    klog::event!(Warn, "gopfb", "edid.refused", reason = "rate", refresh_mhz = mhz, clock_hz = t.clock_hz, htotal = t.htotal, vtotal = t.vtotal);
                    return false;
                }
                self.vblank.set_period(time::now_ns(), mhz, period, RefreshSource::Edid);
                klog::event!(Info, "gopfb", "vblank.refresh", refresh_mhz = mhz, period_ns = period, source = "edid", clock_hz = t.clock_hz,
                    htotal = t.htotal, vtotal = t.vtotal);
                true
            }
            Err(display::edid::EdidError::Ambiguous) => {
                let rates: Vec<u32> = display::edid::matches(edid, self.width, self.height).map(|m| m.iter().map(|t| t.refresh_mhz()).collect()).unwrap_or_default();
                klog::event!(Warn, "gopfb", "edid.ambiguous", width = self.width, height = self.height, rates_mhz = alloc::format!("{rates:?}");
                    "the EDID lists this resolution at several rates and GOP does not say which one runs; keeping the estimate (set CONFIG display_refresh=)");
                false
            }
            Err(e) => {
                klog::event!(Warn, "gopfb", "edid.refused", reason = alloc::format!("{e:?}"), bytes = edid.len(), width = self.width, height = self.height;
                    "no exact refresh from the EDID; keeping the estimate");
                false
            }
        }
    }

    /// Rate (mHz), period (ns) and where they came from.
    pub fn vblank_rate(&self) -> (u32, u64, RefreshSource) {
        self.vblank.rate()
    }
    /// The next estimated vblank.
    pub fn next_vblank(&self) -> VblankEvent {
        self.vblank.next(time::now_ns())
    }
    pub fn vblank_period_ns(&self) -> u64 {
        self.vblank.period_ns()
    }
    /// Sleeps until the next estimated vblank; None if that is further away than `timeout_ns`.
    pub fn wait_vblank(&self, timeout_ns: u64) -> Option<VblankEvent> {
        let now = time::now_ns();
        let next = self.vblank.next(now);
        let dt = next.time_ns.saturating_sub(now);
        if dt > timeout_ns {
            return None;
        }
        sched::sleep_ns(dt);
        Some(next)
    }

    /// Writes rows `y..y+h`, columns `x..x+w` of `src` (stride `src_stride`), already range-checked.
    fn copy_rect(&self, src: &[u32], src_stride: usize, x: usize, y: usize, w: usize, h: usize) -> bool {
        let mut words = [0u32; CHUNK];
        let mut bytes = [0u8; CHUNK * 4];
        for row in y..y + h {
            let s = row * src_stride + x;
            let line = &src[s..s + w];
            let mut off = (row * self.stride as usize + x) * self.bpp;
            let ok = match self.layout {
                Layout::Bgrx => self.target.write_pixels(off, line),
                l if self.bpp == 4 => line.chunks(CHUNK).all(|c| {
                    for (d, &p) in words.iter_mut().zip(c) {
                        *d = l.encode(p);
                    }
                    let r = self.target.write_pixels(off, &words[..c.len()]);
                    off += c.len() * 4;
                    r
                }),
                l => line.chunks(CHUNK).all(|c| {
                    let n = c.len() * self.bpp;
                    for (d, &p) in bytes.chunks_exact_mut(self.bpp).zip(c) {
                        d.copy_from_slice(&l.encode(p).to_le_bytes()[..self.bpp]);
                    }
                    let r = self.target.write_bytes(off, &bytes[..n]);
                    off += n;
                    r
                }),
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

impl DisplayDevice for GopFb {
    fn name(&self) -> &str {
        NAME
    }
    fn modes(&self) -> Vec<Mode> {
        alloc::vec![self.current_mode()]
    }
    fn current_mode(&self) -> Mode {
        let (mhz, _, source) = self.vblank.rate();
        let refresh_mhz = if source == RefreshSource::Default { 0 } else { mhz };
        let (width, height) = self.visible();
        Mode { width, height, refresh_mhz }
    }
    /// The scanout layout; a bit-mask layout reports Bgrx8888 until devmodel can name it (change request
    /// DP-1/1); `GopFb::layout` has the exact one.
    fn format(&self) -> PixelFormat {
        match self.layout {
            Layout::Rgbx => PixelFormat::Rgbx8888,
            _ => PixelFormat::Bgrx8888,
        }
    }
    /// Copies the rectangle `(x, y, w, h)` of the full frame `src` (`stride` pixels per line, 0x00RRGGBB) to
    /// the screen. Refuses (InvalidArgument, nothing written) a rectangle outside the screen or the source.
    fn present(&self, src: &[u32], stride: u32, damage: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
        let (x, y, w, h) = (damage.0 as u64, damage.1 as u64, damage.2 as u64, damage.3 as u64);
        let (vw, vh) = self.visible();
        if x + w > vw as u64 || y + h > vh as u64 || x + w > stride as u64 {
            return Err(DevError::InvalidArgument);
        }
        if w > 0 && h > 0 {
            let last = (y + h - 1) * stride as u64 + x + w;
            if last > src.len() as u64 {
                return Err(DevError::InvalidArgument);
            }
            if self.live
                && let Some(r) = fbdev::flip::present_shown(src, stride, damage)
            {
                return r;
            }
            if !self.copy_rect(src, stride as usize, x as usize, y as usize, w as usize, h as usize) {
                return Err(DevError::InvalidArgument);
            }
            self.target.flush();
        }
        Ok(Fence { seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1, signalled: true })
    }
    fn last_vblank(&self) -> VblankEvent {
        self.vblank.last(time::now_ns())
    }
}

#[cfg(test)]
mod visible_tests {
    use super::*;

    /// GX-4 CR 1: a GopFb that shows a smaller part of its framebuffer reports it as its mode and refuses presents
    /// outside it (same stride, same target).
    #[test]
    fn a_smaller_visible_size_bounds_the_mode_and_the_presents() {
        let t = Arc::new(RamTarget::new(64 * 48 * 4, 0));
        let fb = GopFb::new(t, 64, 48, 64, Layout::Bgrx, "test").unwrap();
        assert_eq!(fb.visible(), (64, 48));
        let src = alloc::vec![0u32; 64 * 48];
        assert!(fb.present(&src, 64, (0, 0, 64, 48)).is_ok());
        fb.visible.store(pack_size(32, 24), Ordering::Release);
        let m = fb.current_mode();
        assert_eq!((m.width, m.height), (32, 24));
        assert!(fb.present(&src, 64, (0, 0, 32, 24)).is_ok());
        assert_eq!(fb.present(&src, 64, (0, 0, 33, 24)).err(), Some(DevError::InvalidArgument));
        assert_eq!(fb.present(&src, 64, (0, 20, 8, 8)).err(), Some(DevError::InvalidArgument));
    }
}
