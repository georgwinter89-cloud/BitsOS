//! SHOT (Georg 14.11.: one whole frame per screenshot, as Windows and Linux take it): MCP `screen` while the holder
//! draws straight into the scanout buffers (WY-17), where no RAM copy of the picture exists. The buffer shown is read,
//! only the rectangle asked for (hal's streaming loads), and held still while it is read: the holder's next BEGIN,
//! which would hand exactly that buffer back for drawing, waits until the copy ended. The flip to the holder's next
//! frame is not held up; with two buffers the holder pauses at that BEGIN, with more nothing visible would. Bounded:
//! after `HOLD_MAX_NS` the copy ends unfinished, the buffer goes back and the answer says the picture could not be
//! taken whole (ask again); never a picture with rows of two frames.
//!
//! Protocol (lock-free, SeqCst): the look stores `reading` = the buffer, then loads the engine's shown buffer again
//! and starts over if a flip came between; the holder's flip stores the engine's shown buffer, its BEGIN then loads
//! `reading`. Of the two store-then-load pairs at least one sees the other's store: either the look starts over on
//! the fresh buffer, or BEGIN waits. A BEGIN that waited `HOLD_MAX_NS` marks the copy spoiled before it returns; the
//! look checks that mark after its last (fenced) load, so a copy that overlapped the holder's drawing is never used.
//! Nothing here is held across the slow read but the look's own mutex (one look at a time), which neither the
//! vblank/flip path nor the console takes.

use super::flip::ScanoutFlip;
use super::{scanout, Fb, MS};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::SeqCst};
use display::{PictureError, Rect};
use ksync::{Mutex, WaitQueue};

/// The longest a buffer is held from the holder (the brief: about 100 ms).
pub const HOLD_MAX_NS: u64 = 100 * MS;
/// Rows read between two looks at the clock.
const BAND: u32 = 16;
/// A flip between choosing the buffer and holding it: choose again, this many times at most.
const TRIES: u32 = 4;
const NONE: u8 = u8::MAX;

klog::counter!(SHOTS, "fbdev.shot.taken");
klog::counter!(NOT_WHOLE, "fbdev.shot.not_whole");
klog::counter!(HELD, "fbdev.shot.held_begins");
klog::counter!(HELD_NS, "fbdev.shot.held_ns");
klog::counter!(COPY_NS, "fbdev.shot.copy_ns");

/// BEGINs that waited for a copy (tests).
pub fn held_begins() -> u64 {
    HELD.get()
}

/// The buffer a look is reading, for the holder's BEGIN.
pub struct Hold {
    reading: AtomicU8,
    since: AtomicU64,
    spoiled: AtomicBool,
    freed: WaitQueue,
    one: Mutex<()>,
}

/// Why `look` gave no picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// No engine flips or the buffer cannot be read: the caller shows fb0's frame (stale) as before.
    Off,
    Failed(PictureError),
}

impl Hold {
    pub const fn new() -> Hold {
        Hold { reading: AtomicU8::new(NONE), since: AtomicU64::new(0), spoiled: AtomicBool::new(false), freed: WaitQueue::new(), one: Mutex::named("fbdev.look", ()) }
    }

    /// The buffer being read now, if any (tests).
    pub fn reading(&self) -> Option<usize> {
        let r = self.reading.load(SeqCst);
        (r != NONE).then_some(usize::from(r))
    }

    /// Chooses the buffer shown and holds it (module doc). Off when no engine flips; NotWhole when flips kept
    /// coming between the choice and the hold.
    fn take(&self, e: &dyn ScanoutFlip, now: u64) -> Result<usize, Look> {
        for _ in 0..TRIES {
            let w = e.shown().ok_or(Look::Off)?;
            self.since.store(now, SeqCst);
            self.spoiled.store(false, SeqCst);
            self.reading.store(w as u8, SeqCst);
            if e.shown() == Some(w) {
                return Ok(w);
            }
            self.give_back();
        }
        Err(Look::Failed(PictureError::NotWhole))
    }

    /// Ends the hold; true when a BEGIN stopped waiting for it (the copy may hold rows of the next frame).
    fn give_back(&self) -> bool {
        self.reading.store(NONE, SeqCst);
        self.freed.wake_all();
        self.spoiled.load(SeqCst)
    }

    /// The holder's BEGIN named buffer `w` to draw into: waits while a look reads it, at most until `HOLD_MAX_NS`
    /// after the look began; then the look is marked spoiled and the holder goes on.
    pub fn wait_free(&self, w: usize) {
        let w = w as u8;
        if self.reading.load(SeqCst) != w {
            return;
        }
        let t = time::now_ns();
        let left = self.since.load(SeqCst).saturating_add(HOLD_MAX_NS).saturating_sub(t);
        let free = self.freed.wait_until_timeout(|| self.reading.load(SeqCst) != w, left);
        // Said once per copy: the holder's later BEGINs on the same buffer pass at once.
        if !free && self.reading.load(SeqCst) == w && !self.spoiled.swap(true, SeqCst) {
            klog::event!(Warn, "fbdev", "shot.hold_ended", window = w, held_ms = (time::now_ns() - t) / MS; "the holder draws again before the copy ended");
        }
        HELD.inc();
        HELD_NS.add(time::now_ns() - t);
    }
}

/// Rectangle `r` of the buffer shown into `dst` (`r.w * r.h` pixels row by row), held still for the copy (module
/// doc); the picture's size. `Look::Off` when no engine flips or the buffer cannot be read.
pub(super) fn look(fb: &Fb, r: Rect, dst: &mut Vec<u32>) -> Result<(u32, u32), Look> {
    let Some(e) = scanout::active() else { return Err(Look::Off) };
    let g = fb.g();
    if !display::rect_fits(r, g.width, g.height) {
        return Err(Look::Failed(PictureError::Size(g.width, g.height)));
    }
    let n = r.w as usize * r.h as usize;
    dst.clear();
    if dst.try_reserve_exact(n).is_err() {
        return Err(Look::Failed(PictureError::NoMemory));
    }
    dst.resize(n, 0);
    if n == 0 {
        return Ok((g.width, g.height));
    }
    let h = &fb.direct.hold;
    // Another look holds a buffer at most HOLD_MAX_NS (and its own read of one band).
    let Some(_one) = h.one.lock_timeout(4 * HOLD_MAX_NS) else { return Err(Look::Failed(PictureError::NotWhole)) };
    let ends = fb.direct.ends();
    let t0 = time::now_ns();
    let w = h.take(e, t0)?;
    let mut res = Ok(());
    let mut y = 0;
    while y < r.h {
        if time::now_ns().saturating_sub(t0) > HOLD_MAX_NS {
            res = Err(Look::Failed(PictureError::NotWhole));
            break;
        }
        let rows = BAND.min(r.h - y);
        let at = (y * r.w) as usize;
        if !e.read_window(w, (r.x, r.y + y, r.w, rows), &mut dst[at..at + (rows * r.w) as usize]) {
            res = Err(Look::Off);
            break;
        }
        y += rows;
    }
    let spoiled = h.give_back();
    let ns = time::now_ns() - t0;
    // The holder's state ended meanwhile: the kernel may have drawn into the buffer (its frames, the console).
    if res.is_ok() && (spoiled || fb.direct.ends() != ends || !fb.direct.drawing()) {
        res = Err(Look::Failed(PictureError::NotWhole));
    }
    match res {
        Ok(()) => {
            SHOTS.inc();
            COPY_NS.add(ns);
        }
        Err(Look::Failed(_)) => {
            NOT_WHOLE.inc();
            klog::event!(Warn, "fbdev", "shot.not_whole", window = w, rows = y, of = r.h, ms = ns / MS, spoiled = spoiled; "the picture could not be held still for the copy");
        }
        Err(Look::Off) => klog::event!(Warn, "fbdev", "scanout.no_look", why = "unreadable"; "the picture is stale"),
    }
    res.map(|()| (g.width, g.height))
}
