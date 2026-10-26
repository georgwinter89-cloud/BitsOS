//! IN-6 (B4): the pointer over a fullscreen program, drawn by the kernel without the program's help. While a
//! program has the screen (KD_GRAPHICS) the console frame is not shown, so the arrow cannot live there: each
//! move takes the patches under the old and the new arrow from the picture the screen shows (`display::
//! with_screen`: the program's own frame, or the console frame when no program holds the scanout), draws the
//! arrow into the new one and copies them to the screen. The program's picture is never changed. The copy goes
//! through the console's RAM frame as a scratch (the device copies rectangles of a full frame): the patch is put
//! in, presented and the old pixels put back, all under `display.frame`, so nobody ever sees the scratch.
//!
//! A program that presents over the arrow hides it until the next move; `after_present` (for DP's hook, change
//! request IN-6/1) draws it again after each program present. Lock order: `vt.pointer` (sleeping, taken only
//! here) > the owner's picture lock > `display.frame`; never under `console.vt`.

use crate::latency::{self, Path};
use crate::render::{ARROW_H, ARROW_W, arrow_pixel};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use devmodel::display::DisplayDevice;
use display::{RamFrame, Rect};
use ksync::Mutex;

/// Where the arrow is on the screen over a program's picture (x, y, w, h by 16 bits; 0 = nowhere).
static DEV_AT: AtomicU64 = AtomicU64::new(0);
/// Its hot spot and scale for `after_present`: x and y by 24 bits, scale by 15, bit 63 = shown.
static SHOWN: AtomicU64 = AtomicU64::new(0);
/// Why the console draws no pointer over programs: `OFF` (`set_enabled(false)`) and `GRABBED` (a program grabbed
/// a pointer device on the machine's sink and draws its own pointer: the compositor, WY-1a change request 2 = IN-6
/// change request 4). One atomic for both, so a switch decides from the state it changed (fixer, checker finding 3).
static STATE: AtomicU8 = AtomicU8::new(0);
const OFF: u8 = 1;
const GRABBED: u8 = 2;
/// Tests: keep the last move's patches (what went to the screen).
static RECORD: AtomicBool = AtomicBool::new(false);

struct Scratch {
    patch: Vec<u32>,
    keep: Vec<u32>,
    last: Vec<(Rect, Vec<u32>)>,
}

static SCRATCH: Mutex<Scratch> = Mutex::named("vt.pointer", Scratch { patch: Vec::new(), keep: Vec::new(), last: Vec::new() });

klog::counter!(DRAWS, "input.pointer_over.draws");
klog::counter!(MISSES, "input.pointer_over.misses");
static MISS_LOGGED: AtomicBool = AtomicBool::new(false);

fn pack(r: Rect) -> u64 {
    if r.is_empty() || r.x > 0xFFFF || r.y > 0xFFFF || r.w > 0xFFFF || r.h > 0xFFFF {
        return 0;
    }
    (r.x as u64) << 48 | (r.y as u64) << 32 | (r.w as u64) << 16 | r.h as u64
}

fn unpack(v: u64) -> Option<Rect> {
    let r = Rect::new((v >> 48) as u32, (v >> 32 & 0xFFFF) as u32, (v >> 16 & 0xFFFF) as u32, (v & 0xFFFF) as u32);
    (!r.is_empty()).then_some(r)
}

fn shown() -> Option<(u32, u32, u32)> {
    let v = SHOWN.load(Ordering::Acquire);
    (v >> 63 == 1).then_some(((v & 0xFF_FFFF) as u32, (v >> 24 & 0xFF_FFFF) as u32, (v >> 48 & 0x7FFF) as u32))
}

/// The arrow's rectangle at hot spot (x, y), clipped to the screen.
pub fn arrow_rect(x: u32, y: u32, scale: u32, sw: u32, sh: u32) -> Rect {
    Rect::new(x, y, ARROW_W * scale, ARROW_H * scale).clip(sw, sh)
}

/// The rectangles a move from `old` to `new` copies: one when they meet, else both.
pub fn plan(old: Option<Rect>, new: Rect) -> [Option<Rect>; 2] {
    match old {
        Some(o) if o.intersects(new) || o.touches(new) => [Some(o.union(new)), None],
        Some(o) => [Some(o), Some(new)],
        None => [Some(new), None],
    }
}

/// Draws the arrow (hot spot `x`, `y`) into `patch`, which holds rectangle `r` of the screen.
pub fn draw_arrow(patch: &mut [u32], r: Rect, x: u32, y: u32, scale: u32) {
    let a = Rect::new(x, y, ARROW_W * scale, ARROW_H * scale);
    for py in 0..r.h {
        let sy = r.y + py;
        if sy < a.y || sy >= a.y.saturating_add(a.h) {
            continue;
        }
        for px in 0..r.w {
            let sx = r.x + px;
            if sx < a.x || sx >= a.x.saturating_add(a.w) {
                continue;
            }
            if let (Some(c), Some(d)) = (arrow_pixel(scale, sx - x, sy - y), patch.get_mut((py * r.w + px) as usize)) {
                *d = c;
            }
        }
    }
}

/// Copies rectangle `r` of a full picture (`w` x `h`) into `out` (row by row); false when `r` lies outside.
pub fn copy_out(px: &[u32], w: u32, h: u32, r: Rect, out: &mut Vec<u32>) -> bool {
    out.clear();
    if r.x as u64 + r.w as u64 > w as u64 || r.y as u64 + r.h as u64 > h as u64 {
        return false;
    }
    for row in r.y..r.y + r.h {
        let at = row as usize * w as usize + r.x as usize;
        match px.get(at..at + r.w as usize) {
            Some(s) => out.extend_from_slice(s),
            None => return false,
        }
    }
    true
}

/// `copy_out` into a new vector (tests); None outside or without memory.
pub fn copy_out_vec(px: &[u32], w: u32, h: u32, r: Rect) -> Option<Vec<u32>> {
    let mut v = Vec::new();
    v.try_reserve((r.w * r.h) as usize).ok()?;
    copy_out(px, w, h, r, &mut v).then_some(v)
}

/// Puts `patch` (rectangle `r`) into `f`, copies `r` to `dev` and puts `f`'s pixels back. No damage is marked:
/// `f` is the same afterwards. Runs under `display.frame` when `f` is the console frame.
pub fn blit(dev: &dyn DisplayDevice, f: &mut RamFrame, r: Rect, patch: &[u32], keep: &mut Vec<u32>) -> bool {
    let (w, h) = (f.width, f.height);
    if patch.len() != (r.w * r.h) as usize || !copy_out(f.pixels(), w, h, r, keep) {
        return false;
    }
    let put = |f: &mut RamFrame, src: &[u32]| {
        let px = f.pixels_mut();
        for (i, row) in (r.y..r.y + r.h).enumerate() {
            let at = row as usize * w as usize + r.x as usize;
            px[at..at + r.w as usize].copy_from_slice(&src[i * r.w as usize..(i + 1) * r.w as usize]);
        }
    };
    put(f, patch);
    let ok = dev.present(f.pixels(), w, (r.x, r.y, r.w, r.h)).is_ok();
    put(f, keep);
    ok
}

/// Where the console frame's arrow was when it came out of a frame nobody shows (`move_to`'s `stale`) and the
/// screen has not got that spot back yet (a move failed): the next move restores it.
static STALE: AtomicU64 = AtomicU64::new(0);
/// Bumped by `forget` (the console takes the screen back, or the pointer is switched off): a move decided before
/// draws nothing and remembers nothing, so no arrow lands on the console's screen.
static GEN: AtomicU64 = AtomicU64::new(0);

/// The generation a move is decided under: read before `vt::graphics()` (KD_TEXT clears that before `forget`).
pub fn generation() -> u64 {
    GEN.load(Ordering::SeqCst)
}

/// What a step of a move came to.
enum Step {
    Sent,
    /// The console took the screen back meanwhile: nothing more to do (it repaints the whole screen).
    Gone,
    Failed(&'static str),
}

fn meets(a: Rect, b: Rect) -> bool {
    a.intersects(b) || a.touches(b)
}

/// Copies rectangle `r` of the screen's picture with the arrow at (x, y) drawn in to the screen, unless the
/// console took the screen back since generation `epoch`.
#[allow(clippy::too_many_arguments)]
/// `arrow`: hot spot and scale of the arrow drawn into the patch; None sends the program's picture alone.
fn send(dev: &dyn DisplayDevice, s: &mut Scratch, r: Rect, arrow: Option<(u32, u32, u32)>, sw: u32, sh: u32, epoch: u64) -> Step {
    let n = (r.w * r.h) as usize;
    let Scratch { patch, keep, .. } = s;
    if patch.try_reserve(n).is_err() || keep.try_reserve(n).is_err() {
        return Step::Failed("no memory for the patch");
    }
    if !display::with_screen(|px, w, h| (w, h) == (sw, sh) && copy_out(px, w, h, r, patch)).unwrap_or(false) {
        return Step::Failed("no picture of the screen's size");
    }
    if let Some((x, y, scale)) = arrow {
        draw_arrow(patch, r, x, y, scale);
    }
    // The generation is checked under `display.frame`: the console's repaint after KD_TEXT takes that lock after
    // `forget` bumped it, so a patch either goes before that repaint (which covers it) or not at all.
    match display::with_frame(|f| {
        if GEN.load(Ordering::SeqCst) != epoch {
            Step::Gone
        } else if (f.width, f.height) == (sw, sh) && blit(dev, f, r, patch, keep) {
            Step::Sent
        } else {
            Step::Failed("the screen refused the patch")
        }
    }) {
        Some(st) => st,
        None => Step::Failed("no display frame"),
    }
}

/// The pointer moved to (x, y) while a program has the screen (`sw` x `sh`); `stale` is where the console frame
/// had the arrow (now removed there) when the screen was handed over, so the screen gets that spot back too;
/// `epoch` is `generation()` read before the caller saw KD_GRAPHICS. Called by the input thread with no console
/// lock held. A failed move keeps what is on the screen remembered (the arrow's old spot, a stale spot), so the
/// next move still puts the program's picture back there.
#[allow(clippy::too_many_arguments)]
pub fn move_to(x: u32, y: u32, scale: u32, sw: u32, sh: u32, stale: Option<Rect>, epoch: u64) {
    let keep_stale = |st: Option<Rect>| {
        if let Some(st) = st {
            let had = unpack(STALE.load(Ordering::Acquire));
            STALE.store(pack(had.map_or(st, |h| h.union(st))), Ordering::Release);
        }
    };
    if !enabled() {
        return;
    }
    let Some(dev) = display::device() else { return };
    // Taken only by the input thread and tests; bounded so the input thread (KernelRt) never waits long.
    let Some(mut s) = SCRATCH.lock_timeout(250_000_000) else {
        keep_stale(stale);
        miss("scratch busy for 250 ms");
        return;
    };
    if GEN.load(Ordering::SeqCst) != epoch {
        return;
    }
    let stale = match (stale, unpack(STALE.swap(0, Ordering::AcqRel))) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (a, b) => a.or(b),
    }
    .map(|r| r.clip(sw, sh))
    .filter(|r| !r.is_empty());
    let new = arrow_rect(x, y, scale, sw, sh);
    let old = unpack(DEV_AT.load(Ordering::Acquire)).map(|o| o.clip(sw, sh)).filter(|o| !o.is_empty());
    // At most three patches, no heap: the stale spot joins a patch it meets, else goes first on its own.
    let [mut a, mut b] = plan(old, new);
    let mut first = None;
    if let Some(st) = stale {
        match (a, b) {
            (Some(p), _) if meets(p, st) => a = Some(p.union(st)),
            (_, Some(p)) if meets(p, st) => b = Some(p.union(st)),
            _ => first = Some(st),
        }
    }
    let record = RECORD.load(Ordering::Acquire);
    s.last.clear();
    let stamp = latency::leaving();
    let (mut sent, mut stale_sent) = (false, false);
    for r in [first, a, b].into_iter().flatten().filter(|r| !r.is_empty()) {
        match send(dev, &mut s, r, Some((x, y, scale)), sw, sh, epoch) {
            Step::Sent => {}
            Step::Gone => {
                latency::not_shown(stamp);
                return;
            }
            Step::Failed(why) => {
                // DEV_AT stays: the old arrow may still be on the screen. A stale spot not yet sent waits.
                if !stale_sent {
                    keep_stale(stale);
                }
                miss(why);
                latency::not_shown(stamp);
                return;
            }
        }
        sent = true;
        stale_sent |= stale.is_some_and(|st| r.union(st) == r);
        if record {
            record_patch(&mut s, r);
        }
    }
    remember(new, x, y, scale);
    if GEN.load(Ordering::SeqCst) != epoch {
        // `forget` ran meanwhile (and may have missed this spot): the console's whole repaint covers it.
        DEV_AT.store(0, Ordering::SeqCst);
        SHOWN.store(0, Ordering::SeqCst);
    }
    drop(s);
    if sent {
        DRAWS.inc();
        latency::left(stamp, Path::PointerOverProgram);
    } else {
        latency::not_shown(stamp);
    }
}

/// Tests: keeps the patch just sent for `r` (`last_patches`).
fn record_patch(s: &mut Scratch, r: Rect) {
    if s.last.try_reserve(1).is_ok() {
        let mut copy = Vec::new();
        if copy.try_reserve(r.w as usize * r.h as usize).is_ok() {
            copy.extend_from_slice(&s.patch);
            s.last.push((r, copy));
        }
    }
}

/// The arrow is on the screen at `at` (hot spot x, y; `scale`) over a program's picture.
pub(crate) fn remember(at: Rect, x: u32, y: u32, scale: u32) {
    DEV_AT.store(pack(at), Ordering::SeqCst);
    SHOWN.store(1 << 63 | ((scale as u64 & 0x7FFF) << 48) | ((y as u64 & 0xFF_FFFF) << 24) | (x as u64 & 0xFF_FFFF), Ordering::SeqCst);
}

fn miss(why: &'static str) {
    MISSES.inc();
    if !MISS_LOGGED.swap(true, Ordering::AcqRel) {
        klog::event!(Warn, "console", "pointer_over.miss", why = why; "the pointer over a program was not drawn (counted from here on)");
    }
}

/// The console takes the screen back: the arrow over the program is gone with the program's picture. Returns
/// where it was, so the caller has that spot copied again from the console frame. A move in flight draws nothing
/// more (`GEN`).
pub fn forget() -> Option<Rect> {
    GEN.fetch_add(1, Ordering::SeqCst);
    STALE.store(0, Ordering::Release);
    SHOWN.store(0, Ordering::SeqCst);
    unpack(DEV_AT.swap(0, Ordering::SeqCst))
}

/// Where the arrow is drawn over a program now (tests, MCP).
pub fn drawn_at() -> Option<Rect> {
    unpack(DEV_AT.load(Ordering::Acquire))
}

/// Whether the console draws the pointer over a program now: not switched off and no pointer grabbed.
pub fn enabled() -> bool {
    STATE.load(Ordering::Acquire) == 0
}

/// Whether a program holds the grab of a pointer device (IN-6b).
pub fn grabbed() -> bool {
    STATE.load(Ordering::Acquire) & GRABBED != 0
}

/// Sets or clears one reason; returns the state before and after, read and written in one step.
fn switch(bit: u8, set: bool) -> (u8, u8) {
    let old = if set { STATE.fetch_or(bit, Ordering::AcqRel) } else { STATE.fetch_and(!bit, Ordering::AcqRel) };
    (old, if set { old | bit } else { old & !bit })
}

/// Off: a program that draws its own pointer (the compositor, WY) asks for none. Called where waiting up to
/// 250 ms for the input thread's scratch is fine (a syscall, a ktest), never from the input path.
pub fn set_enabled(on: bool) {
    let (old, new) = switch(OFF, !on);
    if old != new {
        klog::event!(Info, "console", "pointer_over.enabled", on = on);
        if old == 0 {
            clear();
        }
    }
}

/// The evdev sink's grabs changed (called under its grab lock, so calls come in the order of the changes):
/// `any` = some pointer device is grabbed now. True when this switched the pointer off: the caller then runs
/// `clear` with no lock held.
pub fn set_grabbed(any: bool) -> bool {
    let (old, new) = switch(GRABBED, any);
    if old == new {
        return false;
    }
    klog::event!(Info, "console", "pointer_over.grabbed", grabbed = any;
        "a program that grabbed a pointer device draws its own pointer; the console's arrow over programs is off while it holds the grab");
    old == 0
}

/// The pointer went off: the arrow over a program is removed from the screen (the program's picture comes back
/// at its spot) and forgotten. A move waiting for the scratch draws nothing (`forget` bumps `GEN`).
pub fn clear() {
    let Some(mut s) = SCRATCH.lock_timeout(250_000_000) else {
        let _ = forget();
        miss("scratch busy for 250 ms");
        return;
    };
    // A stale spot (the console's arrow when the program took the screen) not sent yet goes back too.
    let stale = unpack(STALE.load(Ordering::Acquire));
    let at = forget();
    let epoch = GEN.load(Ordering::SeqCst);
    if at.is_none() && stale.is_none() {
        return;
    }
    let (Some(dev), Some((sw, sh))) = (display::device(), display::with_screen(|_, w, h| (w, h))) else { return };
    let record = RECORD.load(Ordering::Acquire);
    s.last.clear();
    for r in [stale, at].into_iter().flatten().map(|r| r.clip(sw, sh)).filter(|r| !r.is_empty()) {
        match send(dev, &mut s, r, None, sw, sh, epoch) {
            Step::Sent if record => record_patch(&mut s, r),
            Step::Sent => {}
            Step::Gone => return,
            Step::Failed(why) => return miss(why),
        }
    }
}

/// Tests: keep the patches of each move (`last_patches`).
pub fn set_record(on: bool) {
    RECORD.store(on, Ordering::Release);
}

/// Tests: the patches the last move copied to the screen.
pub fn last_patches() -> Vec<(Rect, Vec<u32>)> {
    SCRATCH.lock_timeout(250_000_000).map(|s| s.last.clone()).unwrap_or_default()
}

/// For DP's hook after a program's frame went to the screen (change request IN-6/1): when `presented` covered
/// the arrow, it is drawn again on top (into `frame` for the copy, then the frame's pixels are put back). Takes
/// no lock of its own (the owner holds its frame's lock here; `vt.pointer` is outside that lock).
pub fn after_present(dev: &dyn DisplayDevice, frame: &mut RamFrame, presented: Rect) {
    let (Some(at), Some((x, y, scale))) = (unpack(DEV_AT.load(Ordering::Acquire)), shown()) else { return };
    if !at.intersects(presented) || !enabled() {
        return;
    }
    let n = (at.w * at.h) as usize;
    let (mut patch, mut keep) = (Vec::new(), Vec::new());
    if patch.try_reserve(n).is_err() || keep.try_reserve(n).is_err() {
        miss("no memory for the patch");
        return;
    }
    if !copy_out(frame.pixels(), frame.width, frame.height, at, &mut patch) {
        return;
    }
    draw_arrow(&mut patch, at, x, y, scale);
    if !blit(dev, frame, at, &patch, &mut keep) {
        miss("the screen refused the patch");
    }
}

/// The whole hook for DP (change requests IN-6/1 and IN-6/2): a scanout owner's frame went to the screen
/// (`presented` its damage's bounding box, the copy from `start_ns` to `done_ns`): the latency of input a program
/// read is closed, and the arrow is drawn again on top.
pub fn on_owner_present(dev: &dyn DisplayDevice, frame: &mut RamFrame, presented: Rect, start_ns: u64, done_ns: u64) {
    latency::program_frame(start_ns, done_ns);
    after_present(dev, frame, presented);
}
