//! DP-6: a program that knows what it changed tells fb0 (`FBIO_BITSOS_DAMAGE`), and its stores into the mapping
//! cost nothing per page from then on. The tracked path (`mod.rs`, Linux's `fb_defio.c`) write-protects every
//! copied page and takes one fault per page per frame: FD-1 counted 3 600 faults per 1440p frame under bitscomp.
//! A compositor already has its damage rectangles, so it hands them over before FBIOPAN_DISPLAY (or instead of
//! it with one window); the kernel copies exactly those rectangles into the screen frame and never
//! write-protects the mapping while that program holds the screen. One more BitsOS ioctl in DP-3's private 'F'
//! range (ENOTTY on Linux, where a compositor uses KMS instead):
//!
//! ```c
//! struct bitsos_fb_rect { __u32 x, y, width, height; };   /* in the window, clipped to xres x yres */
//! struct bitsos_fb_damage {     /* 24 bytes, little-endian */
//!     __u32 flags;              /* 0, or BITSOS_DAMAGE_END (1): back to tracked stores */
//!     __u32 yoffset;            /* the first buffer row of the window the rectangles belong to */
//!     __u32 count;              /* rectangles at `rects` (0 is allowed: only switches the mode on) */
//!     __u32 reserved;           /* 0 */
//!     __u64 rects;              /* user pointer to `count` struct bitsos_fb_rect */
//! };
//! #define FBIO_BITSOS_DAMAGE _IOW('F', 0xB3, struct bitsos_fb_damage)
//! ```
//!
//! Rules. Only the screen's holder reports (a caller with fb0 open while the console has the screen takes it,
//! as a pan would; EBUSY while another owner has it: the stores stay tracked). From its first report on, its
//! stores are its own business: the rectangles must cover every pixel where the window named by `yoffset` differs
//! from the picture shown now. `yoffset` the shown window: the rectangles are copied and presented at once.
//! Another window (double buffering): they wait for the next FBIOPAN_DISPLAY to exactly that window, which then
//! copies them alone instead of the whole window. The mode ends with the hold of the screen (the program ends,
//! closes, KD_TEXT, a kernel owner) or with BITSOS_DAMAGE_END; then every page is write-protected again and the
//! window copied whole. Other programs that map fb0 meanwhile are not tracked (their stores show at the next
//! whole copy); write(2) still is.
//!
//! Locks: the rectangles are read from the caller's memory under `fbdev.screen` in steps of `STEP` (fbdev.screen
//! > mm.as, as the write-protect; the fault hook takes no fbdev mutex), so any count needs no allocation.
//!
//! Work bound (DP-6 fixer): `count` is the caller's word, and `fbdev.screen` is the lock the fbdev thread needs to
//! hand the screen back. So one call does at most `budget` (twice the window's pixels, see `cost`) worth of work;
//! past it the window is copied whole and the rest of the list is not even read. A waiting window's list stops at
//! one window's worth across all calls before its pan (then that pan copies whole). Both are fallbacks, never
//! errors: the picture stays complete and memory stays bounded by the screen size.

use super::abi::{self, UserIo};
use super::{Fb, Screen};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use display::Rect;
use proc::Process;
use uapi::errno::{EBUSY, EFAULT, EINVAL, EPERM};
use vfs::Errno;

/// `_IOW('F', 0xB3, 24 bytes)`: write direction (1 << 30), size 24 << 16, type 'F', number 0xB3.
pub const FBIO_BITSOS_DAMAGE: u64 = 0x4018_46B3;
pub const SIZE: usize = 24;
pub const RECT_SIZE: usize = 16;
pub const FLAG_END: u32 = 1;
/// Rectangles read from the caller per copy (a stack buffer of 512 bytes; any count is read in such steps).
pub const STEP: usize = 32;
/// Pixels one rectangle and each of its rows cost besides their area (one read and one copy call each).
const OVERHEAD: u64 = 64;

/// The work a reported rectangle `r` (already clipped; empty ones are read too) stands for.
pub fn cost(r: Rect) -> u64 {
    r.area() + OVERHEAD * (r.h as u64 + 1)
}

/// The work one call may do for a `width` x `height` window before the whole copy takes over.
pub fn budget(width: u32, height: u32) -> u64 {
    2 * width as u64 * height as u64
}

klog::counter!(CALLS, "fbdev.damage.calls");
klog::counter!(REFUSED, "fbdev.damage.refused");
klog::counter!(PIXELS, "fbdev.damage.px");
klog::counter!(RECTS, "fbdev.damage.rects");
klog::counter!(WHOLE, "fbdev.damage.whole");
klog::counter!(PANS, "fbdev.damage.pans");
klog::counter!(ENTERED, "fbdev.damage.entered");

/// Counters since boot (tests and the Mini-PC checks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub calls: u64,
    pub refused: u64,
    /// Pixels copied from reported rectangles (at the call or at the pan they waited for).
    pub px: u64,
    pub rects: u64,
    /// Whole-window copies on the damage path (the first report, a pan without matching damage, an END).
    pub whole: u64,
    /// Pans that copied only the rectangles reported for their window.
    pub pans: u64,
    pub entered: u64,
}

pub fn stats() -> Stats {
    Stats {
        calls: CALLS.get(),
        refused: REFUSED.get(),
        px: PIXELS.get(),
        rects: RECTS.get(),
        whole: WHOLE.get(),
        pans: PANS.get(),
        entered: ENTERED.get(),
    }
}

/// The request as the caller wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub flags: u32,
    pub yoffset: u32,
    pub count: u32,
    pub rects: u64,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Request {
    /// EINVAL for unknown flags or a non-zero reserved word (room for later versions).
    pub fn from_bytes(b: &[u8; SIZE]) -> Result<Request, Errno> {
        let r = Request {
            flags: u32_at(b, 0),
            yoffset: u32_at(b, 4),
            count: u32_at(b, 8),
            rects: u64::from_le_bytes([b[16], b[17], b[18], b[19], b[20], b[21], b[22], b[23]]),
        };
        if r.flags & !FLAG_END != 0 || u32_at(b, 12) != 0 {
            return Err(EINVAL);
        }
        Ok(r)
    }
    pub fn to_bytes(&self) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        b[0..4].copy_from_slice(&self.flags.to_le_bytes());
        b[4..8].copy_from_slice(&self.yoffset.to_le_bytes());
        b[8..12].copy_from_slice(&self.count.to_le_bytes());
        b[16..24].copy_from_slice(&self.rects.to_le_bytes());
        b
    }
}

/// One `struct bitsos_fb_rect` clipped to a `width` x `height` window (empty: nothing to copy).
pub fn rect_in(b: &[u8], width: u32, height: u32) -> Rect {
    Rect::new(u32_at(b, 0), u32_at(b, 4), u32_at(b, 8), u32_at(b, 12)).clip(width, height)
}

/// `yoffset` starts a whole window inside the virtual screen.
pub fn window_ok(yoffset: u32, yres: u32, yres_virtual: u32) -> bool {
    yoffset as u64 + yres as u64 <= yres_virtual as u64
}

/// Rectangles reported for a window that is not shown yet; the pan to it copies them (or everything).
#[derive(Default)]
pub struct Pending {
    at: Option<u32>,
    whole: bool,
    rects: Vec<Rect>,
    /// `cost` of what is listed, and where the list gives way to a whole copy (one window's pixels).
    spent: u64,
    limit: u64,
}

impl Pending {
    /// Forgets everything (the capacity stays for the next frame).
    pub fn clear(&mut self) {
        self.at = None;
        self.whole = false;
        self.rects.clear();
        self.spent = 0;
    }
    /// Reports for window `y` (of `limit` pixels) follow: older ones for another window are dropped (a pan to that
    /// one copies whole).
    pub fn start(&mut self, y: u32, limit: u64) {
        if self.at != Some(y) {
            self.clear();
            self.at = Some(y);
        }
        self.limit = limit;
    }
    pub fn is_whole(&self) -> bool {
        self.whole
    }
    pub fn add(&mut self, r: Rect) {
        if self.whole {
            return;
        }
        self.spent = self.spent.saturating_add(cost(r));
        if self.spent > self.limit {
            // More than a whole window's worth: the pan copies it whole (cheaper, and the list stays bounded).
            self.mark_whole();
            return;
        }
        if r.is_empty() {
            return;
        }
        if self.rects.try_reserve(1).is_err() {
            // Without room for the list the pan copies the whole window: never a lost update.
            self.mark_whole();
            return;
        }
        self.rects.push(r);
    }
    pub fn mark_whole(&mut self) {
        self.whole = true;
        self.rects.clear();
    }
    /// What a pan to window `y` copies: Some(rects) (possibly none) when exactly that window was reported and
    /// nothing forced a whole copy; None = the whole window.
    pub fn for_pan(&self, y: u32) -> Option<&[Rect]> {
        (self.at == Some(y) && !self.whole).then_some(&self.rects[..])
    }
}

fn refuse(pid: u32, why: &'static str, e: Errno) -> Result<Vec<u8>, Errno> {
    REFUSED.inc();
    klog::event!(Info, "fbdev", "damage.refused", pid = pid, why = why);
    Err(e)
}

/// `FBIO_BITSOS_DAMAGE` from `who` (None: a kernel caller, refused: it holds no screen).
pub(super) fn ioctl(fb: &Fb, arg: u64, io: &dyn UserIo, who: Option<&Arc<Process>>) -> Result<Vec<u8>, Errno> {
    CALLS.inc();
    let mut b = [0u8; SIZE];
    if !io.read(arg, &mut b) {
        return Err(EFAULT);
    }
    let req = Request::from_bytes(&b)?;
    let Some(me) = who else { return refuse(0, "no calling process", EPERM) };
    // fbdev.meta and fbdev.screen are never nested: the var is read first.
    let (yres, rows) = {
        let m = fb.meta.lock();
        (m.var.0[abi::YRES], m.var.0[abi::YRES_VIRTUAL])
    };
    if !window_ok(req.yoffset, yres, rows) {
        return Err(EINVAL);
    }
    if fb.holder_pid() != me.pid && !(fb.holder_pid() == 0 && fb.take_for(me.pid)) {
        return refuse(me.pid, "another owner has the screen", EBUSY);
    }
    let mut scr = fb.screen.lock();
    if scr.guard.is_none() || scr.holder.as_ref().map(|h| h.pid) != Some(me.pid) {
        // The hold ended between the take and the lock (KD_TEXT, a kernel owner).
        drop(scr);
        return refuse(me.pid, "the screen went back meanwhile", EBUSY);
    }
    if req.flags & FLAG_END != 0 {
        fb.end_damage(&mut scr, "end");
        return Ok(Vec::new());
    }
    // WY-17: the holder draws straight into the scanout buffers: fb0's memory holds none of it, nothing to copy.
    if fb.direct.drawing() {
        return Ok(Vec::new());
    }
    let top = fb.yoffset.load(Ordering::Acquire);
    let first = fb.shared.damage_pid.swap(me.pid, Ordering::AcqRel) != me.pid;
    if first {
        // Stores tracked before the switch are copied with the whole shown window; a hidden window may hold
        // tracked stores too, so the first pan to it copies whole.
        fb.shared.dirty.lock().clear();
        scr.reported.clear();
        fb.refresh(&mut scr);
        WHOLE.inc();
        ENTERED.inc();
        klog::event!(Info, "fbdev", "damage.on", pid = me.pid, yoffset = req.yoffset, count = req.count;
            "the holder reports its damage: its stores into fb0 are no longer write-protected");
    }
    let shown = req.yoffset == top;
    let (w, h) = (fb.g().width, yres);
    if !shown {
        // One full-window rectangle fits; more than that is more than the whole copy the pan would do.
        scr.reported.start(req.yoffset, cost(Rect::new(0, 0, w, h)));
        if first {
            scr.reported.mark_whole();
        }
    }
    let t0 = time::now_ns();
    let limit = budget(w, h);
    let mut spent = 0u64;
    let mut over = false;
    let mut buf = [0u8; STEP * RECT_SIZE];
    let mut done = 0usize;
    let mut px = 0u64;
    let mut marked = false;
    let count = req.count as usize;
    'read: while done < count {
        let n = (count - done).min(STEP);
        let ok = (done as u64)
            .checked_mul(RECT_SIZE as u64)
            .and_then(|o| req.rects.checked_add(o))
            .is_some_and(|a| io.read(a, &mut buf[..n * RECT_SIZE]));
        if !ok {
            // Nothing half-done stays: the shown window is copied whole, a waiting pan copies whole.
            if shown {
                fb.refresh(&mut scr);
                WHOLE.inc();
            } else {
                scr.reported.mark_whole();
            }
            return Err(EFAULT);
        }
        for c in buf[..n * RECT_SIZE].chunks_exact(RECT_SIZE) {
            let r = rect_in(c, w, h);
            if !shown {
                scr.reported.add(r);
                if scr.reported.is_whole() {
                    // The pan copies whole anyway: the rest of the list is not read.
                    over = true;
                    break 'read;
                }
                if !r.is_empty() {
                    RECTS.inc();
                }
                continue;
            }
            spent = spent.saturating_add(cost(r));
            if spent > limit {
                over = true;
                break 'read;
            }
            if r.is_empty() {
                continue;
            }
            RECTS.inc();
            if !first {
                let Screen { frame, .. } = &mut *scr;
                if fb.copy_rect(frame, top, r) {
                    px += r.area();
                    marked = true;
                }
            }
        }
        done += n;
    }
    if over {
        klog::event!(Info, "fbdev", "damage.whole", pid = me.pid, yoffset = req.yoffset, count = req.count, read = done;
            "more damage than a whole copy costs: the window is copied whole, the rest of the list is not read");
    }
    if over && shown && !first {
        // `refresh` copies and presents the whole window (with what was copied already).
        PIXELS.add(px);
        fb.refresh(&mut scr);
        WHOLE.inc();
    } else if marked {
        let Screen { frame, guard, .. } = &mut *scr;
        if let Some(g) = guard.as_mut() {
            fb.show(g, frame);
        }
        PIXELS.add(px);
        super::account_flush(t0, px);
    }
    Ok(Vec::new())
}

impl Fb {
    /// fb0's frame shows window row `top` again where `r` lies (`r` already clipped to the window).
    pub(super) fn copy_rect(&self, frame: &mut display::RamFrame, top: u32, r: Rect) -> bool {
        let line = self.line();
        let w = frame.width as u64;
        let (x, y, rw, rh) = (r.x as u64, r.y as u64, r.w as u64, r.h as u64);
        let px = frame.pixels_mut();
        let ok = if x == 0 && rw == w && line == 4 * w {
            // Whole rows lie back to back in both: one copy.
            self.mem.read_pixels(((top as u64 + y) * line) as usize, &mut px[(y * w) as usize..((y + rh) * w) as usize])
        } else {
            (y..y + rh).all(|row| {
                let at = (row * w + x) as usize;
                self.mem.read_pixels(((top as u64 + row) * line + 4 * x) as usize, &mut px[at..at + rw as usize])
            })
        };
        if !ok {
            klog::event!(Error, "fbdev", "damage.copy_refused", top = top, x = r.x, y = r.y, w = r.w, h = r.h;
                "a damage rectangle lies outside the buffer (bug)");
            return false;
        }
        frame.mark(r);
        true
    }

    /// A pan to the window at `top` while the holder reports damage: only the rectangles reported for it are
    /// copied when there are such (true); false = the caller copies the whole window.
    pub(super) fn pan_damage(&self, scr: &mut Screen, top: u32) -> bool {
        if self.shared.damage_pid.load(Ordering::Acquire) == 0 {
            scr.reported.clear();
            return false;
        }
        let pending = core::mem::take(&mut scr.reported);
        let done = match pending.for_pan(top) {
            Some(rects) => {
                let t0 = time::now_ns();
                let mut px = 0;
                let Screen { frame, guard, .. } = &mut *scr;
                for &r in rects {
                    if self.copy_rect(frame, top, r) {
                        px += r.area();
                    }
                }
                // A pan presents even with nothing to copy: pacing counts it, a flip lands.
                if let Some(g) = guard.as_mut() {
                    self.show(g, frame);
                }
                PIXELS.add(px);
                PANS.inc();
                super::account_flush(t0, px);
                true
            }
            None => {
                WHOLE.inc();
                false
            }
        };
        scr.reported = pending;
        scr.reported.clear();
        done
    }

    /// Back to tracked stores (BITSOS_DAMAGE_END): every page write-protected, the window copied whole.
    pub(super) fn end_damage(&self, scr: &mut Screen, why: &'static str) {
        let pid = self.shared.damage_pid.swap(0, Ordering::AcqRel);
        scr.reported.clear();
        if pid == 0 {
            return;
        }
        self.shared.dirty.lock().clear();
        self.map.write_protect(0, self.shared.pages.saturating_sub(1));
        self.refresh(scr);
        WHOLE.inc();
        klog::event!(Info, "fbdev", "damage.off", pid = pid, why = why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_and_layout() {
        // _IOW('F', 0xB3, 24): 1 << 30 | 24 << 16 | 'F' << 8 | 0xB3.
        assert_eq!(FBIO_BITSOS_DAMAGE, (1 << 30) | ((SIZE as u64) << 16) | ((b'F' as u64) << 8) | 0xB3);
        let r = Request { flags: FLAG_END, yoffset: 800, count: 3, rects: 0x7fff_1234_5678 };
        assert_eq!(Request::from_bytes(&r.to_bytes()), Ok(r));
    }

    #[test]
    fn unknown_bits_are_refused() {
        let mut b = Request { flags: 0, yoffset: 0, count: 0, rects: 0 }.to_bytes();
        b[0] = 2;
        assert_eq!(Request::from_bytes(&b), Err(EINVAL));
        b[0] = 0;
        b[12] = 1;
        assert_eq!(Request::from_bytes(&b), Err(EINVAL));
    }

    #[test]
    fn rects_are_clipped() {
        let mut b = [0u8; RECT_SIZE];
        let put = |b: &mut [u8; RECT_SIZE], v: [u32; 4]| {
            for (i, x) in v.iter().enumerate() {
                b[4 * i..4 * i + 4].copy_from_slice(&x.to_le_bytes());
            }
        };
        put(&mut b, [100, 50, 2000, 2000]);
        assert_eq!(rect_in(&b, 1280, 800), Rect::new(100, 50, 1180, 750));
        put(&mut b, [u32::MAX, 0, u32::MAX, 10]);
        assert!(rect_in(&b, 1280, 800).is_empty());
        put(&mut b, [0, 0, 0, 10]);
        assert!(rect_in(&b, 1280, 800).is_empty());
    }

    #[test]
    fn windows() {
        assert!(window_ok(0, 800, 800));
        assert!(window_ok(800, 800, 1600));
        assert!(!window_ok(801, 800, 1600));
        assert!(!window_ok(u32::MAX, 800, 1600));
    }

    #[test]
    fn pending_follows_one_window() {
        let mut p = Pending::default();
        assert_eq!(p.for_pan(0), None, "nothing reported: whole");
        p.start(800, 1 << 20);
        p.add(Rect::new(1, 2, 3, 4));
        p.add(Rect::new(0, 0, 0, 4));
        assert_eq!(p.for_pan(800), Some(&[Rect::new(1, 2, 3, 4)][..]));
        assert_eq!(p.for_pan(0), None, "another window: whole");
        p.start(800, 1 << 20);
        p.add(Rect::new(5, 6, 7, 8));
        assert_eq!(p.for_pan(800).map(|r| r.len()), Some(2), "same window: reports add up");
        p.start(0, 1 << 20);
        assert_eq!(p.for_pan(0), Some(&[][..]), "a new window drops the old reports");
        p.add(Rect::new(1, 1, 1, 1));
        p.mark_whole();
        p.add(Rect::new(2, 2, 2, 2));
        assert_eq!(p.for_pan(0), None, "whole stays whole");
        p.clear();
        assert_eq!(p.for_pan(0), None);
    }

    #[test]
    fn work_is_bounded() {
        // A call's budget and the waiting list give way to a whole copy, never to an error.
        assert_eq!(budget(1280, 800), 2 * 1280 * 800);
        assert!(cost(Rect::new(0, 0, 0, 0)) > 0, "an empty rectangle is read too");
        assert!(cost(Rect::new(0, 0, 1280, 800)) > 1280 * 800);
        let window = 1280u64 * 800;
        let mut p = Pending::default();
        p.start(800, window);
        let small = Rect::new(0, 0, 8, 8);
        let mut n = 0u64;
        while !p.is_whole() {
            p.add(small);
            n += 1;
            assert!(n <= window, "the list never gives way");
        }
        assert!(n <= window / cost(small) + 1, "{n} rectangles before the whole copy");
        assert_eq!(p.for_pan(800), None, "too much damage: the pan copies whole");
        // Empty rectangles cost too: a list of nothing cannot grow the work without end.
        p.clear();
        p.start(800, window);
        let mut m = 0u64;
        while !p.is_whole() {
            p.add(Rect::new(5, 5, 0, 0));
            m += 1;
            assert!(m <= window, "empty rectangles never give way");
        }
        // A full-window report fits a pan exactly once: a second one is more than a whole copy.
        p.clear();
        p.start(800, cost(Rect::new(0, 0, 1280, 800)));
        p.add(Rect::new(0, 0, 1280, 800));
        assert_eq!(p.for_pan(800).map(|r| r.len()), Some(1));
        p.add(Rect::new(0, 0, 1280, 800));
        assert_eq!(p.for_pan(800), None);
    }
}
