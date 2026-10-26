//! GX-3 (written by lane GX; the GX-3 brief grants gopfb's fbdev): page flips of a display engine as fbdev and
//! the live GopFb use them. amdgpu implements `ScanoutFlip` (`amdgpu::scanout`) and installs it from an
//! initcall; gopfb does not depend on amdgpu. Without an installed or ready engine (QEMU, a refused attach, a
//! late flip) everything stays on the copy path into the GOP framebuffer.
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
    /// No engine flips (not installed, not attached, or fell back to one image).
    Off,
    /// The last flip did not land within its time: the engine fell back to A0 and one image; the next present
    /// must be whole (A0 lacks the frames that went to A1).
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

pub trait ScanoutFlip: Send + Sync {
    /// Thread context: flipping works (attaches the engine on first use; may sleep).
    fn ready(&self) -> bool;
    /// Any context, lock-free, never attaches: the engine flips now.
    fn active(&self) -> bool;
    /// Any context, lock-free: the live GopFb's presents go through `present_shown` (while flipping, and while
    /// the engine is not sure which buffer shows after a failed fall-back).
    fn redirects(&self) -> bool {
        self.active()
    }
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
    /// Sleeps until flip `seq` landed (at most the per-surface timeout; Late: it fell back).
    fn wait(&self, seq: u64) -> Result<(), FlipError>;
    /// The engine's vblank (None while it does not flip).
    fn vblank(&self) -> Option<Vblank>;
    /// Thread context, at a rare state change (fbdev's take of the screen): after a late flip, flipping starts
    /// again if the engine shows A0 settled (V1 kept one image until the next start; V2 re-arms per surface).
    fn rearm(&self);
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
    get().filter(|f| f.redirects()).map(|f| f.present_shown(src, stride, rect))
}

/// What became of one fb0 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// Into the hidden buffer, flip `seq` ordered.
    Flipped(u64),
    /// The caller copies it to the screen; `whole`: the whole frame (the shown buffer lacks frames that went to
    /// the other one, or this frame's damage went into a buffer that is not flipped in).
    Copy { whole: bool },
}

/// One fb0 frame `fr` through the engine `f` (None: none flips): `begin` with `src(fr)` (the full frame and its
/// stride), `into` presents the damage through the hidden buffer's device, `commit`. `flipping` remembers that frames went to the hidden buffer, so the first
/// copy after flipping stopped (a late flip, a fall-back) is whole.
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
                Err(_) => {
                    flipping.store(false, Ordering::Release);
                    Shown::Copy { whole: true }
                }
            }
        }
        // Another frame is being prepared (a program and the kernel at once): copy into the shown buffer.
        Err(FlipError::Busy) => Shown::Copy { whole: false },
        Err(_) => {
            flipping.store(false, Ordering::Release);
            Shown::Copy { whole: true }
        }
    }
}

/// FBIOPAN_DISPLAY's end: flip `seq` (0 = none) landed or fell back; the pan succeeds either way (the fall-back
/// copies the next frame whole, `frame`).
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
        fn rearm(&self) {
            self.note("rearm");
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
    fn errors_fall_back_to_a_whole_copy() {
        let f = Fake::new();
        let on = AtomicBool::new(false);
        // A late flip (begin waited for the last one): copy, whole.
        assert_eq!(run(&f, &on), Shown::Flipped(1));
        *f.begin.lock() = Err(FlipError::Late);
        assert_eq!(run(&f, &on), Shown::Copy { whole: true });
        assert_eq!(f.calls(), ["begin", "into", "commit", "begin"], "no commit after a failed begin");
        // A commit that fails: the damage went into a buffer not flipped in.
        *f.begin.lock() = Ok(());
        *f.commit.lock() = Err(FlipError::Late);
        assert_eq!(run(&f, &on), Shown::Copy { whole: true });
        // Busy: copy only the damage into the shown buffer, flipping stays as it was.
        *f.commit.lock() = Ok(2);
        assert_eq!(run(&f, &on), Shown::Flipped(2));
        *f.begin.lock() = Err(FlipError::Busy);
        assert_eq!(run(&f, &on), Shown::Copy { whole: false });
        assert!(on.load(Ordering::Acquire));
        // The wait times out (the engine fell back): the pan still ends, and once the engine no longer flips the
        // first copy is whole, the next ones are not.
        *f.wait.lock() = Err(FlipError::Late);
        landed(Some(&f), 2);
        let px = &mut [0u32; 8][..];
        let none = frame(None, &on, px, |p| (p, 4), |_, _| panic!("no device without an engine"));
        assert_eq!(none, Shown::Copy { whole: true });
        assert_eq!(frame(None, &on, px, |p| (p, 4), |_, _| {}), Shown::Copy { whole: false });
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
