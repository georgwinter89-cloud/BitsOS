//! GX-3 (spec/gpu.md §3.1, §4 "waits", D2): double buffering by real page flips. Two scanout buffers in graphics
//! memory: A0 = the GOP framebuffer, A1 = A0 + 32 MB (the place gate 4 checked, V1 `DCN_BACK`; a BO reserves it).
//! Whoever writes "the screen" (the live GopFb: console, kernel pointer, panic) writes the buffer shown now
//! (`present_shown`); a frame (fbdev) goes to the hidden one between `begin` and `commit`, and `commit` orders the
//! flip (`flip::order`), which lands at the next VUPDATE. Every write into a buffer is remembered as a bounding
//! box per buffer, so `begin` repairs exactly what the hidden buffer lacks: what the other buffer got since its
//! own `begin` and what was drawn into this one over its last frame (the kernel pointer). Proof sketch: the hidden
//! buffer B equals frame n-1 outside W_B; frame n+1 differs from frame n-1 only inside damage n (in W_A) and
//! damage n+1 (presented after `begin`). This holds for one picture source: a new one (fbdev's take, a test tool)
//! presents its first frame whole, and the marks make both buffers equal to it within two frames.
//!
//! Waits are sleeps in thread context (B4: bounded by `LATE_NS` per surface); landing is checked by whoever
//! waits (no IH yet: GX-D6 needs bus master). A flip not landed `LATE_NS` after its order makes the surface fall
//! back to A0 and the copy path (logged) until `rearm` (fbdev's next take); V1 stayed on one image until restart.
//! Reload/reboot (`Quiesce`) puts A0 and the found flip types back, only if GX ever ordered a flip.

use crate::attach::{Attach, Pipe, Settle};
use crate::cursor::{self, CursorError, Shape};
use crate::flip::{self, FlipError as RegError, Rw, Types};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::display::{DisplayDevice, Fence, Mode, PixelFormat, VblankEvent};
use gopfb::GopFb;
use gopfb::fbdev::flip::{FlipError, Vblank};

/// A flip must land this long after its order (V1 `DCN_LIMIT`, spec/gpu.md §1.2), per surface.
pub const LATE_NS: u64 = 50_000_000;
/// After the expected vblank, look again every millisecond (a flip lands at VUPDATE, ~0.3 ms after vblank start
/// on the Mini-PC's 1481-line timing: spec/gpu.md §2.3).
const POLL_NS: u64 = 1_000_000;
const AFTER_VBLANK_NS: u64 = 500_000;

pub trait Clock: Send + Sync {
    fn now_ns(&self) -> u64;
    fn sleep_ns(&self, ns: u64);
}

/// The OTG's timing as the waits and the vblank clock use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// One line in picoseconds (H_TOTAL / pixel clock).
    pub line_ps: u64,
    pub v_total: u32,
    /// OTG_V_BLANK_START: the first line of vertical blanking.
    pub vblank_start: u32,
}

impl Timing {
    /// From totals, pixel clock (Hz) and the blank start; None if any is zero.
    pub fn new(h_total: u32, v_total: u32, pixel_hz: u64, vblank_start: u32) -> Option<Timing> {
        if h_total == 0 || v_total == 0 || pixel_hz == 0 {
            return None;
        }
        let line_ps = u64::from(h_total).checked_mul(1_000_000_000_000)? / pixel_hz;
        (line_ps != 0).then_some(Timing { line_ps, v_total, vblank_start: vblank_start % v_total })
    }
    pub fn period_ns(&self) -> u64 {
        self.line_ps * u64::from(self.v_total) / 1000
    }
    pub fn refresh_mhz(&self) -> u32 {
        let p = self.period_ns();
        if p == 0 { 0 } else { (1_000_000_000_000 / p).min(u64::from(u32::MAX)) as u32 }
    }
    /// Nanoseconds since the last vblank start, for the OTG on `line`.
    pub fn since_vblank_ns(&self, line: u32) -> u64 {
        let lines = (u64::from(line) + u64::from(self.v_total) - u64::from(self.vblank_start)) % u64::from(self.v_total);
        lines * self.line_ps / 1000
    }
}

/// A rectangle packed as x0, y0, x1, y1 by 16 bits (end exclusive; 0 = empty): one atomic per buffer.
pub fn pack(r: (u32, u32, u32, u32)) -> u64 {
    let (x, y, w, h) = r;
    if w == 0 || h == 0 {
        return 0;
    }
    let c = |v: u32| u64::from(v.min(0xFFFF));
    c(x) << 48 | c(y) << 32 | c(x.saturating_add(w)) << 16 | c(y.saturating_add(h))
}

pub fn unpack(v: u64) -> Option<(u32, u32, u32, u32)> {
    let (x0, y0, x1, y1) = ((v >> 48) as u32, (v >> 32 & 0xFFFF) as u32, (v >> 16 & 0xFFFF) as u32, (v & 0xFFFF) as u32);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

fn union(a: u64, b: u64) -> u64 {
    match (unpack(a), unpack(b)) {
        (None, _) => b,
        (_, None) => a,
        (Some(p), Some(q)) => {
            let (x0, y0) = (p.0.min(q.0), p.1.min(q.1));
            let (x1, y1) = ((p.0 + p.2).max(q.0 + q.2), (p.1 + p.3).max(q.1 + q.3));
            pack((x0, y0, x1 - x0, y1 - y0))
        }
    }
}

/// What was written into each buffer since its last `begin`, and writers inside it now.
struct Marks {
    written: [AtomicU64; 2],
    busy: [AtomicU32; 2],
    /// The size shown (GX-4: a mode switch changes it), packed `w << 32 | h`.
    size: AtomicU64,
}

impl Marks {
    fn size(&self) -> (u32, u32) {
        let v = self.size.load(Ordering::Acquire);
        ((v >> 32) as u32, v as u32)
    }
}

impl Marks {
    fn mark(&self, i: usize, r: (u32, u32, u32, u32)) {
        let p = pack(r);
        if p != 0 {
            let _ = self.written[i].try_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(union(v, p)));
        }
    }
}

/// One scanout buffer as a display device: presents land in its memory and are remembered.
pub struct BufDev {
    fb: GopFb,
    idx: usize,
    marks: Arc<Marks>,
}

impl DisplayDevice for BufDev {
    fn name(&self) -> &str {
        if self.idx == 0 { "dcn.a0" } else { "dcn.a1" }
    }
    fn modes(&self) -> Vec<Mode> {
        self.fb.modes()
    }
    fn current_mode(&self) -> Mode {
        let (width, height) = self.marks.size();
        Mode { width, height, ..self.fb.current_mode() }
    }
    fn format(&self) -> PixelFormat {
        self.fb.format()
    }
    fn present(&self, src: &[u32], stride: u32, damage: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
        self.marks.mark(self.idx, damage);
        self.fb.present(src, stride, damage)
    }
    fn last_vblank(&self) -> VblankEvent {
        self.fb.last_vblank()
    }
}

const READY: u8 = 1;
const FAILED: u8 = 2;
/// `prep`: no frame being prepared; CLAIMED = `begin` runs.
const NONE: u8 = 0xFF;
const CLAIMED: u8 = 0xFE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewError {
    /// The screen is wider or taller than the rectangle packing holds (65535).
    Size,
    /// Settling a pipe left on A1 (or pending) back to A0 failed.
    Settle(RegError),
    /// The settle flip did not land within `LATE_NS`.
    SettleLate,
}

/// Counts and state for the MCP tool (no counter on the flip path beyond the fence numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub flipping: bool,
    pub shown: u8,
    pub ordered: u64,
    pub landed: u64,
    pub late: u64,
    pub settled: bool,
}

pub struct Scanout<R: Rw, C: Clock> {
    regs: R,
    clock: C,
    pipes: Vec<Pipe>,
    gpu: [u64; 2],
    bufs: [BufDev; 2],
    marks: Arc<Marks>,
    timing: Timing,
    /// The OTG whose counter and position are the vblank clock (the first pipe's).
    otg: u32,
    cur: AtomicU8,
    prep: AtomicU8,
    state: AtomicU8,
    /// GX ordered at least one flip (the Quiesce restore runs only then).
    touched: AtomicBool,
    settled: bool,
    ordered: AtomicU64,
    ordered_at: AtomicU64,
    landed: AtomicU64,
    late: AtomicU64,
    /// The frame counter extended to 64 bits (24-bit wraps counted).
    frames: AtomicU64,
    lock_unseen_logged: AtomicBool,
    /// A fall-back could not order A0 (lock stuck, write mismatch): which buffer shows is not known, so presents
    /// of the shown picture go into both and `rearm` orders A0 again.
    a0_unsure: AtomicBool,
    /// One writer of the register sequence at a time (the address pair must not interleave, spec §4); GX-5a:
    /// it also holds the cursor's state (the cursor writes are register sequences too).
    pair: ksync::SpinLock<cursor::State>,
    /// GX-5a: the cursor surfaces (None: no hardware cursor, `NoDevice`).
    cursor_buf: Option<cursor::Buf>,
    /// GX-5a: the cursor registers were written (Quiesce turns the cursor off only then).
    cursor_used: AtomicBool,
    /// GX-4: the boot state of the mode registers (the way back for Quiesce), once a switch wrote them.
    mode_boot: [AtomicU32; crate::mode::SAVED],
    /// The GOP framebuffer's size the boot state shows.
    mode_home: AtomicU64,
    mode_changed: AtomicBool,
}

impl<R: Rw, C: Clock> Scanout<R, C> {
    /// Takes over the attached display: a pipe left on A1 or with a flip pending is settled on A0 first (V1 gate
    /// 8, which GX-2 only reported). `a0` and `a1` write the two buffers.
    pub fn new(regs: R, clock: C, attach: &Attach, a0: GopFb, a1: GopFb, timing: Timing) -> Result<Self, NewError> {
        let m = a0.current_mode();
        if m.width > 0xFFFF || m.height > 0xFFFF {
            return Err(NewError::Size);
        }
        let marks = Arc::new(Marks {
            written: [AtomicU64::new(0), AtomicU64::new(0)],
            busy: [AtomicU32::new(0), AtomicU32::new(0)],
            size: AtomicU64::new(u64::from(m.width) << 32 | u64::from(m.height)),
        });
        let otg = attach.pipes.first().map_or(0, |p| u32::from(p.otg));
        let mut s = Scanout {
            regs,
            clock,
            pipes: attach.pipes.clone(),
            gpu: [attach.a0, attach.a1],
            bufs: [BufDev { fb: a0, idx: 0, marks: marks.clone() }, BufDev { fb: a1, idx: 1, marks: marks.clone() }],
            marks,
            timing,
            otg,
            cur: AtomicU8::new(0),
            prep: AtomicU8::new(NONE),
            state: AtomicU8::new(READY),
            touched: AtomicBool::new(false),
            settled: false,
            ordered: AtomicU64::new(0),
            ordered_at: AtomicU64::new(0),
            landed: AtomicU64::new(0),
            late: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            lock_unseen_logged: AtomicBool::new(false),
            a0_unsure: AtomicBool::new(false),
            pair: ksync::SpinLock::named("amdgpu.flip", cursor::State::new()),
            cursor_buf: None,
            cursor_used: AtomicBool::new(false),
            mode_boot: core::array::from_fn(|_| AtomicU32::new(0)),
            mode_home: AtomicU64::new(0),
            mode_changed: AtomicBool::new(false),
        };
        if attach.settle != Settle::OnA0 || s.pipes.iter().any(|p| p.address != attach.a0) {
            s.settle()?;
        }
        // A1's content is unknown: the first frame into it is whole.
        s.marks.written[1].store(pack((0, 0, m.width, m.height)), Ordering::Release);
        Ok(s)
    }

    fn settle(&mut self) -> Result<(), NewError> {
        {
            let _g = self.pair.lock();
            flip::order(&self.regs, &self.pipes, self.gpu[0], Types::Found).map_err(NewError::Settle)?;
        }
        self.touched.store(true, Ordering::Release);
        let t0 = self.clock.now_ns();
        loop {
            if flip::landed(&self.regs, &self.pipes, self.gpu[0]).map_err(NewError::Settle)? {
                break;
            }
            if self.clock.now_ns().saturating_sub(t0) >= LATE_NS {
                return Err(NewError::SettleLate);
            }
            self.clock.sleep_ns(POLL_NS);
        }
        self.settled = true;
        klog::event!(Info, "dcn", "flip.settled", a0 = self.gpu[0], ms = (self.clock.now_ns() - t0) / 1_000_000;
            "a pipe left on A1 or pending by an earlier kernel shows A0 again");
        Ok(())
    }

    pub fn active(&self) -> bool {
        self.state.load(Ordering::Acquire) == READY
    }

    /// The live GopFb's presents go through `present_shown`: while flipping, and after a fall-back whose A0 order
    /// failed (the picture may still be A1).
    pub fn redirects(&self) -> bool {
        self.active() || self.a0_unsure.load(Ordering::Acquire)
    }

    pub fn state(&self) -> State {
        State {
            flipping: self.active(),
            shown: self.cur.load(Ordering::Acquire),
            ordered: self.ordered.load(Ordering::Acquire),
            landed: self.landed.load(Ordering::Acquire),
            late: self.late.load(Ordering::Acquire),
            settled: self.settled,
        }
    }

    /// The size shown now (the boot size until a mode switch).
    pub fn size(&self) -> (u32, u32) {
        self.marks.size()
    }

    /// The attached pipes (the mode switch handles one).
    pub fn pipes(&self) -> &[Pipe] {
        &self.pipes
    }

    /// Runs `f` on the registers as the one writer of a register sequence (the flip orders take the same lock):
    /// short, no sleep inside.
    pub fn with_pair<T>(&self, f: impl FnOnce(&R) -> T) -> T {
        let _g = self.pair.lock();
        f(&self.regs)
    }

    /// GX-4: keeps the boot state of the mode registers (before the first switch writes any) for Quiesce.
    pub fn keep_mode_boot(&self, boot: &crate::mode::PipeRegs, home: crate::mode::Size) {
        for (a, v) in self.mode_boot.iter().zip(boot.to_saved()) {
            a.store(v, Ordering::Release);
        }
        self.mode_home.store(u64::from(home.w) << 32 | u64::from(home.h), Ordering::Release);
        self.mode_changed.store(true, Ordering::Release);
    }

    /// GX-4: a switch now shows `w` x `h`: presents and the hidden buffer's repair follow it; both buffers are
    /// whole for the next frame.
    pub fn set_size(&self, w: u32, h: u32) {
        self.marks.size.store(u64::from(w) << 32 | u64::from(h), Ordering::Release);
        self.marks.written[0].store(pack((0, 0, w, h)), Ordering::Release);
        self.marks.written[1].store(pack((0, 0, w, h)), Ordering::Release);
        if let Err(e) = self.cursor_follow() {
            klog::event!(Warn, "dcn", "cursor.follow_failed", error = alloc::format!("{e:?}"), w = w, h = h;
                "the hardware cursor did not follow the mode switch");
        }
    }

    /// The boot state Quiesce would put back and the size it shows (None while no switch wrote the mode
    /// registers).
    pub fn mode_boot(&self) -> Option<(crate::mode::PipeRegs, crate::mode::Size)> {
        if !self.mode_changed.load(Ordering::Acquire) {
            return None;
        }
        let v = core::array::from_fn(|i| self.mode_boot[i].load(Ordering::Acquire));
        let l = self.mode_home.load(Ordering::Acquire);
        Some((crate::mode::PipeRegs::from_saved(v), crate::mode::Size::new((l >> 32) as u32, l as u32)))
    }
    pub fn timing(&self) -> Timing {
        self.timing
    }
    pub fn otg(&self) -> u32 {
        self.otg
    }
    pub fn regs(&self) -> &R {
        &self.regs
    }
    pub fn clock(&self) -> &C {
        &self.clock
    }
    /// The bounding box written into buffer `i` since its last `begin` (tests).
    pub fn written(&self, i: usize) -> Option<(u32, u32, u32, u32)> {
        unpack(self.marks.written[i & 1].load(Ordering::Acquire))
    }

    /// The live GopFb's present while flipping: into the buffer shown now. Lock-free (panic path): the busy count
    /// is raised before the buffer is chosen and checked again, so `begin` never repairs a buffer a writer still
    /// fills.
    pub fn present_shown(&self, src: &[u32], stride: u32, rect: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
        loop {
            let c = usize::from(self.cur.load(Ordering::SeqCst) & 1);
            self.marks.busy[c].fetch_add(1, Ordering::SeqCst);
            if usize::from(self.cur.load(Ordering::SeqCst) & 1) != c {
                self.marks.busy[c].fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let r = self.bufs[c].present(src, stride, rect);
            self.marks.busy[c].fetch_sub(1, Ordering::SeqCst);
            if self.a0_unsure.load(Ordering::Acquire) {
                // Not flipping (FAILED): no `begin` repairs either buffer, both get the picture.
                return self.bufs[1 - c].present(src, stride, rect).and(r);
            }
            return r;
        }
    }

    /// Waits for the last flip, repairs the hidden buffer from `src` and hands out its device.
    pub fn begin(&self, src: &[u32], stride: u32) -> Result<&BufDev, FlipError> {
        if !self.active() {
            return Err(FlipError::Off);
        }
        if self.prep.compare_exchange(NONE, CLAIMED, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(FlipError::Busy);
        }
        let seq = self.ordered.load(Ordering::Acquire);
        if seq != 0
            && let Err(e) = self.wait(seq)
        {
            self.prep.store(NONE, Ordering::Release);
            return Err(e);
        }
        let c = usize::from(self.cur.load(Ordering::SeqCst) & 1);
        let back = 1 - c;
        let t0 = self.clock.now_ns();
        while self.marks.busy[back].load(Ordering::SeqCst) != 0 && self.clock.now_ns().saturating_sub(t0) < LATE_NS {
            self.clock.sleep_ns(POLL_NS / 10);
        }
        let own = self.marks.written[back].swap(0, Ordering::AcqRel);
        let other = self.marks.written[c].load(Ordering::Acquire);
        for r in [unpack(own), unpack(other)].into_iter().flatten() {
            // Truth copies, not remembered (they make the buffer equal the frame there).
            let _ = self.bufs[back].fb.present(src, stride, r);
        }
        self.prep.store(back as u8, Ordering::Release);
        Ok(&self.bufs[back])
    }

    /// Orders the flip to the buffer `begin` prepared.
    pub fn commit(&self) -> Result<u64, FlipError> {
        let back = self.prep.load(Ordering::Acquire);
        if back > 1 {
            return Err(FlipError::Off);
        }
        let back = usize::from(back);
        self.bufs[back].fb.target().flush();
        let r = {
            let _g = self.pair.lock();
            flip::order(&self.regs, &self.pipes, self.gpu[back], Types::Vsync)
        };
        self.touched.store(true, Ordering::Release);
        match r {
            Ok(o) => {
                if !o.lock_seen && !self.lock_unseen_logged.swap(true, Ordering::AcqRel) {
                    klog::event!(Info, "dcn", "flip.lock_status", otg = self.otg, reads = flip::LOCK_READS;
                        "UPDATE_LOCK_STATUS not seen after the lock write (Linux goes on too); said once");
                }
                self.ordered_at.store(self.clock.now_ns(), Ordering::Release);
                self.cur.store(back as u8, Ordering::SeqCst);
                let seq = self.ordered.fetch_add(1, Ordering::AcqRel) + 1;
                self.prep.store(NONE, Ordering::Release);
                Ok(seq)
            }
            Err(e) => {
                self.prep.store(NONE, Ordering::Release);
                self.fall_back("order", Some(e));
                Err(FlipError::Late)
            }
        }
    }

    /// Whether the newest order landed (and with it every older one).
    fn check(&self) -> Result<bool, RegError> {
        let o = self.ordered.load(Ordering::Acquire);
        if self.landed.load(Ordering::Acquire) >= o {
            return Ok(true);
        }
        let c = usize::from(self.cur.load(Ordering::Acquire) & 1);
        let done = flip::landed(&self.regs, &self.pipes, self.gpu[c])?;
        if done {
            self.landed.fetch_max(o, Ordering::AcqRel);
        }
        Ok(done)
    }

    /// Sleeps until flip `seq` landed: first to the expected vblank, then every millisecond, at most until
    /// `LATE_NS` after its order; then the surface falls back.
    pub fn wait(&self, seq: u64) -> Result<(), FlipError> {
        if seq > self.ordered.load(Ordering::Acquire) {
            return Err(FlipError::Off);
        }
        let first = self.next_vblank_ns().map(|t| t + AFTER_VBLANK_NS);
        let mut slept_to_vblank = false;
        loop {
            if self.landed.load(Ordering::Acquire) >= seq {
                return Ok(());
            }
            if !self.active() {
                return Err(FlipError::Late);
            }
            match self.check() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    self.fall_back("read", Some(e));
                    return Err(FlipError::Late);
                }
            }
            let now = self.clock.now_ns();
            let deadline = self.ordered_at.load(Ordering::Acquire) + LATE_NS;
            if now >= deadline {
                self.fall_back("late", None);
                return Err(FlipError::Late);
            }
            let mut until = now + POLL_NS;
            if !slept_to_vblank && let Some(t) = first {
                slept_to_vblank = true;
                if t > now {
                    until = t;
                }
            }
            self.clock.sleep_ns(until.min(deadline).saturating_sub(now).max(1));
        }
    }

    /// One image again (A0, the copy path), logged; Quiesce or the next `rearm` go on from here.
    fn fall_back(&self, why: &'static str, e: Option<RegError>) {
        if self.state.compare_exchange(READY, FAILED, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        self.late.fetch_add(1, Ordering::AcqRel);
        let back = {
            let _g = self.pair.lock();
            flip::order(&self.regs, &self.pipes, self.gpu[0], Types::Found)
        };
        self.a0_unsure.store(back.is_err(), Ordering::Release);
        self.cur.store(0, Ordering::SeqCst);
        klog::event!(Warn, "dcn", "flip.fallback", why = why, error = alloc::format!("{e:?}").as_str(),
            ordered = self.ordered.load(Ordering::Acquire), landed = self.landed.load(Ordering::Acquire),
            a0_ordered = back.is_ok(); "a flip did not land in 50 ms or a write failed: one image (A0, copying) until the next take");
    }

    /// After a fall-back: flipping again once A0 shows and nothing is pending. A0 is ordered again first if the
    /// fall-back could not order it.
    pub fn rearm(&self) -> bool {
        if self.state.load(Ordering::Acquire) != FAILED {
            return self.active();
        }
        if self.a0_unsure.load(Ordering::Acquire) {
            let r = {
                let _g = self.pair.lock();
                flip::order(&self.regs, &self.pipes, self.gpu[0], Types::Found)
            };
            match r {
                Ok(_) => {
                    self.a0_unsure.store(false, Ordering::Release);
                    klog::event!(Info, "dcn", "flip.a0_ordered"; "A0 ordered again after a fall-back whose A0 order failed");
                }
                Err(e) => {
                    klog::event!(Warn, "dcn", "flip.a0_retry", error = reg_error_text(e).as_str();
                        "A0 order failed again: presents keep going into both buffers");
                    return false;
                }
            }
        }
        if !flip::landed(&self.regs, &self.pipes, self.gpu[0]).unwrap_or(false) {
            return false;
        }
        let (w, h) = self.size();
        self.marks.written[0].store(0, Ordering::Release);
        self.marks.written[1].store(pack((0, 0, w, h)), Ordering::Release);
        self.landed.store(self.ordered.load(Ordering::Acquire), Ordering::Release);
        let ok = self.state.compare_exchange(FAILED, READY, Ordering::AcqRel, Ordering::Acquire).is_ok();
        if ok {
            klog::event!(Info, "dcn", "flip.rearmed", late = self.late.load(Ordering::Acquire); "A0 settled: flipping again");
        }
        ok
    }

    /// The engine's vblank: the frame counter (extended) and the time of the last vblank start from the scan line.
    pub fn vblank(&self) -> Option<Vblank> {
        let ext = self.extend()?;
        let line = flip::line(&self.regs, self.otg).ok()?;
        let now = self.clock.now_ns();
        let since = self.timing.since_vblank_ns(line);
        Some(Vblank {
            last: VblankEvent { sequence: ext, time_ns: now.saturating_sub(since) },
            period_ns: self.timing.period_ns(),
            refresh_mhz: self.timing.refresh_mhz(),
        })
    }

    pub fn next_vblank_ns(&self) -> Option<u64> {
        let line = flip::line(&self.regs, self.otg).ok()?;
        let now = self.clock.now_ns();
        Some(now - self.timing.since_vblank_ns(line).min(now) + self.timing.period_ns())
    }

    /// The frame counter, extended. It is read inside the update, after the stored value was loaded, so a
    /// reading is never older than the one behind the stored value: a caller that read before another stored
    /// retries with a new reading (two callers at once never count a wrap twice).
    fn extend(&self) -> Option<u64> {
        let mut out = None;
        let _ = self.frames.try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            out = None;
            let v = extend_from(old, flip::frame_count(&self.regs, self.otg).ok()?);
            out = Some(v);
            Some(v)
        });
        out
    }

    /// Reload/reboot/panic (Quiesce: interrupts off, no sleep, no log): A0 and the found flip types back, only if
    /// GX ordered a flip; GX-4: the boot mode registers back first, only if a mode switch wrote them (the next
    /// kernel and the firmware find the GOP's own mode, V1 G3-PLAN §4 "Reload"). The pair lock is tried a bounded
    /// number of times; a holder on a stopped CPU does not keep the restore from running (the writes are the
    /// same either way).
    pub fn restore(&self) -> Result<(), RegError> {
        let mode = self.mode_boot();
        let cur = self.cursor_used.load(Ordering::Acquire);
        if !self.touched.load(Ordering::Acquire) && mode.is_none() && !cur {
            return Ok(());
        }
        let mut g = (0..1 << 16).find_map(|_| {
            let g = self.pair.try_lock();
            if g.is_none() {
                core::hint::spin_loop();
            }
            g
        });
        // GX-5a: the cursor off first (the firmware's console and the next kernel do not use it).
        let c = match (cur, self.pipes.first()) {
            (true, Some(p)) => {
                let hw = g.as_ref().and_then(|g| g.hw).map_or_else(|| cursor::hw(&self.regs, p), Ok);
                let r = hw.and_then(|hw| cursor::off(&self.regs, hw));
                if let Some(g) = g.as_mut() {
                    g.regs = r.is_ok().then_some(cursor::Place::OFF);
                    g.shown = false;
                }
                r.map_err(cursor_reg_error)
            }
            _ => Ok(()),
        };
        let m = match (mode, self.pipes.first()) {
            (Some((boot, home)), Some(p)) => crate::mode::apply(&self.regs, p, &crate::mode::plan(&boot, p.hubp, home, home)).map(|_| ()),
            _ => Ok(()),
        };
        if !self.touched.load(Ordering::Acquire) {
            drop(g);
            return c.and(m);
        }
        let r = flip::order(&self.regs, &self.pipes, self.gpu[0], Types::Found);
        drop(g);
        self.cur.store(0, Ordering::SeqCst);
        if r.is_ok() {
            self.a0_unsure.store(false, Ordering::Release);
        }
        c.and(m).and(r.map(|_| ()))
    }

    /// GX-5a: the cursor surfaces (live: a VRAM BO behind a write-combining window). Before the scanout is
    /// shared.
    pub fn set_cursor_buffer(&mut self, buf: cursor::Buf) {
        self.cursor_buf = Some(buf);
    }

    /// The one pipe the cursor is on (a split pipe would need the cursor per viewport: software pointer).
    fn cursor_pipe(&self) -> Result<(&cursor::Buf, &Pipe), CursorError> {
        let buf = self.cursor_buf.as_ref().ok_or(CursorError::NoDevice("no cursor buffer"))?;
        match self.pipes.as_slice() {
            [p] => Ok((buf, p)),
            _ => Err(CursorError::NoDevice("the display spans more than one pipe")),
        }
    }

    /// GX-5a: a hardware cursor is there (a buffer, one pipe); any context, lock-free.
    pub fn cursor_ready(&self) -> bool {
        self.cursor_pipe().is_ok()
    }

    /// GX-5a: a new cursor image (`px`: `shape.w` x `shape.h`, row by row), shown with its hotspot on `tip`
    /// (plane pixels, may be off the screen). Thread context; the pixels are written outside the pair lock.
    pub fn cursor_image(&self, shape: Shape, px: &[u32], tip: (i32, i32)) -> Result<(), CursorError> {
        let (buf, p) = self.cursor_pipe()?;
        let r = buf.with_image(&shape, px, |addr| {
            let mut g = self.pair.lock();
            self.cursor_used.store(true, Ordering::Release);
            // Under the pair lock: a GX-4 `set_size` stores the size before its `cursor_follow` takes the lock,
            // so either this placement sees the new size or the follow runs after it.
            let view = self.size();
            g.image(&self.regs, p, addr, shape, tip, view)
        });
        match r {
            Ok(()) => klog::event!(Info, "dcn", "cursor.image", w = shape.w, h = shape.h, pitch = shape.pitch(),
                hot_x = shape.hot_x, hot_y = shape.hot_y, x = tip.0, y = tip.1; "hardware cursor image set"),
            Err(e) => klog::event!(Warn, "dcn", "cursor.image_failed", error = alloc::format!("{e:?}")),
        }
        r
    }

    /// GX-5a: the pointer moved to `tip` (shows the cursor again after a hide). Any context but Quiesce: the
    /// pair lock, a few register writes, no allocation, no log on success.
    pub fn cursor_move(&self, tip: (i32, i32)) -> Result<(), CursorError> {
        self.cursor_pipe()?;
        let mut g = self.pair.lock();
        // Read under the pair lock (see `cursor_image`): a switch in between cannot leave a stale placement.
        let view = self.size();
        let r = g.moved(&self.regs, tip, view);
        drop(g);
        if let Err(CursorError::Reg(e)) = r {
            klog::event!(Warn, "dcn", "cursor.move_failed", error = alloc::format!("{e:?}"));
        }
        r
    }

    /// GX-5a: hides the cursor until the next image or move.
    pub fn cursor_hide(&self) -> Result<(), CursorError> {
        self.cursor_pipe()?;
        let mut g = self.pair.lock();
        let was = g.shown;
        let r = g.hide(&self.regs);
        drop(g);
        if was || r.is_err() {
            klog::event!(Info, "dcn", "cursor.hidden", ok = r.is_ok());
        }
        r
    }

    /// The cursor's registers as the state knows them (tools, tests).
    pub fn cursor_state(&self) -> (Option<Shape>, (i32, i32), bool, Option<cursor::Place>) {
        let g = self.pair.lock();
        (g.shape, g.tip, g.shown, g.regs)
    }

    /// GX-4 switch, undo, rollback: the cursor follows the new plane size (`set_size`).
    fn cursor_follow(&self) -> Result<(), CursorError> {
        if !self.cursor_used.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut g = self.pair.lock();
        let view = self.size();
        g.follow(&self.regs, view)
    }

    /// At take-over: a cursor an earlier kernel left on (its Quiesce did not run) points at memory this kernel
    /// hands out again: off.
    pub fn cursor_clear_found(&self) -> Result<bool, CursorError> {
        let Some(p) = self.pipes.first() else { return Ok(false) };
        if !cursor::found_on(&self.regs, p) {
            return Ok(false);
        }
        let _g = self.pair.lock();
        cursor::off(&self.regs, cursor::hw(&self.regs, p)?)?;
        Ok(true)
    }
}

/// The 64-bit frame count after `old` for a 24-bit counter reading `c24` taken after `old` was stored: a low
/// part below the stored one is a wrap.
pub fn extend_from(old: u64, c24: u32) -> u64 {
    const M: u64 = 0xFF_FFFF;
    let c = u64::from(c24) & M;
    let v = (old & !M) | c;
    if c < old & M { v + (1 << 24) } else { v }
}

/// A register error as text (tools, logs).
pub fn reg_error_text(e: RegError) -> alloc::string::String {
    match e {
        RegError::Read(b) => alloc::format!("register {b:#07x} unreadable"),
        RegError::Write(b) => alloc::format!("write to {b:#07x} read back different"),
        RegError::LockStuck(k) => alloc::format!("OTG{k} master update lock still set after the unlock"),
    }
}

fn cursor_reg_error(e: CursorError) -> RegError {
    match e {
        CursorError::Reg(r) => r,
        _ => RegError::Write(crate::layout::CUR_CONTROL),
    }
}
