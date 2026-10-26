//! Display (owner: DP; contract `C: Console / RamFrame` together with IN, V2-PLAN §2.8, §9.1).
//!
//! The authoritative picture is a frame in normal RAM (`RamFrame`); presenting copies only the damaged
//! rectangles to the active `DisplayDevice` (GOP first, `gopfb`). Nobody reads the framebuffer (slow,
//! spec/gpu.md §2.3), and MCP `screen` encodes this RAM frame. DP-1: damage list, scanout hand-over (the console
//! stops presenting while another owner holds the screen; DP-2's fbdev uses it), the timer-based vblank
//! estimate (`vblank`). DP-2 adds fbdev with deferred io, DP-4 surfaces and the compositor.
//!
//! DP-2: a scanout owner may bring its own picture for MCP `screen` (`ScanoutSource`: fbdev's frame), a
//! program owner names its pid (`scanout_holder`), the VT's hand-over hooks run when a program takes or gives
//! back the screen (`set_vt_hooks`, `sync_vt`), the VT can ask for the screen back (`request_scanout_back`,
//! KD_TEXT) or hand it to a program at once (`request_scanout_for`, KD_GRAPHICS; fbdev registers the owner).
//!
//! DP-3: `edid` decodes the monitor's detailed timings (the exact refresh of the scanout mode); the vblank clock
//! keeps an exact period and where its rate came from (`RefreshSource`), for fbdev's pacing ioctl.
//!
//! Locking: `display.frame` (a SpinLock at the klog-sink level, ksync/order.rs) guards the console's frame
//! and the hand-over state; a present runs under it (the copy is ~1.7 ms for a full 1440p frame). Nothing
//! under it sleeps, wakes a thread or logs; events are emitted after it is released. A `ScanoutSource` and the
//! VT hooks are called with no display lock held. The VT hooks run only from `sync_vt`, under its own sleeping
//! `display.vt` (nothing else is taken there but what the hooks take); the owner of a program scanout calls
//! `sync_vt` once its own locks are released, so a VT path that holds a console lock and reads the screen
//! (`with_screen` -> the owner's lock) can never meet the owner holding its lock and waiting for the console.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::display::DisplayDevice;
use ksync::{Mutex, OnceRef, SpinLock};

pub mod font;
mod frame;
mod rect;
pub mod edid;
pub mod vblank;

pub use frame::{PresentStats, RamFrame};
pub use rect::Rect;
pub use vblank::{RefreshSource, VblankClock};

struct State {
    frame: RamFrame,
    /// Who holds the scanout instead of the console (None = the console's frame is shown).
    held_by: Option<&'static str>,
    /// The holder's own picture for `snapshot` (MCP `screen`), if it brought one.
    source: Option<Arc<dyn ScanoutSource>>,
    /// The program holding the scanout (0 = a kernel owner).
    pid: u32,
    /// A returned scanout marked the console frame whole and no present has shown it yet (DP-2f).
    returning: bool,
    /// The pixels of the present that showed the console frame again after the last return.
    shown_px: u64,
}

/// The picture a scanout owner shows, for `with_screen`/`snapshot` (MCP `screen` sees what a fullscreen program
/// drew: the kernel's surface model, V2-PLAN §2.8). Called without any display lock held.
pub trait ScanoutSource: Send + Sync {
    /// Runs `f` on the picture (pixels 0x00RRGGBB, width, height) under the owner's own lock; false = no picture
    /// now (`f` not called). `f` must be short: it only copies.
    fn with_picture(&self, f: &mut dyn FnMut(&[u32], u32, u32)) -> bool;
}

/// The VT's side of a program taking the screen (IN registers `console::vt::hand_over`/`take_back`): called
/// when a program (pid != 0) takes the scanout and when it gives it back, with no display lock held.
#[derive(Clone, Copy)]
pub struct VtHooks {
    pub hand_over: fn(u32),
    pub take_back: fn(),
}

static VT_HOOKS: OnceRef<VtHooks> = OnceRef::new();
/// The program the VT was last told about (0 = the console has the screen as far as the VT knows).
static VT_TOLD: Mutex<u32> = Mutex::named("display.vt", 0);

/// Installs the VT's hand-over hooks (once; IN's console at init). False if hooks were set already.
pub fn set_vt_hooks(h: &'static VtHooks) -> bool {
    VT_HOOKS.set(h)
}

/// Brings the VT in line with who holds the scanout: `take_back()` when the program it was told about no longer
/// holds it, `hand_over(pid)` for the program that holds it now (a kernel owner counts as the console: the VT is
/// never told about tests). Idempotent and ordered (`display.vt`), so concurrent takes and releases always end
/// with the VT told the final state. Called by a program scanout's owner after a take or a release, with none of
/// its own locks held, and by `ScanoutGuard`'s drop. B4: waits at most 1 s for another sync (event, then the
/// next sync catches up).
pub fn sync_vt() {
    let Some(h) = VT_HOOKS.get() else { return };
    let Some(mut told) = VT_TOLD.lock_timeout(1_000_000_000) else {
        klog::event!(Warn, "display", "vt.sync_busy"; "another VT sync ran for 1 s: the VT is told at the next one");
        return;
    };
    let now = scanout_holder().map_or(0, |(_, pid)| pid);
    if *told == now {
        return;
    }
    if *told != 0 {
        (h.take_back)();
    }
    if now != 0 {
        (h.hand_over)(now);
    }
    klog::event!(Debug, "display", "vt.synced", was = *told, now = now);
    *told = now;
}

/// A program owner (fbdev) that can show a program's picture at once, for `request_scanout_for`.
#[derive(Clone, Copy)]
pub struct ProgramOwner {
    /// Takes the screen for program `pid` (true when it shows that program's picture afterwards). Called with no
    /// display lock held; it calls `sync_vt` itself.
    pub take_for: fn(u32) -> bool,
    /// `request_scanout_back` was called: look at the release generation now, not at the next poll (DP-2f).
    /// Called with no display lock held, possibly under the caller's (the VT's ioctl) own; must not block.
    pub give_back: fn(),
}

static PROGRAM_OWNER: OnceRef<ProgramOwner> = OnceRef::new();

/// Installs the program owner (once; fbdev at init). False if one was set already.
pub fn set_program_owner(o: &'static ProgramOwner) -> bool {
    PROGRAM_OWNER.set(o)
}

/// A program's terminal switched to KD_GRAPHICS (KDSETMODE): the screen shows that program's picture now, before
/// its first store, and the console stops drawing on it (Linux: a KD_GRAPHICS tty is not drawn on). The screen
/// comes back when the program ends or `request_scanout_back` is called. False without a program owner, or when
/// another owner holds the screen. The VT calls it with none of its locks held (its `hand_over` hook runs inside).
pub fn request_scanout_for(pid: u32) -> bool {
    let ok = PROGRAM_OWNER.get().is_some_and(|o| (o.take_for)(pid));
    klog::event!(Info, "display", "scanout.for_requested", pid = pid, taken = ok);
    ok
}

/// Bumped by `request_scanout_back`; a program owner gives the screen back when it changes.
static RELEASE_GEN: AtomicU64 = AtomicU64::new(0);

/// The console wants the screen back from a program (KDSETMODE KD_TEXT on its terminal): the program owner
/// (fbdev) is woken (`ProgramOwner::give_back`) and drops its scanout at its next pass. A kernel owner (a test)
/// is not asked.
pub fn request_scanout_back() {
    RELEASE_GEN.fetch_add(1, Ordering::AcqRel);
    if let Some(o) = PROGRAM_OWNER.get() {
        (o.give_back)();
    }
    klog::event!(Info, "display", "scanout.back_requested", holder = scanout_holder().map_or(0, |h| h.1));
}

/// The pixels the last scanout return copied to show the console frame again (one whole frame, or 0 when another
/// owner took the screen before it was shown). Exact while a console write presents at the same time; the
/// `scanout.returned` event's `presented` counts every present during the return.
pub fn returned_pixels() -> u64 {
    RETURNED_PX.load(Ordering::Acquire)
}

static RETURNED_PX: AtomicU64 = AtomicU64::new(0);

/// The number of `request_scanout_back` calls so far (an owner compares it with the value at its take).
pub fn release_generation() -> u64 {
    RELEASE_GEN.load(Ordering::Acquire)
}

static DEVICE: OnceRef<dyn DisplayDevice> = OnceRef::new();
static FRAME: SpinLock<Option<State>> = SpinLock::named("display.frame", None);

static PRESENTS: AtomicU64 = AtomicU64::new(0);
static RECTS: AtomicU64 = AtomicU64::new(0);
static PIXELS: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static HELD_SKIPS: AtomicU64 = AtomicU64::new(0);

/// Counters since boot: presents that copied something, rectangles, pixels, rectangles the device refused,
/// console presents held back because another owner had the scanout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub presents: u64,
    pub rects: u64,
    pub pixels: u64,
    pub errors: u64,
    pub held_skips: u64,
}

pub fn stats() -> Stats {
    Stats {
        presents: PRESENTS.load(Ordering::Relaxed),
        rects: RECTS.load(Ordering::Relaxed),
        pixels: PIXELS.load(Ordering::Relaxed),
        errors: ERRORS.load(Ordering::Relaxed),
        held_skips: HELD_SKIPS.load(Ordering::Relaxed),
    }
}

fn count(st: PresentStats) -> PresentStats {
    if st.rects + st.errors > 0 {
        PRESENTS.fetch_add(1, Ordering::Relaxed);
        RECTS.fetch_add(st.rects as u64, Ordering::Relaxed);
        PIXELS.fetch_add(st.pixels, Ordering::Relaxed);
        ERRORS.fetch_add(st.errors as u64, Ordering::Relaxed);
    }
    st
}

/// B6 for rectangles the device refused (they are dropped; a retry would be refused again). Called after
/// `display.frame` is released, never on the panic/NMI path. Rate-limited to the first refusal and then each
/// time the total passes a power of two, so a device that refuses everything cannot flood the log (the console
/// is a klog sink: its own presents may be the ones refused).
fn report(dev: &dyn DisplayDevice, st: PresentStats) {
    if st.errors == 0 {
        return;
    }
    let total = ERRORS.load(Ordering::Relaxed);
    if worth_reporting(total.saturating_sub(st.errors as u64), total) {
        klog::event!(Warn, "display", "present.refused", device = dev.name(), rects = st.errors, total = total;
            "damaged rectangles dropped: the device refused them (frame and mode disagree?)");
    }
}

/// True when the refusal count went from `before` to `total` across 1 or a power of two.
fn worth_reporting(before: u64, total: u64) -> bool {
    total > before && (before == 0 || total.ilog2() > before.ilog2())
}

/// Runs after a scanout owner's frame went to the screen (IN-6: the kernel pointer over a program's picture,
/// and the program latency path): `(device, frame, presented bbox, copy start ns, copy end ns)`. It runs under
/// the owner's lock and never under `display.frame`; it takes no lock of its own and puts back any frame pixels it
/// changes before returning.
pub type OwnerPresentHook = fn(&dyn DisplayDevice, &mut RamFrame, Rect, u64, u64);

static OWNER_HOOK: OnceRef<OwnerPresentHook> = OnceRef::new();

/// Installs the owner-present hook (once; the console at start). False if one was set already.
pub fn set_owner_present_hook(h: &'static OwnerPresentHook) -> bool {
    OWNER_HOOK.set(h)
}

/// Presents `frame`'s damage on `dev` and counts it in `stats` (for owners of their own frames).
pub fn present_frame(dev: &dyn DisplayDevice, frame: &mut RamFrame) -> PresentStats {
    let bbox = frame.damage();
    let t0 = klog::now();
    let st = count(frame.present_to(dev));
    let t1 = klog::now();
    if st.rects > 0 {
        if let Some(h) = OWNER_HOOK.get() {
            h(dev, frame, bbox, t0, t1);
        }
    }
    report(dev, st);
    st
}

/// Installs the scanout device and creates the RAM frame at its resolution. Boot only, once: a second call
/// is refused with an event (the console's frame, a held scanout and the device must stay one consistent set;
/// a mode change goes through `resize`, GX-4).
pub fn set_device(d: &'static dyn DisplayDevice) {
    let m = d.current_mode();
    if !DEVICE.set(d) {
        let have = DEVICE.get().map_or("(being set)", |o| o.name());
        klog::event!(Error, "display", "device.refused", name = d.name(), current = have; "a display device is already installed");
        return;
    }
    *FRAME.lock() = Some(State { frame: RamFrame::new(m.width, m.height), held_by: None, source: None, pid: 0, returning: false, shown_px: 0 });
    klog::event!(Info, "display", "device", name = d.name(), width = m.width, height = m.height);
}

/// GX-4 (CR 1): the device shows a new size (a mode switch): the console's RAM frame is made again at `w` x `h`,
/// black and whole-damaged (the VT redraws it, `console::refit`). Also while another owner holds the scanout
/// (a rollback must not wait for it): the console frame is not shown then, and the owner's picture is its own.
/// The new frame is allocated before `display.frame` is taken; the old one is freed after it is released.
pub fn resize(w: u32, h: u32) -> Result<(), &'static str> {
    if w == 0 || h == 0 {
        return Err("empty size");
    }
    if DEVICE.get().is_none() {
        return Err("no display");
    }
    let mut frame = RamFrame::try_new(w, h).ok_or("no memory for the console frame")?;
    frame.mark_all();
    let old = {
        let mut g = FRAME.lock();
        let s = g.as_mut().ok_or("no display")?;
        core::mem::replace(&mut s.frame, frame)
    };
    drop(old);
    klog::event!(Info, "display", "resized", width = w, height = h);
    Ok(())
}

pub fn device() -> Option<&'static dyn DisplayDevice> {
    DEVICE.get()
}

/// Runs `f` on the RAM frame (if a display exists).
pub fn with_frame<R>(f: impl FnOnce(&mut RamFrame) -> R) -> Option<R> {
    FRAME.lock().as_mut().map(|s| f(&mut s.frame))
}

/// `with_frame` for the panic/NMI path: runs `f` only if the frame lock is free right now (never waits).
pub fn try_with_frame<R>(f: impl FnOnce(&mut RamFrame) -> R) -> Option<R> {
    FRAME.try_lock()?.as_mut().map(|s| f(&mut s.frame))
}

/// Copies the damaged part of the RAM frame to the screen (kept for later while another owner holds it).
pub fn present() {
    let st = FRAME.lock().as_mut().and_then(|s| present_state(s, false));
    if let (Some(st), Some(dev)) = (st, DEVICE.get()) {
        report(dev, st);
    }
}

/// `present` for the panic/NMI path: skipped if the frame lock is busy (the damage stays for the next one).
/// While another owner holds the scanout the whole console frame is shown: an emergency message wins.
pub fn try_present() {
    if let Some(mut g) = FRAME.try_lock()
        && let Some(s) = g.as_mut()
    {
        let _ = present_state(s, true);
    }
}

/// Runs under `display.frame`; the caller reports refusals (`report`) once the lock is released.
fn present_state(s: &mut State, emergency: bool) -> Option<PresentStats> {
    let dev = DEVICE.get()?;
    if s.held_by.is_some() {
        if !emergency {
            HELD_SKIPS.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        s.frame.mark_all();
    }
    let st = count(s.frame.present_to(dev));
    // The first present after a return takes the whole console frame, whoever calls it (the VT's redraw or a
    // console write at the same time): its pixels are the return's own copy, without any other present's.
    if s.returning && s.held_by.is_none() {
        s.returning = false;
        s.shown_px = st.pixels;
    }
    Some(st)
}

/// Runs `f` on the picture the screen shows (pixels, width, height): the scanout owner's own picture while an
/// owner with a `ScanoutSource` holds it (fbdev: what the program drew), else the console's RAM frame. MCP
/// `screen` reads through this (MC change request DP-2/1). None without a display or when the owner has no
/// picture now (never the console frame the screen does not show). `f` runs under a lock: copy only.
pub fn with_screen<R>(f: impl FnOnce(&[u32], u32, u32) -> R) -> Option<R> {
    let src = FRAME.lock().as_ref()?.source.clone();
    match src {
        Some(src) => {
            let mut f = Some(f);
            let mut out = None;
            src.with_picture(&mut |px, w, h| {
                if let Some(f) = f.take() {
                    out = Some(f(px, w, h));
                }
            });
            out
        }
        None => FRAME.lock().as_ref().map(|s| f(s.frame.pixels(), s.frame.width, s.frame.height)),
    }
}

/// A copy of the picture the screen shows (`with_screen`) for MCP `screen` and tests (width, height, pixels);
/// None without a display, when the owner has no picture, or when the copy cannot be allocated. The buffer is
/// allocated before any lock is taken (14.7 MB at 1440p: never under the lock); the lock then covers one memcpy,
/// shorter than a full present.
pub fn snapshot() -> Option<(u32, u32, Vec<u32>)> {
    let (w, h) = FRAME.lock().as_ref().map(|s| (s.frame.width, s.frame.height))?;
    let n = w as usize * h as usize;
    let mut px = Vec::new();
    if px.try_reserve_exact(n).is_err() {
        klog::event!(Warn, "display", "snapshot.no_memory", width = w, height = h);
        return None;
    }
    // Every picture has the display's size (set_device is boot-only); never copy into a buffer of the wrong size.
    let ok = with_screen(|p, pw, ph| {
        if (pw, ph) != (w, h) {
            return false;
        }
        px.extend_from_slice(p);
        true
    })?;
    ok.then_some((w, h, px))
}

/// Why the scanout could not be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanoutError {
    NoDisplay,
    Busy { owner: &'static str },
}

/// The screen handed to an owner other than the console (an fbdev program, a test). While it lives the console
/// draws into its RAM frame but does not present; dropping it shows the console frame again, whole.
pub struct ScanoutGuard {
    owner: &'static str,
    dev: &'static dyn DisplayDevice,
    pid: u32,
}

/// Takes the screen for `owner`. Pending console damage is presented first, in the same locked section, so the
/// screen shows exactly the console frame at the hand-over.
pub fn take_scanout(owner: &'static str) -> Result<ScanoutGuard, ScanoutError> {
    take_scanout_with(owner, None, 0)
}

/// `take_scanout` for an owner that shows its own picture (`source`, for `snapshot`) on behalf of program
/// `pid` (0 = a kernel owner). The VT is not told here (the caller may hold its own locks): for a program the
/// caller runs `sync_vt` once they are released; the guard's drop runs it too.
pub fn take_scanout_with(owner: &'static str, source: Option<Arc<dyn ScanoutSource>>, pid: u32) -> Result<ScanoutGuard, ScanoutError> {
    let dev = DEVICE.get().ok_or(ScanoutError::NoDisplay)?;
    let st = {
        let mut g = FRAME.lock();
        let s = g.as_mut().ok_or(ScanoutError::NoDisplay)?;
        if let Some(o) = s.held_by {
            drop(g);
            klog::event!(Info, "display", "scanout.busy", owner = o, wanted_by = owner);
            return Err(ScanoutError::Busy { owner: o });
        }
        let st = present_state(s, false);
        s.held_by = Some(owner);
        s.source = source;
        s.pid = pid;
        st
    };
    if let Some(st) = st {
        report(dev, st);
    }
    klog::event!(Info, "display", "scanout.taken", owner = owner, pid = pid);
    Ok(ScanoutGuard { owner, dev, pid })
}

/// Who holds the scanout now (None = the console).
pub fn scanout_owner() -> Option<&'static str> {
    FRAME.lock().as_ref().and_then(|s| s.held_by)
}

/// The owner holding the scanout and the program it holds it for (pid 0 = a kernel owner); None = the console.
pub fn scanout_holder() -> Option<(&'static str, u32)> {
    FRAME.lock().as_ref().and_then(|s| s.held_by.map(|o| (o, s.pid)))
}

impl ScanoutGuard {
    pub fn owner(&self) -> &'static str {
        self.owner
    }
    pub fn device(&self) -> &'static dyn DisplayDevice {
        self.dev
    }
    /// The program this guard holds the screen for (0 = a kernel owner).
    pub fn pid(&self) -> u32 {
        self.pid
    }
    /// Presents the owner's frame (its damage) on the screen.
    pub fn present(&mut self, frame: &mut RamFrame) -> PresentStats {
        present_frame(self.dev, frame)
    }
    /// Presents through another back end over the same scanout (tests of other pixel formats).
    pub fn present_via(&mut self, dev: &dyn DisplayDevice, frame: &mut RamFrame) -> PresentStats {
        present_frame(dev, frame)
    }
}

/// The console frame is shown whole by one present: for a program owner the VT is told first (`sync_vt`), and
/// its `take_back` redraws the text and presents, which takes the damage marked here; the `present` after it
/// then has nothing left (DP-2f: before, the drop copied the whole frame and the VT's redraw copied it again).
/// For a kernel owner (no VT change) the `present` copies it.
impl Drop for ScanoutGuard {
    fn drop(&mut self) {
        let px = PIXELS.load(Ordering::Relaxed);
        let source = {
            let mut g = FRAME.lock();
            g.as_mut().and_then(|s| {
                s.held_by = None;
                s.pid = 0;
                s.frame.mark_all();
                s.returning = true;
                s.shown_px = 0;
                s.source.take()
            })
        };
        // The source (perhaps the last reference to its frame) is dropped outside display.frame.
        drop(source);
        // A program owner drops its guard with none of its locks held (see the module docs).
        if self.pid != 0 {
            sync_vt();
        }
        present();
        let presented = PIXELS.load(Ordering::Relaxed).wrapping_sub(px);
        // Not shown yet only if another owner took the screen first: then this return copied nothing.
        let shown = FRAME.lock().as_mut().map_or(0, |s| {
            let shown = if s.returning { 0 } else { s.shown_px };
            s.returning = false;
            shown
        });
        RETURNED_PX.store(shown, Ordering::Release);
        klog::event!(Info, "display", "scanout.returned", owner = self.owner, pid = self.pid, pixels = shown, presented = presented);
    }
}

/// Keeps a shared handle type for back ends that want `Arc`.
pub type DeviceArc = Arc<dyn DisplayDevice>;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use devmodel::display::{Fence, Mode, PixelFormat, VblankEvent};
    use devmodel::DevError;

    /// A device that records the rectangles it was asked to present.
    pub struct Recorder {
        pub rects: SpinLock<Vec<(u32, u32, u32, u32)>>,
    }
    impl DisplayDevice for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn modes(&self) -> Vec<Mode> {
            alloc::vec![self.current_mode()]
        }
        fn current_mode(&self) -> Mode {
            Mode { width: 50, height: 40, refresh_mhz: 0 }
        }
        fn format(&self) -> PixelFormat {
            PixelFormat::Bgrx8888
        }
        fn present(&self, _src: &[u32], _stride: u32, d: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
            self.rects.lock().push(d);
            Ok(Fence { seq: 1, signalled: true })
        }
        fn last_vblank(&self) -> VblankEvent {
            VblankEvent::default()
        }
    }

    #[test]
    fn handover_holds_console_presents_and_restores_whole_frame() {
        static REC: Recorder = Recorder { rects: SpinLock::named("display.test_rec", Vec::new()) };
        static OTHER: Recorder = Recorder { rects: SpinLock::named("display.test_rec_other", Vec::new()) };
        set_device(&REC);
        with_frame(|f| f.fill(Rect::new(0, 0, 5, 5), 1));
        set_device(&OTHER);
        assert!(core::ptr::addr_eq(device().unwrap(), &REC), "a second set_device is refused");
        assert_eq!(with_frame(|f| f.damage()), Some(Rect::new(0, 0, 5, 5)), "and keeps the frame");
        let mut g = take_scanout("test").unwrap();
        assert_eq!(REC.rects.lock().as_slice(), &[(0, 0, 5, 5)], "pending damage presented at the hand-over");
        assert_eq!(take_scanout("other").err(), Some(ScanoutError::Busy { owner: "test" }));
        assert_eq!(scanout_owner(), Some("test"));
        with_frame(|f| f.fill(Rect::new(10, 10, 2, 2), 2));
        let skips = stats().held_skips;
        present();
        assert_eq!(stats().held_skips, skips + 1);
        assert_eq!(REC.rects.lock().len(), 1, "console present held back");
        let mut own = RamFrame::new(50, 40);
        own.fill(Rect::new(3, 4, 5, 6), 9);
        assert_eq!(g.present(&mut own).rects, 1);
        drop(g);
        assert_eq!(REC.rects.lock().last(), Some(&(0, 0, 50, 40)), "whole console frame back");
        assert_eq!(scanout_owner(), None);
        try_present();
        assert!(take_scanout("again").is_ok());
        assert!(OTHER.rects.lock().is_empty());

        // GX-4 CR 1: a mode switch makes the frame again at the new size, black and whole-damaged; refused for
        // an empty or impossible size (nothing changes).
        assert_eq!(resize(0, 10), Err("empty size"));
        assert!(resize(u32::MAX, u32::MAX).is_err());
        assert_eq!(with_frame(|f| (f.width, f.height)), Some((50, 40)));
        resize(30, 20).unwrap();
        assert_eq!(with_frame(|f| (f.width, f.height, f.damage())), Some((30, 20, Rect::new(0, 0, 30, 20))));
        resize(50, 40).unwrap();
        assert_eq!(with_frame(|f| (f.width, f.height)), Some((50, 40)));

        // DP-2: an owner with its own picture and a program behind it (one test: the display is global).
        struct Pic;
        impl ScanoutSource for Pic {
            fn with_picture(&self, f: &mut dyn FnMut(&[u32], u32, u32)) -> bool {
                f(&[7; 2000], 50, 40);
                true
            }
        }
        static HANDED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        static BACK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        fn hand(pid: u32) {
            HANDED.store(pid, Ordering::SeqCst);
        }
        // As the console's `take_back` (console/src/vt.rs `set_kd_mode`): a full redraw, then a present.
        fn back() {
            BACK.fetch_add(1, Ordering::SeqCst);
            with_frame(|f| f.mark_all());
            present();
        }
        static HOOKS: VtHooks = VtHooks { hand_over: hand, take_back: back };
        assert!(set_vt_hooks(&HOOKS));
        let console = snapshot().unwrap();
        assert_eq!((console.0, console.1), (50, 40));
        let g = take_scanout_with("fbdev", Some(Arc::new(Pic)), 4242).unwrap();
        assert_eq!(scanout_holder(), Some(("fbdev", 4242)));
        assert_eq!(g.pid(), 4242);
        assert_eq!(HANDED.load(Ordering::SeqCst), 0, "the VT is told by sync_vt, not under the owner's locks");
        sync_vt();
        assert_eq!(HANDED.load(Ordering::SeqCst), 4242, "the VT was told");
        sync_vt();
        assert_eq!(BACK.load(Ordering::SeqCst), 0, "a second sync changes nothing");
        assert_eq!(snapshot(), Some((50, 40, alloc::vec![7; 2000])), "MCP screen shows the owner's picture");
        assert_eq!(with_screen(|p, w, h| (p[0], w, h)), Some((7, 50, 40)));
        let g0 = release_generation();
        request_scanout_back();
        assert_eq!(release_generation(), g0 + 1);
        let n = REC.rects.lock().len();
        drop(g);
        assert_eq!(BACK.load(Ordering::SeqCst), 1, "the VT took the screen back");
        // DP-2f: one whole copy, by the VT's present (the drop does not copy it a second time).
        assert_eq!(&REC.rects.lock()[n..], &[(0, 0, 50, 40)], "the console frame copied once");
        assert_eq!(returned_pixels(), 2000, "the return's own copy");
        assert_eq!(scanout_holder(), None);
        assert_eq!(snapshot().map(|s| s.2), Some(console.2), "the console frame again");
        let k = take_scanout("kernel").unwrap();
        assert_eq!(scanout_holder(), Some(("kernel", 0)));
        sync_vt();
        let n = REC.rects.lock().len();
        drop(k);
        assert_eq!(&REC.rects.lock()[n..], &[(0, 0, 50, 40)], "a kernel owner's drop copies the console frame once");
        assert_eq!(returned_pixels(), 2000);
        assert_eq!(BACK.load(Ordering::SeqCst), 1, "a kernel owner does not call the VT hooks");
        assert_eq!(HANDED.load(Ordering::SeqCst), 4242);

        // KD_GRAPHICS: the program owner is asked; without one the request is refused.
        assert!(!request_scanout_for(77));
        fn take_for(pid: u32) -> bool {
            pid == 77
        }
        static GIVEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        fn give_back() {
            GIVEN.fetch_add(1, Ordering::SeqCst);
        }
        static OWNER: ProgramOwner = ProgramOwner { take_for, give_back };
        request_scanout_back();
        assert_eq!(GIVEN.load(Ordering::SeqCst), 0, "no owner yet");
        assert!(set_program_owner(&OWNER));
        assert!(request_scanout_for(77) && !request_scanout_for(78));
        let g0 = release_generation();
        request_scanout_back();
        assert_eq!((release_generation(), GIVEN.load(Ordering::SeqCst)), (g0 + 1, 1), "the owner is woken at once");

        // A return that another owner overtakes before the console frame is shown: its presents are held back
        // and do not count as the return's copy; the first free present does.
        let set = |returning: bool, held: Option<&'static str>| {
            let mut g = FRAME.lock();
            let st = g.as_mut().unwrap();
            (st.returning, st.held_by, st.shown_px) = (returning, held, 0);
            st.frame.mark_all();
        };
        let state = || FRAME.lock().as_ref().map(|s| (s.returning, s.shown_px));
        set(true, Some("overtaker"));
        present();
        assert_eq!(state(), Some((true, 0)), "a held present is not the return's copy");
        set(true, None);
        present();
        assert_eq!(state(), Some((false, 2000)), "the first free present is");
    }

    #[test]
    fn refusal_events_are_rate_limited() {
        let fired: Vec<u64> = (1..=100u64).filter(|&t| worth_reporting(t - 1, t)).collect();
        assert_eq!(fired, alloc::vec![1, 2, 4, 8, 16, 32, 64]);
        assert!(worth_reporting(0, 5) && worth_reporting(3, 9) && !worth_reporting(9, 12) && !worth_reporting(4, 4));
    }
}
