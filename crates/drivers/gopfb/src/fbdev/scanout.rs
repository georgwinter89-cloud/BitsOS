//! WY-17 (gx-scout-mesa.md C3): the compositor draws straight into what the screen shows. One BitsOS ioctl on
//! /dev/fb0 in DP-3's private 'F' range (ENOTTY on Linux, where bitscomp reads back as before):
//!
//! ```c
//! struct bitsos_fb_scanout {
//!     __u32 op;       /* 1 EXPORT, 2 BEGIN, 3 END */
//!     __u32 window;   /* EXPORT: in, 0 or 1. BEGIN: out, the window to draw next */
//!     __s32 fd;       /* EXPORT: out, a close-on-exec dma-buf */
//!     __u32 flags;    /* out: 1 ENGINE (the window is a scanout buffer), 2 WHOLE (BEGIN: redraw all of it) */
//!     __u32 width, height, stride, fourcc;   /* out: the window's layout, linear, stride in bytes */
//!     __u64 offset;   /* EXPORT: out, the window's first byte in the dma-buf */
//!     __u64 size;     /* EXPORT: out, the dma-buf's bytes */
//! };
//! #define FBIO_BITSOS_SCANOUT _IOWR('F', 0xB4, struct bitsos_fb_scanout)   /* 0xc03046b4 */
//! ```
//!
//! Only the screen's holder may ask; EXPORT alone takes a screen nobody holds (a program with fb0 open, as for the other
//! BitsOS ioctls). BEGIN and END by anyone else are EBUSY, BEGIN and END with nothing exported EINVAL, and the input
//! `flags` must be 0 (a later bit means something this kernel does not know: EINVAL, the other out fields are ignored).
//! What a window is depends on the machine, the program sees one protocol:
//! - **With a flipping display engine:** window `w` is the scanout buffer A`w` itself: linear XRGB8888 at a plain
//!   physical address, what the flip code already points the pipes at. From the first BEGIN of a hold,
//!   FBIOPAN_DISPLAY to window `w` orders the flip (`ScanoutFlip::direct_flip`, the same order GX-3's `commit`
//!   gives: no other display register, GX-D9) and returns once it landed; fb0's RAM and frame take no part.
//!   BEGIN waits for the last flip and names the hidden buffer. A hold's direct state ends with the hold, on END,
//!   and when the engine stops flipping (late flip): the pan or BEGIN that saw it answers EIO, BEGIN goes on answering
//!   EIO until END, and the program goes back to drawing into fb0's mapping. Whichever way it ends, the view goes back
//!   on window 0, unseen, so the next store is a plain one; fb0's memory holds none of the program's frames, so a
//!   hold that ended this way leaves it to be seeded with the console's picture at the next take (`seed`).
//!   A refused pan changes nothing (fb_pan_display, linux-6.6/drivers/video/fbdev/core/fbmem.c:726-736: the var moves
//!   only after the driver said yes).
//! - **Without one (QEMU, the engine off):** the windows are fb0's own RAM, one dma-buf over all of it with an
//!   `offset` per window (a window need not start on a page). The rest stays as it is: FBIO_BITSOS_DAMAGE and the pan
//!   copy the reported rectangles into the frame (DP-6).
//!
//! Lifetime: a dma-buf outlives the hold, as a BO outlives a DRM master. Nothing revokes it: a holder that gave its
//! fd away, or a job still running at its death, can write into a buffer the console now shows, until the console
//! draws again. The program already passed `allowed` (B3, class Gpu) and held the whole screen.

use super::abi::{self, UserIo, Var};
use super::{flip, Fb};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use ksync::OnceRef;
use drm::dma_buf::{DmaBuf, Exporter};
use flip::{DirectBuffer, DirectFrame, FlipError, ScanoutFlip};
use hal::paging::CacheType;
use mm::device::Aperture;
use proc::Process;
use uapi::errno::{EBUSY, EFAULT, EINVAL, EIO, EPERM};
use vfs::Errno;

/// _IOWR('F', 0xB4, 48 bytes).
pub const FBIO_BITSOS_SCANOUT: u64 = 0xC030_46B4;
pub const SIZE: usize = 48;
pub const OP_EXPORT: u32 = 1;
pub const OP_BEGIN: u32 = 2;
pub const OP_END: u32 = 3;
pub const FLAG_ENGINE: u32 = 1;
pub const FLAG_WHOLE: u32 = 2;
/// DRM_FORMAT_XRGB8888 ('XR24').
pub const XRGB8888: u32 = 0x3432_5258;
const PAGE: u64 = 4096;

klog::counter!(CALLS, "fbdev.scanout.calls");
klog::counter!(REFUSED, "fbdev.scanout.refused");
klog::counter!(FLIPS, "fbdev.scanout.flips");

/// The ioctl's argument.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub op: u32,
    pub window: u32,
    pub fd: i32,
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub offset: u64,
    pub size: u64,
}

impl Args {
    pub fn from_bytes(b: &[u8; SIZE]) -> Args {
        let u = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let q = |o: usize| u64::from(u(o)) | u64::from(u(o + 4)) << 32;
        Args { op: u(0), window: u(4), fd: u(8) as i32, flags: u(12), width: u(16), height: u(20), stride: u(24), fourcc: u(28), offset: q(32), size: q(40) }
    }
    pub fn to_bytes(self) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        for (i, v) in [self.op, self.window, self.fd as u32, self.flags, self.width, self.height, self.stride, self.fourcc].into_iter().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[32..40].copy_from_slice(&self.offset.to_le_bytes());
        b[40..48].copy_from_slice(&self.size.to_le_bytes());
        b
    }
}

const NOTHING: u8 = 0;
const RAM: u8 = 1;
const ENGINE: u8 = 2;
/// The kernel ended the direct state within the hold (the engine stopped): BEGIN says so until the program's END.
const ENDED: u8 = 3;

/// A test's fake engine, for the test's length: the only engine QEMU can have (`dp.fbdev.scanout_engine`). Asked only
/// here, on the ioctls and pans of this protocol: a relaxed load per frame, nothing on fbdev's other paths.
static STAND_IN: OnceRef<dyn ScanoutFlip> = OnceRef::new();
static STANDING_IN: AtomicBool = AtomicBool::new(false);

fn stand_in() -> Option<&'static dyn ScanoutFlip> {
    if STANDING_IN.load(Ordering::Relaxed) { STAND_IN.get() } else { None }
}
fn ready() -> Option<&'static dyn ScanoutFlip> {
    stand_in().or_else(flip::ready)
}
fn active() -> Option<&'static dyn ScanoutFlip> {
    stand_in().or_else(flip::active)
}
/// Sets the test's engine (the same one every time: `OnceRef`); `on`: this protocol talks to it until `false`.
pub(super) fn stand_in_for_test(f: &'static dyn ScanoutFlip, on: bool) {
    let _ = STAND_IN.set(f);
    STANDING_IN.store(on, Ordering::Relaxed);
}

/// What the holder exported (`NOTHING`, `RAM`, `ENGINE`, `ENDED`) and whether its frames go straight to the scanout.
pub struct State {
    kind: AtomicU8,
    drawing: AtomicBool,
}

impl State {
    pub const fn new() -> State {
        State { kind: AtomicU8::new(NOTHING), drawing: AtomicBool::new(false) }
    }
    /// The holder draws into the scanout buffers now: a pan is a flip and fb0's memory holds nothing.
    pub fn drawing(&self) -> bool {
        self.drawing.load(Ordering::Acquire)
    }
    /// The hold ended (or END). True when the holder's frames went straight to the scanout until now: fb0's memory
    /// holds none of them and the view may be on either window (the caller: `view_home`).
    pub fn end(&self, why: &'static str) -> bool {
        self.kind.store(NOTHING, Ordering::Release);
        self.off(why)
    }
    /// The engine stopped within the hold. The holder learns it here or at its next BEGIN.
    fn stop(&self, why: &'static str) {
        self.kind.store(ENDED, Ordering::Release);
        self.off(why);
    }
    fn off(&self, why: &'static str) -> bool {
        let was = self.drawing.swap(false, Ordering::AcqRel);
        if was {
            klog::event!(Info, "fbdev", "scanout.off", why = why; "frames go through fb0's memory again");
        }
        was
    }
}

/// The view back on window 0, unseen (no refresh): under `fbdev.meta`, so never with `fbdev.screen` held.
pub(super) fn view_home(fb: &Fb) {
    let mut m = fb.meta.lock();
    m.var.0[abi::YOFFSET] = 0;
    fb.yoffset.store(0, Ordering::Release);
}

/// MCP `screen` while the GPU's frames pass fb0's frame by: what shows, read from the scanout buffer into a buffer of
/// its own. Without `fbdev.screen`: a read of write-combined memory takes tens of ms for a whole frame, and the
/// holder's pan needs that lock. May tear with a flip: this is a look. False: it cannot be read (the caller shows
/// fb0's frame, which is stale, and that is said).
pub(super) fn look(fb: &Fb, f: &mut dyn FnMut(&[u32], u32, u32)) -> bool {
    let Some(e) = active() else { return false };
    let g = fb.g();
    let n = g.width as usize * g.height as usize;
    let mut px: Vec<u32> = Vec::new();
    if px.try_reserve_exact(n).is_err() {
        klog::event!(Warn, "fbdev", "scanout.no_look", why = "no memory"; "the picture is stale");
        return false;
    }
    px.resize(n, 0);
    if !e.read_shown(&mut px, g.width) {
        klog::event!(Warn, "fbdev", "scanout.no_look", why = "unreadable"; "the picture is stale");
        return false;
    }
    f(&px, g.width, g.height);
    true
}

fn refuse(pid: u32, op: u32, why: &'static str, e: Errno) -> Result<Vec<u8>, Errno> {
    REFUSED.inc();
    klog::event!(Info, "fbdev", "scanout.refused", pid = pid, op = op, why = why);
    Err(e)
}

/// fb0's RAM as one dma-buf, in its pieces. Writes through it are not tracked: its user reports damage (DP-6).
struct RamWindows(Arc<Fb>);

impl Exporter for RamWindows {
    fn name(&self) -> &'static str {
        "bitsos-fb0"
    }
    fn size(&self) -> u64 {
        self.0.mem.len() as u64
    }
    fn aperture(&self) -> Option<Aperture> {
        None
    }
    fn apertures(&self) -> Vec<Aperture> {
        self.0.mem.apertures()
    }
    fn locate(&self, pgoff: u64) -> (usize, u64) {
        self.0.mem.locate(pgoff).unwrap_or((0, pgoff))
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        self.0.mem.page_phys(page)
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        usize::try_from(off).is_ok_and(|o| self.0.mem.read(o, out))
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        usize::try_from(off).is_ok_and(|o| self.0.mem.write(o, data))
    }
}

/// One scanout buffer: device memory outside RAM, write-combining as the kernel's own window over it (MM refuses
/// another type). The kernel never reads or writes through this object.
struct ScanoutWindow {
    phys: u64,
    len: u64,
}

impl Exporter for ScanoutWindow {
    fn name(&self) -> &'static str {
        "bitsos-scanout"
    }
    fn size(&self) -> u64 {
        self.len
    }
    fn aperture(&self) -> Option<Aperture> {
        Some(Aperture::Mmio { phys: self.phys, len: self.len, cache: CacheType::WriteCombining })
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        (page < self.len / PAGE).then(|| self.phys + page * PAGE)
    }
    fn read(&self, _off: u64, _out: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _off: u64, _data: &[u8]) -> bool {
        false
    }
}

/// The page-aligned range of a buffer and its first byte's offset in that range; None for an empty one or an overflow.
pub fn page_range(b: DirectBuffer) -> Option<(u64, u64, u64)> {
    let base = b.phys & !(PAGE - 1);
    let offset = b.phys - base;
    let end = offset.checked_add(b.len)?.checked_add(PAGE - 1)? & !(PAGE - 1);
    (b.len != 0).then_some((base, end, offset))
}

/// Where window `w` of fb0's RAM starts; None when it does not fit.
pub fn ram_offset(w: u32, line: u64, height: u64, rows: u64, mem: u64) -> Option<u64> {
    let top = u64::from(w).checked_mul(height)?;
    let end = top.checked_add(height)?;
    (w < 2 && end <= rows && end.checked_mul(line)? <= mem).then_some(top * line)
}

/// BEGIN on the engine: (window, flags), Err when it no longer flips.
pub fn begin_engine(f: Option<&dyn ScanoutFlip>) -> Result<(u32, u32), FlipError> {
    let DirectFrame { window, whole } = f.ok_or(FlipError::Off)?.direct_begin()?;
    Ok((window as u32, FLAG_ENGINE | if whole { FLAG_WHOLE } else { 0 }))
}

/// BEGIN on fb0's RAM: the window not shown.
pub fn begin_ram(yoffset: u32, height: u32) -> u32 {
    u32::from(height == 0 || yoffset / height == 0)
}

/// The window a pan to `yoffset` names, on the window grid; None off it.
pub fn window_of(yoffset: u32, height: u32) -> Option<usize> {
    let w = yoffset.checked_div(height)? as usize;
    (w < 2 && yoffset.is_multiple_of(height)).then_some(w)
}

fn export(fb: &Fb, a: &mut Args, me: &Arc<Process>) -> Result<(), Errno> {
    let g = fb.g();
    let line = u64::from(g.line_length());
    if a.window > 1 {
        return Err(EINVAL);
    }
    let engine = ready().and_then(|f| f.direct_buffer(a.window as usize));
    let kind = if engine.is_some() { ENGINE } else { RAM };
    if fb.direct.kind.compare_exchange(NOTHING, kind, Ordering::AcqRel, Ordering::Acquire).is_err_and(|k| k != kind) {
        // The engine came or went between the two windows: the holder cannot flip one of each.
        return Err(EBUSY);
    }
    let (exp, offset, stride): (Arc<dyn Exporter>, u64, u64) = match engine {
        Some(b) => {
            let (base, len, offset) = page_range(b).ok_or(EINVAL)?;
            (Arc::new(ScanoutWindow { phys: base, len }), offset, u64::from(b.stride))
        }
        None => {
            let offset = ram_offset(a.window, line, u64::from(g.height), u64::from(g.rows), fb.mem.len() as u64).ok_or(EINVAL)?;
            (Arc::new(RamWindows(fb.me.upgrade().ok_or(EIO)?)), offset, line)
        }
    };
    let buf = DmaBuf::export(exp, None)?;
    a.size = buf.size();
    a.fd = buf.install(Some(&me.fds), true)?;
    a.offset = offset;
    a.flags = if kind == ENGINE { FLAG_ENGINE } else { 0 };
    a.width = g.width;
    a.height = g.height;
    a.stride = stride as u32;
    a.fourcc = XRGB8888;
    klog::event!(Info, "fbdev", "scanout.exported", pid = me.pid, window = a.window, engine = kind == ENGINE, size = a.size, offset = a.offset, stride = a.stride);
    Ok(())
}

/// `FBIO_BITSOS_SCANOUT` from `who` (None: a kernel caller has no fd table, refused).
pub(super) fn ioctl(fb: &Fb, arg: u64, io: &dyn UserIo, who: Option<&Arc<Process>>) -> Result<Vec<u8>, Errno> {
    CALLS.inc();
    let mut b = [0u8; SIZE];
    if !io.read(arg, &mut b) {
        return Err(EFAULT);
    }
    let mut a = Args::from_bytes(&b);
    let Some(me) = who else { return refuse(0, a.op, "no calling process", EPERM) };
    if !matches!(a.op, OP_EXPORT | OP_BEGIN | OP_END) || a.flags != 0 {
        return Err(EINVAL);
    }
    // A program that only looks must not take the console's screen: EXPORT says it wants it, BEGIN and END do not.
    if fb.holder_pid() != me.pid && !(a.op == OP_EXPORT && fb.holder_pid() == 0 && fb.take_for(me.pid)) {
        return refuse(me.pid, a.op, "the screen is not the caller's", EBUSY);
    }
    let kind = fb.direct.kind.load(Ordering::Acquire);
    if a.op != OP_EXPORT && kind == NOTHING {
        return refuse(me.pid, a.op, "nothing exported", EINVAL);
    }
    match a.op {
        OP_EXPORT => export(fb, &mut a, me)?,
        OP_BEGIN if kind == ENGINE => match begin_engine(active()) {
            Ok((window, flags)) => {
                // After the first BEGIN the frames are the holder's: the RAM frame stops being the truth.
                fb.direct.drawing.store(true, Ordering::Release);
                (a.window, a.flags) = (window, flags);
            }
            Err(e) => {
                klog::event!(Warn, "fbdev", "scanout.begin_failed", pid = me.pid, error = alloc::format!("{e:?}"));
                fb.direct.stop("the engine stopped flipping");
                view_home(fb);
                return refuse(me.pid, a.op, "the engine stopped flipping", EIO);
            }
        },
        OP_BEGIN if kind == ENDED => return refuse(me.pid, a.op, "the engine stopped flipping", EIO),
        OP_BEGIN => {
            a.window = begin_ram(fb.yoffset.load(Ordering::Acquire), fb.g().height);
            a.flags = 0;
        }
        _ => {
            fb.direct.end("end");
            // The program's next store is its first into fb0's memory.
            view_home(fb);
        }
    }
    Ok(a.to_bytes().to_vec())
}

/// FBIOPAN_DISPLAY while the holder draws into the scanout buffers: orders the flip and waits for it to land. An
/// engine that stopped flipping ends the direct state: EIO, and the program copies again.
pub(super) fn pan(fb: &Fb, var: &Var, who: Option<&Arc<Process>>) -> Result<Vec<u8>, Errno> {
    let Some(me) = who.filter(|p| p.pid == fb.holder_pid()) else {
        return refuse(who.map_or(0, |p| p.pid), OP_BEGIN, "a pan by someone else while the holder draws", EBUSY);
    };
    // Nothing moves until the flip is ordered: a refused pan leaves the var and the view as they were.
    let window = {
        let mut next = fb.meta.lock().var;
        abi::pan(var, &mut next)?;
        if next.0[abi::XOFFSET] != 0 {
            return Err(EINVAL);
        }
        window_of(next.0[abi::YOFFSET], next.0[abi::YRES]).ok_or(EINVAL)?
    };
    let engine = active();
    let ordered = {
        // Under the screen's lock, so a hold that just ended is seen and no kernel frame is prepared meanwhile.
        let scr = fb.screen.lock();
        if scr.guard.is_none() || !fb.direct.drawing() {
            drop(scr);
            return refuse(me.pid, OP_BEGIN, "the screen went back meanwhile", EBUSY);
        }
        engine.map_or(Err(FlipError::Off), |f| f.direct_flip(window))
    };
    match ordered {
        Ok(seq) => {
            FLIPS.inc();
            {
                let mut m = fb.meta.lock();
                if abi::pan(var, &mut m.var).is_ok() {
                    fb.yoffset.store(m.var.0[abi::YOFFSET], Ordering::Release);
                }
            }
            fb.flip_seq.store(seq, Ordering::Release);
            fb.presented();
            flip::landed(engine, seq);
            Ok(var.to_bytes().to_vec())
        }
        Err(e) => {
            klog::event!(Warn, "fbdev", "scanout.flip_failed", pid = me.pid, window = window, error = alloc::format!("{e:?}"));
            fb.direct.stop("the flip failed");
            view_home(fb);
            refuse(me.pid, OP_BEGIN, "the flip failed", EIO)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_number_and_layout_are_frozen() {
        assert_eq!(FBIO_BITSOS_SCANOUT, 3 << 30 | (SIZE as u64) << 16 | (b'F' as u64) << 8 | 0xB4);
        let a = Args { op: 2, window: 1, fd: -1, flags: 3, width: 4, height: 5, stride: 6, fourcc: XRGB8888, offset: 1 << 40, size: 9 };
        let b = a.to_bytes();
        assert_eq!(Args::from_bytes(&b), a);
        assert_eq!(&b[8..12], &[0xFF; 4], "fd at byte 8");
        assert_eq!(&b[28..32], b"XR24");
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 1 << 40, "offset at byte 32");
    }

    #[test]
    fn a_window_that_starts_inside_a_page() {
        // 1366 x 768: window 1 starts half a page in, so one dma-buf with an offset, not two of whole pages.
        let (line, h) = (5464, 768);
        assert_eq!(ram_offset(0, line, h, 2 * h, 2 * line * h), Some(0));
        assert_eq!(ram_offset(1, line, h, 2 * h, 2 * line * h), Some(line * h));
        assert_eq!((line * h) % PAGE, 2048);
        assert_eq!(ram_offset(1, line, h, h, line * h), None, "one screen of memory has no window 1");
        assert_eq!(ram_offset(2, line, h, 3 * h, 3 * line * h), None);
        assert_eq!(ram_offset(u32::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX), None, "no overflow");
    }

    #[test]
    fn a_buffer_off_the_page_grid_keeps_its_first_byte() {
        let b = |phys, len| DirectBuffer { phys, len, stride: 4 };
        assert_eq!(page_range(b(0x1000, 0x2000)), Some((0x1000, 0x2000, 0)));
        assert_eq!(page_range(b(0x1800, 0x1000)), Some((0x1000, 0x2000, 0x800)));
        assert_eq!(page_range(b(0x1000, 0)), None);
        assert_eq!(page_range(b(0x1000, u64::MAX)), None);
    }

    #[test]
    fn windows_on_the_grid() {
        assert_eq!(window_of(0, 800), Some(0));
        assert_eq!(window_of(800, 800), Some(1));
        assert_eq!(window_of(400, 800), None);
        assert_eq!(window_of(1600, 800), None);
        assert_eq!(window_of(0, 0), None);
        assert_eq!(begin_ram(0, 800), 1);
        assert_eq!(begin_ram(800, 800), 0);
        assert_eq!(begin_ram(0, 0), 1);
    }

    struct Fake(Result<DirectFrame, FlipError>);
    impl ScanoutFlip for Fake {
        fn ready(&self) -> bool {
            true
        }
        fn active(&self) -> bool {
            true
        }
        fn present_shown(&self, _: &[u32], _: u32, _: (u32, u32, u32, u32)) -> Result<devmodel::display::Fence, devmodel::DevError> {
            Err(devmodel::DevError::Unsupported)
        }
        fn begin(&self, _: &[u32], _: u32) -> Result<&dyn devmodel::display::DisplayDevice, FlipError> {
            Err(FlipError::Off)
        }
        fn commit(&self) -> Result<u64, FlipError> {
            Err(FlipError::Off)
        }
        fn wait(&self, _: u64) -> Result<(), FlipError> {
            Ok(())
        }
        fn vblank(&self) -> Option<flip::Vblank> {
            None
        }
        fn rearm(&self) {}
        fn direct_begin(&self) -> Result<DirectFrame, FlipError> {
            self.0
        }
    }

    #[test]
    fn begin_on_the_engine_says_what_to_redraw() {
        let f = Fake(Ok(DirectFrame { window: 1, whole: false }));
        assert_eq!(begin_engine(Some(&f)), Ok((1, FLAG_ENGINE)));
        let f = Fake(Ok(DirectFrame { window: 0, whole: true }));
        assert_eq!(begin_engine(Some(&f)), Ok((0, FLAG_ENGINE | FLAG_WHOLE)));
        let f = Fake(Err(FlipError::Late));
        assert_eq!(begin_engine(Some(&f)), Err(FlipError::Late));
        assert_eq!(begin_engine(None), Err(FlipError::Off));
    }
}
