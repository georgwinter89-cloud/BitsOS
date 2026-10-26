//! GX-3 kernel glue: the scanout pair of the attached display (`scanout::Scanout` over `DcnRegs`, the kernel
//! clock, the live GOP framebuffer's window for A0 and a new write-combining window for A1), taken over when the
//! PCI binder probes the 780M's display function (boot thread, after the scan, no lock held: the attach reads PCI
//! configuration and a settle may sleep up to 50 ms), installed into gopfb's fbdev (`gopfb::fbdev::flip`) by an
//! initcall, and its Quiesce (Reload/reboot/panic: A0 back if GX flipped). QEMU has no such function: nothing
//! binds, fbdev keeps copying.

use crate::attach::{self, BACK_OFFSET, Regs};
use crate::cursor::{self, CursorError, Shape};
use crate::dcn::{DcnError, DcnRegs};
use crate::flip::Rw;
use crate::gpu::Gpu;
use crate::layout as l;
use crate::scanout::{self, Clock, Scanout, Timing};
use alloc::string::String;
use alloc::sync::Arc;
use devmodel::DevError;
use devmodel::display::{DisplayDevice, Fence};
use gopfb::GopFb;
use gopfb::fbdev::flip::{CursorError as FbCursorError, FlipError, ScanoutFlip, Vblank};
use ksync::Once;

/// The register owner as the flip sequences see it.
pub struct Live(pub &'static DcnRegs);

impl Regs for Live {
    fn read(&self, byte: u32) -> Option<u32> {
        attach::Regs::read(self.0, byte)
    }
}

impl Rw for Live {
    fn write(&self, byte: u32, value: u32, check: u32) -> Result<(), DcnError> {
        self.0.write_masked(byte, value, check)
    }
}

pub struct KClock;

impl Clock for KClock {
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        sched::sleep_ns(ns);
    }
}

pub type KScanout = Scanout<Live, KClock>;

static SCANOUT: Once<Option<KScanout>> = Once::new();

/// The scanout pair if it was taken over (never builds; any context).
pub fn get() -> Option<&'static KScanout> {
    SCANOUT.get().and_then(Option::as_ref)
}

/// Takes the display over on first use. Thread context with no lock held, after the PCI scan (the binder's
/// probe; the MCP tool and the ktests call it too, later).
pub fn attach() -> Option<&'static KScanout> {
    SCANOUT.get_or_init(build).as_ref()
}

fn refuse(why: &str) -> Option<KScanout> {
    klog::event!(Info, "dcn", "flip.off", why = why; "no page flips: the display keeps the copy path");
    None
}

fn build() -> Option<KScanout> {
    let g = Gpu::get()?;
    let Ok(a) = g.display.as_ref() else {
        return refuse("display attach refused (see dcn display.refused)");
    };
    let Some((fb, target)) = gopfb::live() else {
        return refuse("no live GOP framebuffer window");
    };
    if fb.phys.checked_sub(a.bar0) != Some(a.a0_offset) {
        return refuse("the GOP framebuffer is not the attach's A0");
    }
    let Ok(layout) = gopfb::Layout::from_bootinfo(&fb) else {
        return refuse("GOP pixel layout refused");
    };
    let Ok(a0) = GopFb::new(target, fb.width, fb.height, fb.stride, layout, "dcn.a0") else {
        return refuse("A0 geometry does not fit its window");
    };
    let w1 = match hal::drv_dp::FbWindow::map_wc(fb.phys + BACK_OFFSET, a.bytes) {
        Ok(w) => w,
        Err(e) => return refuse(&alloc::format!("A1 window: {e:?}")),
    };
    let Ok(a1) = GopFb::new(Arc::new(w1), fb.width, fb.height, fb.stride, layout, "dcn.a1") else {
        return refuse("A1 geometry does not fit its window");
    };
    if let Some(bos) = &g.bos {
        let off = a.a1 - a.fb_first;
        if let Err(e) = bos.lock().create_at(off..off + a.bytes) {
            return refuse(&alloc::format!("A1 is taken in graphics memory: {e:?}"));
        }
    }
    let Some(timing) = timing(g.regs, a) else {
        return refuse("no OTG timing (pixel clock, totals) for the vblank clock");
    };
    match Scanout::new(Live(g.regs), KClock, a, a0, a1, timing) {
        Ok(mut s) => {
            cursor_buffer(g, a, &mut s);
            match s.cursor_clear_found() {
                Ok(true) => klog::event!(Warn, "dcn", "cursor.found_on"; "a cursor left on by an earlier kernel is off"),
                Ok(false) => {}
                Err(e) => klog::event!(Warn, "dcn", "cursor.found_on_failed", error = alloc::format!("{e:?}")),
            }
            devmodel::register_quiesce(Arc::new(FlipQuiesce));
            klog::event!(Info, "dcn", "flip.ready", a0 = a.a0, a1 = a.a1, bytes = a.bytes, pipes = a.pipes.len(),
                otg = s.otg(), period_ns = timing.period_ns(), refresh_mhz = timing.refresh_mhz();
                "page flips between A0 and A1 (fbdev frames go to the hidden buffer)");
            Some(s)
        }
        Err(e) => refuse(&alloc::format!("{e:?}")),
    }
}

/// GX-5a: the cursor surfaces, a VRAM BO the HUBP fetches like the scanout buffers (system RAM is out: the
/// firmware leaves bus mastering off on the PCIe ports, CONTRACTS 6b), mapped write-combining like A1. First
/// choice right after A1 (the range the gates checked, next to memory the Mini-PC showed unused), else the
/// first free range the CPU reaches. Without it the cursor answers NoDevice and the compositor draws its own.
fn cursor_buffer(g: &Gpu, a: &attach::Attach, s: &mut KScanout) {
    let Some(bos) = &g.bos else { return };
    let limit = a.reach.min(u64::from(a.aper_mb) << 20);
    let after_a1 = (a.a1 - a.fb_first + a.bytes).next_multiple_of(64 << 10);
    let off = {
        let mut b = bos.lock();
        let h = match b.create_at(after_a1..after_a1 + cursor::BYTES) {
            Ok(h) if after_a1 + cursor::BYTES <= limit => Ok(h),
            Ok(h) => {
                b.release(h);
                b.create(cursor::BYTES, crate::bo::Placement::Vram, 64 << 10, Some(limit))
            }
            Err(_) => b.create(cursor::BYTES, crate::bo::Placement::Vram, 64 << 10, Some(limit)),
        };
        match h.map(|h| b.get(h).map(|bo| bo.range.start)) {
            Ok(Some(off)) => off,
            Ok(None) | Err(_) => {
                klog::event!(Warn, "dcn", "cursor.off", why = "no graphics memory for the cursor surfaces");
                return;
            }
        }
    };
    let w = match hal::drv_dp::FbWindow::map_wc(a.bar0 + off, cursor::BYTES) {
        Ok(w) => w,
        Err(e) => {
            klog::event!(Warn, "dcn", "cursor.off", why = alloc::format!("cursor window: {e:?}"));
            return;
        }
    };
    match cursor::Buf::new(Arc::new(w), a.fb_first + off) {
        Some(b) => {
            klog::event!(Info, "dcn", "cursor.ready", offset = off, gpu = a.fb_first + off, bytes = cursor::BYTES;
                "hardware cursor surfaces in graphics memory");
            s.set_cursor_buffer(b);
        }
        None => klog::event!(Warn, "dcn", "cursor.off", why = "cursor window smaller than its surfaces"),
    }
}

/// GX-5a: the hardware cursor for gopfb's fbdev (`FBIO_BITSOS_CURSOR`, change request in plan/notes/gx-GX-5a.md)
/// and the `dcn_cursor` tool. NoDevice without a display engine taken over (QEMU).
pub fn cursor_ready() -> bool {
    get().is_some_and(|s| s.cursor_ready())
}
pub fn cursor_image(shape: Shape, px: &[u32], tip: (i32, i32)) -> Result<(), CursorError> {
    get().ok_or(CursorError::NoDevice("no display engine taken over"))?.cursor_image(shape, px, tip)
}
pub fn cursor_move(tip: (i32, i32)) -> Result<(), CursorError> {
    get().ok_or(CursorError::NoDevice("no display engine taken over"))?.cursor_move(tip)
}
pub fn cursor_hide() -> Result<(), CursorError> {
    get().ok_or(CursorError::NoDevice("no display engine taken over"))?.cursor_hide()
}

/// The first pipe's OTG timing (KMS view) and its vertical blank start.
fn timing(r: &DcnRegs, a: &attach::Attach) -> Option<Timing> {
    let k = a.pipes.first()?.otg;
    let c = crate::kms::read(r, false).crtcs.into_iter().find(|c| c.id == k)?;
    let vb = attach::Regs::read(r, l::otg(u32::from(k), l::OTG_V_BLANK))?;
    Timing::new(c.h_total, c.v_total, c.pixel_hz?, (vb & l::V_BLANK_START) >> l::V_BLANK_START_SHIFT)
}

struct FlipQuiesce;

impl devmodel::Quiesce for FlipQuiesce {
    fn name(&self) -> &str {
        "amdgpu.flip"
    }
    fn quiesce(&self) {
        if let Some(s) = get() {
            let _ = s.restore();
        }
    }
}

/// gopfb's view (`gopfb::fbdev::flip`).
struct Provider;

impl ScanoutFlip for Provider {
    fn ready(&self) -> bool {
        get().is_some_and(|s| s.active())
    }
    fn active(&self) -> bool {
        get().is_some_and(|s| s.active())
    }
    fn redirects(&self) -> bool {
        get().is_some_and(|s| s.redirects())
    }
    fn present_shown(&self, src: &[u32], stride: u32, rect: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
        get().ok_or(DevError::Unsupported)?.present_shown(src, stride, rect)
    }
    fn begin(&self, src: &[u32], stride: u32) -> Result<&dyn DisplayDevice, FlipError> {
        let d: &dyn DisplayDevice = get().ok_or(FlipError::Off)?.begin(src, stride)?;
        Ok(d)
    }
    fn commit(&self) -> Result<u64, FlipError> {
        get().ok_or(FlipError::Off)?.commit()
    }
    fn wait(&self, seq: u64) -> Result<(), FlipError> {
        get().ok_or(FlipError::Off)?.wait(seq)
    }
    fn vblank(&self) -> Option<Vblank> {
        get().filter(|s| s.active())?.vblank()
    }
    fn rearm(&self) {
        if let Some(s) = get() {
            s.rearm();
        }
    }
    fn cursor_ready(&self) -> bool {
        cursor_ready()
    }
    fn cursor_image(&self, w: u32, h: u32, hot: (u32, u32), straight: bool, px: &[u32], at: (i32, i32)) -> Result<(), FbCursorError> {
        let alpha = if straight { cursor::Alpha::Straight } else { cursor::Alpha::Premultiplied };
        let shape = Shape::new(w, h, hot.0, hot.1, alpha).map_err(fb_cursor_error)?;
        cursor_image(shape, px, at).map_err(fb_cursor_error)
    }
    fn cursor_move(&self, at: (i32, i32)) -> Result<(), FbCursorError> {
        cursor_move(at).map_err(fb_cursor_error)
    }
    fn cursor_hide(&self) -> Result<(), FbCursorError> {
        cursor_hide().map_err(fb_cursor_error)
    }
}

fn fb_cursor_error(e: CursorError) -> FbCursorError {
    match e {
        CursorError::NoDevice(_) => FbCursorError::NoDevice,
        CursorError::Invalid(_) => FbCursorError::Invalid,
        CursorError::Busy => FbCursorError::Busy,
        CursorError::Reg(_) => FbCursorError::Failed,
    }
}

static PROVIDER: Provider = Provider;

fn install() -> Result<(), &'static str> {
    if gopfb::fbdev::flip::install(&PROVIDER) { Ok(()) } else { Err("another flip engine is installed") }
}
kreg::initcall!(kreg::Level::Device, "amdgpu.flip", install);

/// The 780M's display function (Phoenix 1002:15BF, Phoenix 2 1002:1900; `ip::known_blocks`). No class match:
/// other display functions (QEMU's VGA) never reach this probe.
const MATCHES: [pci::Match; 2] =
    [pci::Match::Id { vendor: 0x1002, device: 0x15BF }, pci::Match::Id { vendor: 0x1002, device: 0x1900 }];

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    if f.class_code() != 0x03 {
        return Err(String::from("not a display function"));
    }
    match attach() {
        Some(_) => Ok(()),
        None => Err(String::from("display not taken over (see the dcn flip.off event)")),
    }
}
pci::pci_driver!(pci::DriverEntry { name: "amdgpu", matches: &MATCHES, opt_in_key: None, probe });

/// The scanout's state as one line (MCP tool, ktests).
pub fn state_line() -> String {
    match get() {
        None => String::from("flip: off (no display engine taken over)"),
        Some(s) => {
            let st = s.state();
            let t = s.timing();
            alloc::format!(
                "flip: {}, shows A{}, ordered {}, landed {}, late {}, settled at take-over {}, OTG{} {} lines of {} ps, {} mHz",
                if st.flipping { "on" } else { "fell back (one image)" },
                st.shown,
                st.ordered,
                st.landed,
                st.late,
                st.settled,
                s.otg(),
                t.v_total,
                t.line_ps,
                t.refresh_mhz()
            )
        }
    }
}

pub use scanout::reg_error_text;
