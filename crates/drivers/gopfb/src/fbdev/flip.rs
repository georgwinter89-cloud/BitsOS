//! GX-3 (written by lane GX; the GX-3 brief grants gopfb's fbdev): page flips of a display engine as fbdev and
//! the live GopFb use them. amdgpu implements `ScanoutFlip` (`amdgpu::scanout`) and installs it from an
//! initcall; gopfb does not depend on amdgpu. Without an installed or ready engine (QEMU, a refused attach: no GPU,
//! the text console's picture) everything goes into the GOP framebuffer. With one, frames reach the screen only by
//! flips: a late flip drops its frame, the next one flips again (no copy stands in).
//!
//! Two buffers: A0 (the GOP framebuffer) and A1. Every present of the live GopFb (console, kernel pointer, panic)
//! goes to the buffer that is shown (`present_shown`); fbdev's frames go to the hidden one (`begin` .. `commit`) and are
//! flipped in at the next vblank, so the scanout never shows a frame half-copied (no tearing).

use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::DevError;
use devmodel::display::{DisplayDevice, Fence, VblankEvent};
use display::RefreshSource;
use display::vblank::Snapshot;
use ksync::OnceRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipError {
    /// No engine flips (not installed, not attached).
    Off,
    /// The last flip did not land within its time, or this one could not be ordered: the frame is dropped (the
    /// engine gave the late flip up, the next frame flips again).
    Late,
    /// Another frame is being prepared.
    Busy,
}

/// The engine's own vblank: the last one (sequence = its frame counter), the period and the rate.
#[derive(Clone, Copy, Debug, Default)]
pub struct Vblank {
    pub last: VblankEvent,
    pub period_ns: u64,
    pub refresh_mhz: u32,
}

/// GX-5a: why the hardware cursor refused (`fbdev::cursor` turns it into an errno).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorError {
    /// No hardware cursor (no engine, QEMU, a split pipe, no buffer): ENODEV, the pointer is drawn in software.
    NoDevice,
    /// Size, hotspot or pixel count wrong, or a move before any image: EINVAL.
    Invalid,
    /// Another image is being written: EBUSY.
    Busy,
    /// A register write failed: EIO.
    Failed,
}

/// WY-17: one scanout buffer as a program gets it: `len` bytes at `phys`, linear XRGB8888, `stride` bytes per line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectBuffer {
    pub phys: u64,
    pub len: u64,
    pub stride: u32,
}

/// WY-17: the buffer a program draws its next frame into; `whole`: something else wrote into the buffers since
/// (the kernel's pointer, the console), so the frame must redraw all of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectFrame {
    pub window: usize,
    pub whole: bool,
}

pub trait ScanoutFlip: Send + Sync {
    /// Thread context: flipping works (attaches the engine on first use; may sleep).
    fn ready(&self) -> bool;
    /// Any context, lock-free, never attaches: the engine flips now.
    fn active(&self) -> bool;
    /// Any context (panic included), lock-free: presents the rectangle `(x, y, w, h)` of the full frame `src` into
    /// the buffer shown now (what `DisplayDevice::present` does on the live GopFb while the engine flips).
    fn present_shown(&self, src: &[u32], stride: u32, rect: (u32, u32, u32, u32)) -> Result<Fence, DevError>;
    /// Thread context, may sleep (bounded by the per-surface timeout): waits until the last flip landed, brings
    /// the hidden buffer up to date from `src` (a full frame, `stride` pixels per line) where it lacks what the
    /// shown one got, and returns the device that writes into it. Everything presented through it until
    /// `commit` is part of the next flip.
    fn begin(&self, src: &[u32], stride: u32) -> Result<&dyn DisplayDevice, FlipError>;
    /// Orders the flip to the buffer `begin` prepared; returns its sequence number (1, 2, ...).
    fn commit(&self) -> Result<u64, FlipError>;
    /// Sleeps until flip `seq` landed (at most the per-surface timeout; Late: it was given up).
    fn wait(&self, seq: u64) -> Result<(), FlipError>;
    /// The engine's vblank (None while it does not flip).
    fn vblank(&self) -> Option<Vblank>;
    /// WY-17, thread context: scanout buffer `window` (0 = A0, 1 = A1), when the engine flips and the buffer can be
    /// handed out (None: fbdev's own windows go out instead).
    fn direct_buffer(&self, _window: usize) -> Option<DirectBuffer> {
        None
    }
    /// WY-17, thread context, may sleep (bounded as `begin`): waits until the last flip landed and names the hidden
    /// buffer. Unlike `begin` it repairs nothing and prepares nothing: the program draws into the buffer itself.
    fn direct_begin(&self) -> Result<DirectFrame, FlipError> {
        Err(FlipError::Off)
    }
    /// WY-17: orders the flip to `window`, which the program finished drawing: the same order `commit` gives, nothing
    /// else is written (GX-D9). Its sequence number; the one already ordered when `window` is the shown buffer.
    fn direct_flip(&self, _window: usize) -> Result<u64, FlipError> {
        Err(FlipError::Off)
    }
    /// SHOT, any context, lock-free: the scanout buffer shown now (the last one ordered), None while it does not flip.
    /// SeqCst against the store of a new order, so fbdev's hold of a buffer and the holder's next BEGIN see each other.
    fn shown(&self) -> Option<usize> {
        None
    }
    /// SHOT, thread context: rectangle `(x, y, w, h)` of scanout buffer `window` into `dst` (`w * h` pixels row by
    /// row), for MCP `screen` while no copy of the picture exists. False: it cannot be read.
    fn read_window(&self, _window: usize, _rect: (u32, u32, u32, u32), _dst: &mut [u32]) -> bool {
        false
    }
    /// GX-5a, any context, lock-free: a hardware cursor is there (default: none, the compositor draws its own).
    fn cursor_ready(&self) -> bool {
        false
    }
    /// GX-5a, thread context: a new cursor image, `w` x `h` ARGB8888 pixels row by row (pre-multiplied unless
    /// `straight`), its hotspot `hot` on `at` (screen pixels, any value), and shown.
    fn cursor_image(&self, _w: u32, _h: u32, _hot: (u32, u32), _straight: bool, _px: &[u32], _at: (i32, i32)) -> Result<(), CursorError> {
        Err(CursorError::NoDevice)
    }
    /// GX-5a: the cursor's hotspot to `at` (and shown); a few register writes, no allocation.
    fn cursor_move(&self, _at: (i32, i32)) -> Result<(), CursorError> {
        Err(CursorError::NoDevice)
    }
    /// GX-5a: hidden until the next image or move.
    fn cursor_hide(&self) -> Result<(), CursorError> {
        Err(CursorError::NoDevice)
    }
}

static FLIP: OnceRef<dyn ScanoutFlip> = OnceRef::new();

/// Installs the engine (once; amdgpu's initcall). False if one was installed already.
pub fn install(f: &'static dyn ScanoutFlip) -> bool {
    FLIP.set(f)
}

/// The installed engine, whether or not it flips.
pub fn get() -> Option<&'static dyn ScanoutFlip> {
    FLIP.get()
}

/// Thread context: the engine, if it flips (attaching it on first use).
pub fn ready() -> Option<&'static dyn ScanoutFlip> {
    get().filter(|f| f.ready())
}

/// Any context: the engine, if it flips now.
pub fn active() -> Option<&'static dyn ScanoutFlip> {
    get().filter(|f| f.active())
}

/// The live GopFb's redirect: Some(result) when the engine flips (the present went to the buffer shown).
pub fn present_shown(src: &[u32], stride: u32, rect: (u32, u32, u32, u32)) -> Option<Result<Fence, DevError>> {
    get().filter(|f| f.active()).map(|f| f.present_shown(src, stride, rect))
}

/// What became of one fb0 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// Into the hidden buffer, flip `seq` ordered.
    Flipped(u64),
    /// The caller copies it to the screen; `whole`: the whole frame (no engine flips any more, and the GOP
    /// framebuffer lacks the frames that went to the other buffer).
    Copy { whole: bool },
    /// The engine flips, but this frame's flip was not ordered (the last flip was late, or the order failed):
    /// nothing reached the screen and nothing is copied. What the frame still owes the screen stays in it (a
    /// failed `begin` took none of its damage; what went into the hidden buffer is repaired from the frame by the
    /// next `begin`), so the next frame carries it.
    Dropped,
}

/// One fb0 frame `fr` through the engine `f` (None: none flips): `begin` with `src(fr)` (the full frame and its
/// stride), `into` presents the damage through the hidden buffer's device, `commit`. `flipping` remembers that frames went to the hidden buffer, so the first
/// copy after the engine went away is whole.
pub fn frame<T: ?Sized>(
    f: Option<&dyn ScanoutFlip>,
    flipping: &AtomicBool,
    fr: &mut T,
    src: impl FnOnce(&T) -> (&[u32], u32),
    into: impl FnOnce(&dyn DisplayDevice, &mut T),
) -> Shown {
    let Some(f) = f else {
        return Shown::Copy { whole: flipping.swap(false, Ordering::AcqRel) };
    };
    let (px, stride) = src(fr);
    match f.begin(px, stride) {
        Ok(back) => {
            into(back, fr);
            match f.commit() {
                Ok(seq) => {
                    flipping.store(true, Ordering::Release);
                    Shown::Flipped(seq)
                }
                Err(FlipError::Off) => {
                    flipping.store(false, Ordering::Release);
                    Shown::Copy { whole: true }
                }
                Err(_) => Shown::Dropped,
            }
        }
        // Another frame is being prepared (a program and the kernel at once): copy into the shown buffer.
        Err(FlipError::Busy) => Shown::Copy { whole: false },
        Err(FlipError::Late) => Shown::Dropped,
        Err(FlipError::Off) => {
            flipping.store(false, Ordering::Release);
            Shown::Copy { whole: true }
        }
    }
}

/// FBIOPAN_DISPLAY's end: flip `seq` (0 = none) landed or was given up; the pan succeeds either way (a given-up
/// flip's frame is dropped, the next one flips).
pub fn landed(f: Option<&dyn ScanoutFlip>, seq: u64) {
    if seq != 0
        && let Some(f) = f
    {
        let _ = f.wait(seq);
    }
}

/// The engine's vblank as the display clock (source Hardware); None for a period of 0.
pub fn snapshot(v: Vblank) -> Option<Snapshot> {
    if v.period_ns == 0 {
        return None;
    }
    let next = VblankEvent { sequence: v.last.sequence + 1, time_ns: v.last.time_ns + v.period_ns };
    Some(Snapshot { refresh_mhz: v.refresh_mhz, period_ns: v.period_ns, source: RefreshSource::Hardware, last: v.last, next })
}

/// FBIO_WAITFORVSYNC on the engine `f`: to the next vblank start, then until the frame counter moved (it may step
/// a few lines later), at most two periods (B4). False: `f` gives no vblank (the caller uses the estimate).
pub fn wait_vsync(f: &dyn ScanoutFlip, now: &dyn Fn() -> u64, sleep: &dyn Fn(u64)) -> bool {
    const MS: u64 = 1_000_000;
    let Some(v0) = f.vblank() else {
        return false;
    };
    let t0 = now();
    let deadline = t0 + 2 * v0.period_ns.max(MS);
    let at = v0.last.time_ns + v0.period_ns;
    if at > t0 {
        sleep(at - t0);
    }
    while let Some(v) = f.vblank() {
        if v.last.sequence > v0.last.sequence || now() >= deadline {
            break;
        }
        sleep(MS / 4);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU64, Ordering};
    use devmodel::display::{Mode, PixelFormat};
    use ksync::SpinLock;

    /// A device that records the rectangles presented through it.
    struct Back(SpinLock<Vec<(u32, u32, u32, u32)>>);

    impl DisplayDevice for Back {
        fn name(&self) -> &str {
            "back"
        }
        fn modes(&self) -> Vec<Mode> {
            Vec::new()
        }
        fn current_mode(&self) -> Mode {
            Mode { width: 4, height: 2, refresh_mhz: 0 }
        }
        fn format(&self) -> PixelFormat {
            PixelFormat::Bgrx8888
        }
        fn present(&self, _src: &[u32], _stride: u32, d: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
            self.0.lock().push(d);
            Ok(Fence { seq: 0, signalled: true })
        }
        fn last_vblank(&self) -> VblankEvent {
            VblankEvent::default()
        }
    }

    /// A fake engine: answers set per test, every call logged; its vblank follows the fake clock `t`.
    struct Fake {
        log: SpinLock<Vec<String>>,
        begin: SpinLock<Result<(), FlipError>>,
        commit: SpinLock<Result<u64, FlipError>>,
        wait: SpinLock<Result<(), FlipError>>,
        back: Back,
        t: AtomicU64,
        period: u64,
        /// The counter steps this long after the vblank start (scan line vs. frame counter).
        lag: u64,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                log: SpinLock::new(Vec::new()),
                begin: SpinLock::new(Ok(())),
                commit: SpinLock::new(Ok(1)),
                wait: SpinLock::new(Ok(())),
                back: Back(SpinLock::new(Vec::new())),
                t: AtomicU64::new(0),
                period: 16_666_667,
                lag: 0,
            }
        }
        fn calls(&self) -> Vec<String> {
            core::mem::take(&mut *self.log.lock())
        }
        fn note(&self, s: &str) {
            self.log.lock().push(String::from(s));
        }
    }

    impl ScanoutFlip for Fake {
        fn ready(&self) -> bool {
            true
        }
        fn active(&self) -> bool {
            true
        }
        fn present_shown(&self, _: &[u32], _: u32, _: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
            self.note("shown");
            Ok(Fence { seq: 0, signalled: true })
        }
        fn begin(&self, _: &[u32], _: u32) -> Result<&dyn DisplayDevice, FlipError> {
            self.note("begin");
            (*self.begin.lock())?;
            Ok(&self.back)
        }
        fn commit(&self) -> Result<u64, FlipError> {
            self.note("commit");
            *self.commit.lock()
        }
        fn wait(&self, seq: u64) -> Result<(), FlipError> {
            self.log.lock().push(alloc::format!("wait {seq}"));
            *self.wait.lock()
        }
        fn vblank(&self) -> Option<Vblank> {
            let t = self.t.load(Ordering::Acquire);
            let n = t.saturating_sub(self.lag) / self.period;
            Some(Vblank {
                last: VblankEvent { sequence: n, time_ns: t / self.period * self.period },
                period_ns: self.period,
                refresh_mhz: 60_000,
            })
        }
    }

    fn run(f: &Fake, flipping: &AtomicBool) -> Shown {
        frame(Some(f), flipping, &mut [0u32; 8][..], |p| (p, 4), |d, p| {
            f.note("into");
            let _ = d.present(p, 4, (0, 0, 4, 2));
        })
    }

    #[test]
    fn pan_is_begin_into_commit_wait() {
        let f = Fake::new();
        let on = AtomicBool::new(false);
        *f.commit.lock() = Ok(7);
        assert_eq!(run(&f, &on), Shown::Flipped(7));
        landed(Some(&f), 7);
        assert_eq!(f.calls(), ["begin", "into", "commit", "wait 7"]);
        assert_eq!(*f.back.0.lock(), [(0, 0, 4, 2)], "the damage went into the hidden buffer");
        assert!(on.load(Ordering::Acquire));
        // No flip ordered yet: no wait.
        landed(Some(&f), 0);
        assert!(f.calls().is_empty());
    }

    #[test]
    fn a_late_flip_drops_the_frame_and_no_engine_copies_whole() {
        let f = Fake::new();
        let on = AtomicBool::new(false);
        // A late flip (begin waited for the last one): dropped, nothing copied, no commit.
        assert_eq!(run(&f, &on), Shown::Flipped(1));
        *f.begin.lock() = Err(FlipError::Late);
        assert_eq!(run(&f, &on), Shown::Dropped);
        assert_eq!(f.calls(), ["begin", "into", "commit", "begin"], "no commit after a failed begin");
        assert!(on.load(Ordering::Acquire), "the engine still flips");
        // A commit that fails: the damage went into the hidden buffer, which the next begin repairs: dropped too.
        *f.begin.lock() = Ok(());
        *f.commit.lock() = Err(FlipError::Late);
        assert_eq!(run(&f, &on), Shown::Dropped);
        // Busy: copy only the damage into the shown buffer, flipping stays as it was.
        *f.commit.lock() = Ok(2);
        assert_eq!(run(&f, &on), Shown::Flipped(2));
        *f.begin.lock() = Err(FlipError::Busy);
        assert_eq!(run(&f, &on), Shown::Copy { whole: false });
        assert!(on.load(Ordering::Acquire));
        // The wait times out (the flip was given up): the pan still ends. Once no engine flips (it went away), the
        // first copy is whole, the next ones are not.
        *f.wait.lock() = Err(FlipError::Late);
        landed(Some(&f), 2);
        let px = &mut [0u32; 8][..];
        let none = frame(None, &on, px, |p| (p, 4), |_, _| panic!("no device without an engine"));
        assert_eq!(none, Shown::Copy { whole: true });
        assert_eq!(frame(None, &on, px, |p| (p, 4), |_, _| {}), Shown::Copy { whole: false });
        // An engine that says Off copies whole once as well.
        on.store(true, Ordering::Release);
        *f.begin.lock() = Err(FlipError::Off);
        assert_eq!(run(&f, &on), Shown::Copy { whole: true });
        assert!(!on.load(Ordering::Acquire));
    }

    #[test]
    fn vsync_and_pacing_follow_the_engine() {
        let mut f = Fake::new();
        f.lag = 200_000;
        f.t.store(5 * f.period + 3_000_000, Ordering::Release);
        let s = snapshot(f.vblank().unwrap()).unwrap();
        assert_eq!(s.source, RefreshSource::Hardware);
        assert_eq!(super::super::pacing::flags(s.source) & super::super::pacing::VBLANK_ESTIMATED, 0, "not an estimate");
        assert_eq!((s.last.sequence, s.next.sequence, s.next.time_ns), (5, 6, 6 * f.period));
        assert!(snapshot(Vblank::default()).is_none(), "period 0 is no clock");
        // WAITFORVSYNC: sleeps to the vblank start, then until the counter moved (200 us later).
        let t = &f.t;
        let slept = SpinLock::new(Vec::new());
        assert!(wait_vsync(&f, &|| t.load(Ordering::Acquire), &|ns| {
            slept.lock().push(ns);
            t.fetch_add(ns, Ordering::AcqRel);
        }));
        assert_eq!(f.vblank().unwrap().last.sequence, 6);
        let slept = slept.lock().clone();
        assert_eq!(slept[0], f.period - 3_000_000);
        assert!(t.load(Ordering::Acquire) < 6 * f.period + 1_000_000, "{slept:?}");
        // A counter that never moves: bounded by two periods.
        f.lag = u64::MAX;
        let t0 = f.t.load(Ordering::Acquire);
        let t = &f.t;
        wait_vsync(&f, &|| t.load(Ordering::Acquire), &|ns| {
            t.fetch_add(ns, Ordering::AcqRel);
        });
        assert!(f.t.load(Ordering::Acquire) - t0 <= 2 * f.period + 1_000_000);
    }
}
