//! In-kernel tests of SHOT (`dp.fbdev.shot`): MCP `screen` while a holder draws straight into the scanout buffers
//! gets one whole frame, never rows of two, and holds a buffer from the holder no longer than `HOLD_MAX_NS`.
//!
//! The fake engine of `dp.fbdev.scanout_engine` with two RAM pictures as its buffers. A kernel thread acts for the
//! program as fast as it can: BEGIN, one colour per frame into the buffer BEGIN named, pan. The test meanwhile takes
//! shots of the whole picture and of regions; the engine's reads are slowed down like device memory, so each shot
//! spans many of the holder's frames: without the hold (the code before SHOT) the holder drew into the buffer being
//! read and the shot held rows of two frames. Then the bound: a copy too slow for it ends with NotWhole and gives the
//! buffer back, and a holder whose BEGIN waited `HOLD_MAX_NS` goes on drawing and spoils that copy. Last, the
//! console takes the screen back while a BEGIN waits for a copy: that BEGIN is refused and leaves nothing behind.

use super::ktests::{fb, handed_back, need_program, start, text, wait_for, SEC};
use super::ktests_scanout::{draw_direct, pan_as, raw_as, Pics, ENGINE};
use super::scanout::{self, Args, OP_BEGIN};
use super::shot::HOLD_MAX_NS;
use super::{fix_node_owner, Fb, MS};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use display::{PictureError, Rect, ScanoutSource};
use ksync::SpinLock;
use ktest::{kassert, ktest, TestResult};

/// Frame `k`'s colour: no picture anything else could have drawn.
const fn frame_px(k: u64) -> u32 {
    0x0050_0000 | (k as u32 & 0xF_FFFF)
}
const fn is_frame(p: u32) -> bool {
    p & 0xFFF0_0000 == 0x0050_0000
}

/// What the holder thread did.
struct Run {
    stop: AtomicBool,
    frames: AtomicU64,
    /// The longest BEGIN since the test last reset it.
    max_begin_ns: AtomicU64,
    failed: SpinLock<Option<String>>,
}

fn holder(fb: &'static Arc<Fb>, p: Arc<proc::Process>, run: Arc<Run>, pics: &'static Pics) {
    let h = fb.g().height;
    let mut k = 0u64;
    while !run.stop.load(Ordering::Acquire) {
        let t = time::now_ns();
        let w = match raw_as(fb, Some(&p), Args { op: OP_BEGIN, fd: -1, ..Args::default() }) {
            Ok(a) => a.window as usize,
            Err(e) => {
                *run.failed.lock() = Some(format!("BEGIN after {k} frames: {e}"));
                return;
            }
        };
        run.max_begin_ns.fetch_max(time::now_ns() - t, Ordering::AcqRel);
        k += 1;
        for px in pics.bufs[w & 1].iter() {
            px.store(frame_px(k), Ordering::Relaxed);
        }
        if let Err(e) = pan_as(fb, &p, w as u32 * h) {
            *run.failed.lock() = Some(format!("pan after {k} frames: {e}"));
            return;
        }
        run.frames.store(k, Ordering::Release);
    }
}

/// One shot of `r`: one frame's colour in every pixel.
fn shot(fb: &Fb, r: Rect) -> Result<Result<u32, PictureError>, String> {
    let g = fb.g();
    let mut px = Vec::new();
    // The trait's copy (fbdev's own `copy_rect` copies damage into its frame).
    match ScanoutSource::copy_rect(fb, r, &mut px) {
        Ok(size) => {
            kassert!(size == (g.width, g.height) && px.len() == r.w as usize * r.h as usize, "{r:?}: size {size:?}, {} pixels", px.len());
            let first = px[0];
            let mixed = px.iter().filter(|&&p| p != first).count();
            kassert!(is_frame(first) && mixed == 0, "{r:?}: {mixed} of {} pixels are not frame {:#x}'s (rows of two frames)", px.len(), first);
            Ok(Ok(first))
        }
        Err(e) => Ok(Err(e)),
    }
}

fn steps(fb: &'static Arc<Fb>, run: &Arc<Run>) -> TestResult {
    let e = &ENGINE;
    let g = fb.g();
    let (w, h) = (g.width, g.height);
    let bands = u64::from(h.div_ceil(16));
    // A whole frame takes about 30 ms to read: many of the holder's frames, well inside the bound.
    e.delay_ns.store(30 * MS / bands, Ordering::Release);
    let held0 = super::shot::held_begins();
    let regions = [Rect::new(0, 0, w, h), Rect::new(0, 0, w, 180.min(h)), Rect::new(w / 3, h / 2, w / 3, h / 4), Rect::new(w - 7, h - 5, 7, 5)];
    let f0 = run.frames.load(Ordering::Acquire);
    let mut seen = Vec::new();
    for i in 0..24 {
        let r = regions[i % regions.len()];
        match shot(fb, r)? {
            Ok(p) => seen.push(p),
            Err(err) => return ktest::fail(&format!("shot {i} of {r:?}: {err:?}")),
        }
    }
    let frames = run.frames.load(Ordering::Acquire) - f0;
    let held = super::shot::held_begins() - held0;
    let max_begin = run.max_begin_ns.load(Ordering::Acquire);
    let failed = run.failed.lock().clone();
    kassert!(failed.is_none(), "the holder: {failed:?}");
    seen.dedup();
    klog::event!(Info, "dp", "ktest.shot", shots = 24, frames = frames, held_begins = held, different = seen.len(), max_begin_ms = max_begin / MS);
    // The proof that the shots overlapped the drawing (a loaded host draws fewer frames, the holder waits anyway).
    kassert!(frames >= 16, "the holder drew only {frames} frames during 24 shots: the test proves nothing");
    kassert!(held >= 12, "only {held} BEGINs waited for a copy: the shots did not overlap the drawing");
    kassert!(seen.len() >= 8, "only {} different frames in 24 shots", seen.len());
    kassert!(max_begin < HOLD_MAX_NS + 30 * MS, "a BEGIN waited {} ms", max_begin / MS);

    // The look's own bound: 30 ms a band, so the copy gives up after about 100 ms and gives the buffer back.
    e.delay_ns.store(30 * MS, Ordering::Release);
    run.max_begin_ns.store(0, Ordering::Release);
    let t = time::now_ns();
    let r = shot(fb, Rect::new(0, 0, w, h))?;
    let took = time::now_ns() - t;
    kassert!(r == Err(PictureError::NotWhole), "a copy past the bound: {r:?} after {} ms", took / MS);
    kassert!(took >= HOLD_MAX_NS && took < HOLD_MAX_NS + 80 * MS, "it took {} ms", took / MS);
    kassert!(fb.direct.hold.reading().is_none(), "the buffer is still held after the copy gave up");
    let max_begin = run.max_begin_ns.load(Ordering::Acquire);
    kassert!(max_begin < HOLD_MAX_NS + 30 * MS, "a BEGIN waited {} ms", max_begin / MS);
    let f = run.frames.load(Ordering::Acquire);
    kassert!(wait_for(SEC, || run.frames.load(Ordering::Acquire) > f + 10), "the holder stopped drawing after the copy gave up");

    // The holder's bound: one band takes 150 ms; its BEGIN gives up waiting at 100 ms and spoils the copy.
    e.delay_ns.store(150 * MS, Ordering::Release);
    run.max_begin_ns.store(0, Ordering::Release);
    let r = shot(fb, Rect::new(0, 0, w, 16.min(h)))?;
    kassert!(r == Err(PictureError::NotWhole), "a copy the holder stopped waiting for: {r:?}");
    let max_begin = run.max_begin_ns.load(Ordering::Acquire);
    kassert!(max_begin >= HOLD_MAX_NS - 10 * MS && max_begin < HOLD_MAX_NS + 30 * MS, "the longest BEGIN waited {} ms", max_begin / MS);
    kassert!(fb.direct.hold.reading().is_none(), "the buffer is still held");
    e.delay_ns.store(0, Ordering::Release);
    kassert!(matches!(shot(fb, Rect::new(0, 0, w, h))?, Ok(_)), "a shot after the bounds");
    Ok(())
}

/// The console takes the screen back while the holder's BEGIN waits for a copy: that BEGIN must not mark the screen
/// as drawn straight into afterwards (the next holder's pans would be flips of buffers it never drew into), it is
/// refused. The release is called here directly, so it lands inside the wait: a shot reads one band for 150 ms.
fn taken_back_while_waiting(fb: &'static Arc<Fb>, p: &Arc<proc::Process>) -> TestResult {
    let e = &ENGINE;
    let w = fb.g().width;
    let h = fb.g().height;
    let shown = e.shown.load(Ordering::Acquire) as usize;
    e.delay_ns.store(150 * MS, Ordering::Release);
    let got: Arc<SpinLock<Option<Result<(u32, u32), PictureError>>>> = Arc::new(SpinLock::named("ktest.shot", None));
    let g2 = got.clone();
    let reader = sched::spawn("ktest-shot-reader", sched::Class::Normal, move || {
        let mut px = Vec::new();
        *g2.lock() = Some(ScanoutSource::copy_rect(&**fb, Rect::new(0, 0, w, 16.min(h)), &mut px));
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let r = (|| -> TestResult {
        kassert!(wait_for(SEC, || fb.direct.hold.reading() == Some(shown)), "the shot holds no buffer: {:?}", fb.direct.hold.reading());
        // The program's next frame goes into the other buffer and shows; its BEGIN after that names the held one.
        let a = raw_as(fb, Some(p), Args { op: OP_BEGIN, fd: -1, ..Args::default() }).map_err(|e| format!("BEGIN: {e}"))?;
        kassert!(a.window as usize == shown ^ 1, "BEGIN named {} while {shown} shows", a.window);
        pan_as(fb, p, a.window * h).map_err(|e| format!("pan: {e}"))?;
        let held0 = super::shot::held_begins();
        let begun: Arc<SpinLock<Option<Result<Args, i64>>>> = Arc::new(SpinLock::named("ktest.shot", None));
        let (b2, p2) = (begun.clone(), p.clone());
        let begin = sched::spawn("ktest-shot-begin", sched::Class::Normal, move || {
            let r = raw_as(fb, Some(&p2), Args { op: OP_BEGIN, fd: -1, ..Args::default() });
            *b2.lock() = Some(r);
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        sched::sleep_ns(20 * MS);
        let early = begun.lock().clone();
        kassert!(early.is_none(), "the BEGIN on the held buffer did not wait: {early:?}");
        let returned = {
            let mut scr = fb.screen.lock();
            fb.release(&mut scr, "ktest: the console takes the screen back")
        };
        fb.settle(returned);
        kassert!(begin.join_timeout(2 * SEC), "the BEGIN never came back");
        let b = begun.lock().clone();
        kassert!(super::shot::held_begins() == held0 + 1, "the BEGIN did not wait for the copy (held {} -> {})", held0, super::shot::held_begins());
        kassert!(b == Some(Err(uapi::errno::EBUSY)), "a BEGIN that waited while the screen went back: {b:?}");
        kassert!(!fb.direct.drawing(), "the screen is marked as drawn straight into after the console took it back");
        Ok(())
    })();
    kassert!(reader.join_timeout(2 * SEC), "the shot never came back");
    e.delay_ns.store(0, Ordering::Release);
    let shot = *got.lock();
    r?;
    kassert!(shot == Some(Err(PictureError::NotWhole)), "a shot during which the console took the screen: {shot:?}");
    kassert!(fb.direct.hold.reading().is_none(), "the buffer is still held");
    Ok(())
}

fn shots() -> TestResult {
    if display::device().is_none() {
        return ktest::fail("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let Some((_, target)) = crate::live() else {
        return ktest::fail("no live framebuffer");
    };
    let Some((_, phys)) = target.window() else {
        return ktest::fail("the framebuffer is no mapped window");
    };
    let g = fb.g();
    let e = &ENGINE;
    let pics = e.pics.get_or_init(|| {
        let n = g.width as usize * g.height as usize;
        Pics { w: g.width, h: g.height, bufs: [(0..n).map(|_| AtomicU32::new(0)).collect(), (0..n).map(|_| AtomicU32::new(0)).collect()] }
    });
    kassert!((pics.w, pics.h) == (g.width, g.height), "the pictures are {}x{}, the screen {}x{}", pics.w, pics.h, g.width, g.height);
    e.phys.store(phys, Ordering::Release);
    e.len.store(target.len() as u64, Ordering::Release);
    e.stride.store(g.line_length(), Ordering::Release);
    e.shown.store(0, Ordering::Release);
    e.stopped.store(false, Ordering::Release);
    e.whole.store(true, Ordering::Release);
    e.use_pics.store(true, Ordering::Release);
    kassert!(fb.holder_pid() == 0, "held at the start by {}", fb.holder_pid());
    scanout::stand_in_for_test(e, true);
    let r = (|| -> TestResult {
        let s = start(&["abi-fbdev", "idle"])?;
        let r = (|| -> TestResult {
            kassert!(wait_for(30 * SEC, || text(&s.out).contains("idle pid=") || s.p.has_exited()), "no 'idle' line: {}{}", text(&s.out), text(&s.err));
            kassert!(fb.take_for(s.p.pid), "the take");
            draw_direct(fb, &s)?;
            let run = Arc::new(Run { stop: AtomicBool::new(false), frames: AtomicU64::new(0), max_begin_ns: AtomicU64::new(0), failed: SpinLock::named("ktest.shot", None) });
            let (p, r2) = (s.p.clone(), run.clone());
            let t = sched::spawn("ktest-shot-holder", sched::Class::Normal, move || holder(fb, p, r2, pics)).map_err(|e| format!("spawn: {e:?}"))?;
            kassert!(wait_for(5 * SEC, || run.frames.load(Ordering::Acquire) >= 10), "the holder draws no frames: {:?}", run.failed.lock().clone());
            let r = steps(fb, &run);
            run.stop.store(true, Ordering::Release);
            kassert!(t.join_timeout(5 * SEC), "the holder thread did not stop");
            r?;
            // The release ends the hold: no END afterwards (it would be refused, the screen is the console's).
            taken_back_while_waiting(fb, &s.p)
        })();
        s.p.kill(uapi::ab::signal::SIGKILL);
        let _ = s.p.wait(Some(10 * SEC));
        r?;
        handed_back(fb, "the kill")
    })();
    e.delay_ns.store(0, Ordering::Release);
    e.use_pics.store(false, Ordering::Release);
    scanout::stand_in_for_test(e, false);
    r
}
ktest!("dp.fbdev.shot", Screen, shots);
