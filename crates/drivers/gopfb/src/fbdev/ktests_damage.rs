//! In-kernel tests of DP-6 (`dp.fbdev.damage*`): `abi-fbdev frames` draws frames into its mapping and tells the
//! kernel each one: by fsync (the tracked path, plain programs), by FBIO_BITSOS_DAMAGE for the shown window
//! (bitscomp with one window) or for the hidden one before FBIOPAN_DISPLAY (two windows). With damage reports no
//! write fault and no device fault may happen after the warm-up, and the picture is the program's; the plain
//! run after a damage run still faults (the hold's end armed the tracked path again). Each run reports faults,
//! write-protected pages and copied bytes per frame (event `dp ktest.fbdev_damage`): the numbers of the DP-6
//! decision (plan/notes/dp-DP-6.md). `dp.fbdev.damage_rules` checks the refusals a program cannot provoke on
//! itself (EPERM for a kernel caller, EBUSY for a non-holder) and the work bound of one call (a huge `count`
//! degrades to a whole copy after a bounded number of reads); `dp.fbdev.damage_end` ends the mode with
//! BITSOS_DAMAGE_END and expects write-protect, faults and one whole copy back. All Safe and rerunnable: every
//! program ends before the test returns.

use super::ktests::{fb, handed_back, need_program, pat, start, text, wait_for, Started, SEC};
use super::abi::UserIo;
use super::{damage, fix_node_owner, stats, Fb};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::Cell;
use display::Rect;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::{EBUSY, EPERM};

/// A damage request at `AT` whose rectangles (at `RECTS`, as many as asked for) are all `rect`; counts reads.
struct FakeReq {
    req: [u8; damage::SIZE],
    rect: [u8; damage::RECT_SIZE],
    reads: Cell<u64>,
}
const AT: u64 = 0x7200_0000;
const RECTS: u64 = 0x7300_0000;

impl FakeReq {
    fn new(count: u32, r: Rect) -> FakeReq {
        let req = damage::Request { flags: 0, yoffset: 0, count, rects: RECTS }.to_bytes();
        let mut rect = [0u8; damage::RECT_SIZE];
        for (i, v) in [r.x, r.y, r.w, r.h].iter().enumerate() {
            rect[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        FakeReq { req, rect, reads: Cell::new(0) }
    }
}

impl UserIo for FakeReq {
    fn read(&self, addr: u64, out: &mut [u8]) -> bool {
        self.reads.set(self.reads.get() + 1);
        if addr == AT && out.len() == damage::SIZE {
            out.copy_from_slice(&self.req);
            return true;
        }
        if addr < RECTS || (addr - RECTS) % damage::RECT_SIZE as u64 != 0 || out.len() % damage::RECT_SIZE != 0 {
            return false;
        }
        for c in out.chunks_exact_mut(damage::RECT_SIZE) {
            c.copy_from_slice(&self.rect);
        }
        true
    }
    fn write(&self, _: u64, _: &[u8]) -> bool {
        false
    }
}

fn report(fb: &Fb, io: &FakeReq, who: Option<&Started>) -> Result<Vec<u8>, i64> {
    fb.ioctl(damage::FBIO_BITSOS_DAMAGE, AT, io, who.map(|s| &s.p))
}

fn counter(name: &str) -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == name).map_or(0, |(_, v)| v)
}

#[derive(Clone, Copy, Default)]
struct Counts {
    write_faults: u64,
    dev_faults: u64,
    protected: u64,
    flush_px: u64,
    flushed_rows: u64,
    refreshes: u64,
    damage: damage::Stats,
}

fn counts() -> Counts {
    let s = stats();
    Counts {
        write_faults: s.write_faults,
        dev_faults: counter("mm.device.faults"),
        protected: counter("mm.device.protected_pages"),
        flush_px: counter("fbdev.flush_px"),
        flushed_rows: s.flushed_rows,
        refreshes: s.refreshes,
        damage: damage::stats(),
    }
}

/// What one run measured after the warm-up (deltas over `frames` frames).
struct Run {
    frames: u64,
    width: u32,
    height: u32,
    shape_px: u64,
    d: Counts,
    /// Damage mode after the frames, while the program still holds the screen.
    damaging: bool,
}

fn delta(a: &Counts, b: &Counts) -> Counts {
    let ds = |x: u64, y: u64| y.saturating_sub(x);
    Counts {
        write_faults: ds(a.write_faults, b.write_faults),
        dev_faults: ds(a.dev_faults, b.dev_faults),
        protected: ds(a.protected, b.protected),
        flush_px: ds(a.flush_px, b.flush_px),
        flushed_rows: ds(a.flushed_rows, b.flushed_rows),
        refreshes: ds(a.refreshes, b.refreshes),
        damage: damage::Stats {
            calls: ds(a.damage.calls, b.damage.calls),
            refused: ds(a.damage.refused, b.damage.refused),
            px: ds(a.damage.px, b.damage.px),
            rects: ds(a.damage.rects, b.damage.rects),
            whole: ds(a.damage.whole, b.damage.whole),
            pans: ds(a.damage.pans, b.damage.pans),
            entered: ds(a.damage.entered, b.damage.entered),
        },
    }
}

fn kill(s: &Started) {
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
}

/// The "drawn a= b= box=x,y,w,h shape=" line: the picture is pat(b) inside the box (everywhere for full),
/// pat(a) outside.
fn check(out: &str, w: u32, h: u32, px: &[u32]) -> Result<u64, String> {
    let l = out.lines().find(|l| l.starts_with("drawn ")).ok_or("no 'drawn' line")?;
    let f = |k: &str| l.split_whitespace().find_map(|x| x.strip_prefix(k)).ok_or_else(|| format!("no {k} in '{l}'"));
    let a: u32 = f("a=")?.parse().map_err(|_| "a=")?;
    let b: u32 = f("b=")?.parse().map_err(|_| "b=")?;
    let bx: alloc::vec::Vec<u32> = f("box=")?.split(',').filter_map(|v| v.parse().ok()).collect();
    kassert!(bx.len() == 4, "box in '{l}'");
    kassert!(px.len() == (w * h) as usize, "picture {} pixels, screen {w}x{h}", px.len());
    for y in 0..h {
        for x in 0..w {
            let inside = x >= bx[0] && x < bx[0] + bx[2] && y >= bx[1] && y < bx[1] + bx[3];
            let seed = if inside { b } else { a };
            let got = px[(y * w + x) as usize];
            if got != pat(seed, x, y) {
                return Err(format!("pixel ({x}, {y}) is {got:#08x}, the program drew {:#08x} (seed {seed}; {l})", pat(seed, x, y)));
            }
        }
    }
    Ok(bx[2] as u64 * bx[3] as u64)
}

/// One `abi-fbdev frames <seed> <n> <how> <shape>` run; None when two windows are not offered (pan).
fn run(seed: u32, n: u64, how: &str, shape: &str) -> Result<Option<Run>, String> {
    let fb = fb()?;
    let (seed_s, n_s) = (seed.to_string(), n.to_string());
    let s = start(&["abi-fbdev", "frames", &seed_s, &n_s, how, shape])?;
    let r = (|| -> Result<Option<Run>, String> {
        let warm = |o: &str| o.contains("first pid=") || o.contains("pan: unsupported");
        kassert!(wait_for(60 * SEC, || warm(&text(&s.out)) || s.p.has_exited()), "no 'first' line:\n{}{}", text(&s.out), text(&s.err));
        let o = text(&s.out);
        if o.contains("pan: unsupported") {
            return Ok(None);
        }
        kassert!(o.contains("first pid="), "ended early: {:?}\n{o}{}", s.p.status(), text(&s.err));
        if how != "plain" {
            kassert!(o.contains("rules EINVAL EINVAL EINVAL EFAULT"), "the damage refusals differ:\n{o}");
        }
        // The plain run's store claims the screen through the fbdev thread.
        kassert!(wait_for(10 * SEC, || fb.holder_pid() == s.p.pid), "fbdev holder {}, program pid {}", fb.holder_pid(), s.p.pid);
        kassert!(fb.damaging() == (how != "plain"), "damage mode {} for a {how} run", fb.damaging());
        let before = counts();
        s.p.send_signal(uapi::ab::signal::SIGUSR1);
        kassert!(wait_for(120 * SEC, || text(&s.out).contains("drawn ") || s.p.has_exited()), "no 'drawn' line:\n{}{}", text(&s.out), text(&s.err));
        let d = delta(&before, &counts());
        let damaging = fb.damaging();
        let (w, h, px) = display::snapshot().ok_or("no snapshot")?;
        let shape_px = check(&text(&s.out), w, h, &px).map_err(|e| format!("{how} {shape}: {e}"))?;
        Ok(Some(Run { frames: n, width: w, height: h, shape_px, d, damaging }))
    })();
    kill(&s);
    let r = r?;
    handed_back(fb, "the kill")?;
    kassert!(!fb.damaging(), "damage mode stayed on after the hold ended");
    if let Some(r) = &r {
        let d = &r.d;
        let line = r.width as u64 * 4;
        let window_pages = (r.height as u64 * line).div_ceil(4096);
        let copied = if how == "plain" { d.flush_px * 4 } else { d.damage.px * 4 };
        // Option A (dirty bits of the page-table entries, plan/notes/dp-DP-6.md) finds the pages the tracked path
        // faulted on, copies their rows like `flush`, and walks the whole window each frame with one shootdown.
        klog::event!(Info, "dp", "ktest.fbdev_damage", how = how, shape = shape, frames = r.frames, width = r.width, height = r.height,
            faults_per_frame = d.write_faults / r.frames, dev_faults_per_frame = d.dev_faults / r.frames,
            protected_per_frame = d.protected / r.frames, copied_bytes_per_frame = copied / r.frames,
            shape_bytes = r.shape_px * 4, rows_copied_per_frame = d.flushed_rows / r.frames, window_pages = window_pages,
            whole_copies = d.refreshes + d.damage.whole, damage_pans = d.damage.pans);
    }
    Ok(r)
}

fn need() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    fb()?;
    need_program()?;
    fix_node_owner();
    Ok(())
}

/// One window: damage reports cost no fault after the warm-up and copy exactly the reported rectangle; the tracked
/// path still faults for a plain program (and for the next holder after a damage run).
fn single() -> TestResult {
    need()?;
    for (i, shape) in ["box", "full"].into_iter().enumerate() {
        let seed = 40 + 10 * i as u32;
        let n = if shape == "box" { 6 } else { 3 };
        let r = run(seed, n, "damage", shape)?.ok_or("one window cannot be unsupported")?;
        kassert!(r.d.write_faults == 0, "{shape}: {} write faults in {n} reported frames", r.d.write_faults);
        kassert!(r.d.dev_faults == 0, "{shape}: {} device faults in {n} reported frames", r.d.dev_faults);
        kassert!(r.d.protected == 0, "{shape}: {} pages write-protected in damage mode", r.d.protected);
        kassert!(r.d.damage.px == n * r.shape_px, "{shape}: {} pixels copied, {n} frames of {}", r.d.damage.px, r.shape_px);
        kassert!(r.d.refreshes == 0 && r.d.damage.whole == 0, "{shape}: whole copies {} + {}", r.d.refreshes, r.d.damage.whole);
        let p = run(seed + 5, n, "plain", shape)?.ok_or("one window cannot be unsupported")?;
        kassert!(p.d.write_faults >= n, "{shape}: plain program after a damage run: {} write faults in {n} frames", p.d.write_faults);
        kassert!(p.d.protected > 0, "{shape}: plain program: nothing write-protected");
    }
    Ok(())
}
ktest!("dp.fbdev.damage", Safe, single);

/// Two windows: the damage of the hidden window waits for the pan to it, which copies only that.
fn panned() -> TestResult {
    need()?;
    let n = 6;
    let Some(r) = run(70, n, "pan", "box")? else {
        return ktest::skip("fb0 offers no second window here (yres_virtual)");
    };
    kassert!(r.d.write_faults == 0 && r.d.dev_faults == 0, "{} write / {} device faults in {n} panned frames", r.d.write_faults, r.d.dev_faults);
    kassert!(r.d.damage.pans == n, "{} of {n} pans copied only the damage", r.d.damage.pans);
    kassert!(r.d.damage.px == n * r.shape_px, "{} pixels copied, {n} frames of {}", r.d.damage.px, r.shape_px);
    kassert!(r.d.refreshes == 0, "{} whole-window copies", r.d.refreshes);
    Ok(())
}
ktest!("dp.fbdev.damage_pan", Safe, panned);

/// The refusals and the work bound of one call, with `abi-fbdev frames ... damage box` holding the screen.
fn rules() -> TestResult {
    need()?;
    let fb = fb()?;
    let s = start(&["abi-fbdev", "frames", "90", "1", "damage", "box"])?;
    let idle = start(&["abi-fbdev", "idle"]);
    let r = (|| -> TestResult {
        let idle = idle.as_ref().map_err(|e| e.clone())?;
        kassert!(wait_for(60 * SEC, || text(&s.out).contains("first pid=") || s.p.has_exited()), "no 'first' line:\n{}{}", text(&s.out), text(&s.err));
        kassert!(wait_for(10 * SEC, || text(&idle.out).contains("idle pid=")), "idle did not start:\n{}", text(&idle.err));
        kassert!(fb.holder_pid() == s.p.pid && fb.damaging(), "holder {} (program {}), damage mode {}", fb.holder_pid(), s.p.pid, fb.damaging());
        let (w, h) = (fb.g().width, fb.g().height);
        let one = FakeReq::new(0, Rect::new(0, 0, 0, 0));
        kassert!(report(fb, &one, None) == Err(EPERM), "a kernel caller holds no screen: {:?}", report(fb, &one, None));
        let before = counts();
        let busy = report(fb, &one, Some(idle));
        kassert!(busy == Err(EBUSY), "a report from a non-holder while another one has the screen: {busy:?}");
        let d = delta(&before, &counts());
        kassert!(d.damage.refused == 1, "{} refusals counted", d.damage.refused);
        kassert!(fb.holder_pid() == s.p.pid && fb.damaging(), "the refusal changed the holder ({}) or the mode ({})", fb.holder_pid(), fb.damaging());
        // A huge count of whole-window rectangles: the second one already costs more than the whole copy.
        let full = FakeReq::new(u32::MAX, Rect::new(0, 0, w, h));
        let before = counts();
        kassert!(report(fb, &full, Some(&s)).is_ok(), "the bounded call is no error");
        let d = delta(&before, &counts());
        kassert!(full.reads.get() <= 3, "{} reads for whole-window rectangles", full.reads.get());
        kassert!(d.damage.whole == 1 && d.refreshes == 1, "whole copies {} / refreshes {}", d.damage.whole, d.refreshes);
        // A huge count of empty rectangles: they cost too, so the reads stop at the budget.
        let empty = FakeReq::new(u32::MAX, Rect::new(w, h, 0, 0));
        let before = counts();
        kassert!(report(fb, &empty, Some(&s)).is_ok(), "the bounded call is no error");
        let d = delta(&before, &counts());
        let most = damage::budget(w, h) / damage::cost(Rect::new(0, 0, 0, 0)) / damage::STEP as u64 + 2;
        kassert!(empty.reads.get() <= most, "{} reads for empty rectangles (bound {most})", empty.reads.get());
        kassert!(d.damage.whole == 1 && d.damage.px == 0, "whole copies {} / pixels {}", d.damage.whole, d.damage.px);
        kassert!(fb.damaging(), "the bound ended damage mode");
        // The program's own frame still lands and the picture is complete.
        s.p.send_signal(uapi::ab::signal::SIGUSR1);
        kassert!(wait_for(120 * SEC, || text(&s.out).contains("drawn ") || s.p.has_exited()), "no 'drawn' line:\n{}{}", text(&s.out), text(&s.err));
        let (sw, sh, px) = display::snapshot().ok_or("no snapshot")?;
        check(&text(&s.out), sw, sh, &px)?;
        Ok(())
    })();
    if let Ok(i) = &idle {
        kill(i);
    }
    kill(&s);
    r?;
    handed_back(fb, "the kill")?;
    kassert!(!fb.damaging(), "damage mode stayed on after the hold ended");
    Ok(())
}
ktest!("dp.fbdev.damage_rules", Safe, rules);

/// BITSOS_DAMAGE_END while the holder keeps the screen: every page write-protected, one whole copy, and the next
/// frames fault and are tracked again (fsync).
fn ended() -> TestResult {
    need()?;
    let n = 3;
    let r = run(100, n, "end", "box")?.ok_or("one window cannot be unsupported")?;
    let window_pages = (r.height as u64 * r.width as u64 * 4).div_ceil(4096);
    kassert!(!r.damaging, "damage mode still on after END");
    kassert!(r.d.damage.whole >= 1, "END copied nothing whole");
    kassert!(r.d.protected >= window_pages, "{} pages write-protected by END, the window has {window_pages}", r.d.protected);
    kassert!(r.d.write_faults >= n, "{} write faults in {n} tracked frames after END", r.d.write_faults);
    kassert!(r.d.damage.px == 0 && r.d.damage.pans == 0, "damage copies after END: {} px, {} pans", r.d.damage.px, r.d.damage.pans);
    Ok(())
}
ktest!("dp.fbdev.damage_end", Safe, ended);
