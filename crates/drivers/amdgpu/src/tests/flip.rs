//! GX-3 against a fake engine: the Mini-PC image (G-1 + saved-5.json) whose HUBP takes an address at the low write,
//! sets PENDING and lands at the next vblank unless its OTG is locked or the latch is stuck; the clock is the
//! engine's (a sleep advances the scan). Checks the order (lock, high, low, unlock), the landed check, the 50 ms
//! timeout with its fall-back, the settle from A1, the lock released on every error path, the type restore, the
//! back buffer's repair and the vblank clock.

use super::fake::*;
use crate::attach::{self, Attach, Regs};
use crate::dcn::DcnError;
use crate::flip::{self, Rw, Types};
use crate::layout as l;
use crate::scanout::{self, Clock, LATE_NS, NewError, Scanout, Timing};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::display::DisplayDevice;
use gopfb::fbdev::flip::FlipError;
use gopfb::{FbTarget, GopFb, Layout, RamTarget};
use std::sync::Mutex;

const W: u32 = 1024;
const H: u32 = 768;
/// 1344 x 806 at 65 MHz (a 60 Hz 1024x768 timing): 20.677 us per line, 16.666 ms per frame.
const H_TOTAL: u32 = 1344;
const V_TOTAL: u32 = 806;
const PCLK: u64 = 65_000_000;

pub(super) struct St {
    pub(super) regs: Image,
    writes: Vec<(u32, u32)>,
    requested: [u64; 4],
    pub(super) stuck: bool,
    /// A write to this offset reads back something else.
    pub(super) refuse: Option<u32>,
    /// The refusal happens once.
    pub(super) refuse_once: bool,
    now: u64,
    frame: u64,
    /// GX-4: a viewport/scaler update written under the lock stays pending (never takes effect).
    pub(super) mode_stuck: bool,
    /// GX-4: the OTG stops counting frames.
    pub(super) otg_stopped: bool,
    /// GX-4: HUBP underflows while it shows a viewport of this width.
    pub(super) underflow_at_width: Option<u32>,
}

pub struct Engine(pub(super) Mutex<St>);

pub(super) fn timing() -> Timing {
    Timing::new(H_TOTAL, V_TOTAL, PCLK, H).unwrap()
}

impl Engine {
    pub(super) fn new(img: Image) -> Arc<Engine> {
        let mut regs = img;
        let mut requested = [0; 4];
        for (p, r) in requested.iter_mut().enumerate() {
            let p = p as u32;
            let hi = regs.get(&l::hubp(p, l::HUBP_ADDR_HI)).copied().unwrap_or(0) & 0xFFFF;
            let lo = regs.get(&l::hubp(p, l::HUBP_ADDR)).copied().unwrap_or(0);
            *r = u64::from(hi) << 32 | u64::from(lo);
            regs.entry(l::hubp(p, l::HUBP_INUSE)).or_insert(lo);
            regs.entry(l::hubp(p, l::HUBP_INUSE_HI)).or_insert(hi);
        }
        regs.insert(l::otg(0, l::OTG_LOCK), 0);
        Arc::new(Engine(Mutex::new(St {
            regs,
            writes: Vec::new(),
            requested,
            stuck: false,
            refuse: None,
            refuse_once: false,
            now: 60 * timing().period_ns(),
            frame: 0,
            mode_stuck: false,
            otg_stopped: false,
            underflow_at_width: None,
        })))
    }
    pub(super) fn st(&self) -> std::sync::MutexGuard<'_, St> {
        self.0.lock().unwrap()
    }
    pub(super) fn writes(&self) -> Vec<(u32, u32)> {
        self.st().writes.clone()
    }
    pub(super) fn reg(&self, b: u32) -> u32 {
        self.st().regs.get(&b).copied().unwrap_or(0)
    }
    /// Time passes; each vblank start crossed lands what the HUBPs were asked for (VUPDATE right after).
    pub(super) fn advance(&self, ns: u64) {
        let period = timing().period_ns();
        let mut s = self.st();
        let end = s.now + ns;
        loop {
            let next = (s.now / period + 1) * period;
            if next > end {
                break;
            }
            s.now = next;
            if !s.otg_stopped {
                s.frame += 1;
            }
            let f = (s.frame & 0xFF_FFFF) as u32;
            s.regs.insert(l::otg(0, l::OTG_FRAME_COUNT), f);
            let locked = s.regs.get(&l::otg(0, l::OTG_LOCK)).copied().unwrap_or(0) & l::LOCK != 0;
            // GX-4: a viewport/scaler update takes effect at the first vblank after the unlock.
            if !locked && !s.mode_stuck {
                s.regs.insert(l::dpp(0, l::DSCL_UPDATE), 0);
                s.regs.insert(l::otg(0, l::OTG_PIPE_UPDATE_STATUS), 0);
                let w = s.regs.get(&l::hubp(0, l::HUBP_VIEWPORT)).copied().unwrap_or(0) & 0x3FFF;
                if s.underflow_at_width == Some(w) {
                    let c = s.regs.get(&l::hubp(0, l::HUBP_CNTL)).copied().unwrap_or(0);
                    s.regs.insert(l::hubp(0, l::HUBP_CNTL), c | 0x1000_0000);
                }
            }
            for p in 0..4u32 {
                let flip = s.regs.get(&l::hubp(p, l::HUBP_FLIP)).copied().unwrap_or(0);
                if flip & l::FLIP_PENDING != 0 && !locked && !s.stuck {
                    let a = s.requested[p as usize];
                    s.regs.insert(l::hubp(p, l::HUBP_INUSE), a as u32);
                    s.regs.insert(l::hubp(p, l::HUBP_INUSE_HI), (a >> 32) as u32);
                    s.regs.insert(l::hubp(p, l::HUBP_FLIP), flip & !l::FLIP_PENDING);
                }
            }
            // The CRC of the frame scanned: here a function of the buffer pipe 0 shows.
            if s.regs.get(&l::otg(0, l::OTG_CRC_CNTL)).copied().unwrap_or(0) & l::CRC_EN != 0 {
                let a = s.regs.get(&l::hubp(0, l::HUBP_INUSE)).copied().unwrap_or(0);
                s.regs.insert(l::otg(0, l::OTG_CRC0_DATA_RG), 0x1234_0000 | (a >> 16));
                s.regs.insert(l::otg(0, l::OTG_CRC0_DATA_B), 0x55);
            }
        }
        s.now = end;
    }
}

impl Regs for Engine {
    fn read(&self, b: u32) -> Option<u32> {
        if !crate::snapshot::readable(b) {
            return None;
        }
        let s = self.st();
        if b == l::otg(0, l::OTG_POSITION) {
            let t = timing();
            let into = s.now % t.period_ns();
            let line = (u64::from(t.vblank_start) + into * 1000 / t.line_ps) % u64::from(t.v_total);
            return Some(line as u32);
        }
        Some(s.regs.get(&b).copied().unwrap_or(0))
    }
}

impl Rw for Engine {
    fn write(&self, b: u32, v: u32, check: u32) -> Result<(), DcnError> {
        let mut s = self.st();
        s.writes.push((b, v));
        let cur = s.regs.get(&b).copied().unwrap_or(0);
        let mut stored = v;
        for p in 0..4u32 {
            if b == l::hubp(p, l::HUBP_ADDR) {
                let hi = s.regs.get(&l::hubp(p, l::HUBP_ADDR_HI)).copied().unwrap_or(0) & 0xFFFF;
                s.requested[p as usize] = u64::from(hi) << 32 | u64::from(v);
                let f = s.regs.get(&l::hubp(p, l::HUBP_FLIP)).copied().unwrap_or(0);
                s.regs.insert(l::hubp(p, l::HUBP_FLIP), f | l::FLIP_PENDING);
            }
            if b == l::hubp(p, l::HUBP_FLIP) {
                stored = (v & !l::FLIP_PENDING) | (cur & l::FLIP_PENDING);
            }
        }
        if b == l::otg(0, l::OTG_LOCK) {
            stored = if v & l::LOCK != 0 { l::LOCK | l::LOCK_STATUS } else { 0 };
        }
        // GX-4: viewport and scaler registers are double buffered: pending until the next vblank after unlock.
        let mode_regs = [l::HUBP_VIEWPORT, l::HUBP_SEC_VIEWPORT].map(|r| l::hubp(0, r)).into_iter()
            .chain([l::DSCL_AUTOCAL, l::DSCL_CONTROL, l::SCL_MODE, l::LB_MEMORY_CTRL].map(|r| l::dpp(0, r)))
            .chain(l::SCL_FILTER.map(|(r, _)| l::dpp(0, r)));
        if mode_regs.into_iter().any(|r| r == b) {
            s.regs.insert(l::dpp(0, l::DSCL_UPDATE), l::SCL_UPDATE_PENDING);
            s.regs.insert(l::otg(0, l::OTG_PIPE_UPDATE_STATUS), l::OTG_DC_REG_UPDATE_PENDING);
        }
        // LB_MEMORY_CTRL's partition counts [30:16] are the hardware's.
        if b == l::dpp(0, l::LB_MEMORY_CTRL) {
            stored = (v & 0xFFFF) | 0x1010_0000;
        }
        if s.refuse == Some(b) {
            stored = !v;
            if s.refuse_once {
                s.refuse = None;
            }
        }
        s.regs.insert(b, stored);
        if (stored ^ v) & check != 0 {
            return Err(DcnError::ReadBackMismatch { offset: b, wrote: v, read: stored });
        }
        Ok(())
    }
}

pub(super) struct FakeClock(pub(super) Arc<Engine>);

impl Clock for FakeClock {
    fn now_ns(&self) -> u64 {
        self.0.st().now
    }
    fn sleep_ns(&self, ns: u64) {
        self.0.advance(ns);
    }
}

struct Rig {
    e: Arc<Engine>,
    s: Scanout<Er, FakeClock>,
    t: [Arc<RamTarget>; 2],
}

/// The engine as the scanout's register owner.
pub(super) struct Er(pub(super) Arc<Engine>);

impl Regs for Er {
    fn read(&self, b: u32) -> Option<u32> {
        self.0.read(b)
    }
}
impl Rw for Er {
    fn write(&self, b: u32, v: u32, check: u32) -> Result<(), DcnError> {
        self.0.write(b, v, check)
    }
}

pub(super) fn attach_of(img: &Image) -> Attach {
    attach::gates(&pci(), Some(gop()), &reader(img)).unwrap()
}

fn buf() -> (Arc<RamTarget>, GopFb) {
    let t = Arc::new(RamTarget::new(BYTES as usize, 0xEE));
    let fb = GopFb::new(t.clone(), W, H, W, Layout::Bgrx, "test").unwrap();
    (t, fb)
}

fn rig_with(img: Image) -> Result<Rig, NewError> {
    let a = attach_of(&img);
    let e = Engine::new(img);
    let (t0, a0) = buf();
    let (t1, a1) = buf();
    let s = Scanout::new(Er(e.clone()), FakeClock(e.clone()), &a, a0, a1, timing())?;
    Ok(Rig { e, s, t: [t0, t1] })
}

fn rig() -> Rig {
    rig_with(mini_pc()).unwrap()
}

fn frame(v: u32) -> Vec<u32> {
    vec![v; (W * H) as usize]
}

/// Pixel (x, y) of buffer `i`.
fn px(r: &Rig, i: usize, x: u32, y: u32) -> u32 {
    let mut b = [0u8; 4];
    assert!(r.t[i].read(((y * W + x) * 4) as usize, &mut b));
    u32::from_le_bytes(b)
}

fn whole_equals(r: &Rig, i: usize, src: &[u32]) -> bool {
    let bytes = r.t[i].to_vec();
    src.iter().enumerate().all(|(n, &p)| bytes[n * 4..n * 4 + 4] == p.to_le_bytes())
}

#[test]
fn flip_order_is_lock_high_low_unlock_and_lands() {
    let r = rig();
    assert!(r.s.active());
    assert!(r.e.writes().is_empty(), "taking over an A0 display writes nothing");
    let src = frame(0x0012_3456);
    let back = r.s.begin(&src, W).unwrap();
    assert_eq!(back.name(), "dcn.a1");
    back.present(&src, W, (0, 0, 10, 10)).unwrap();
    let seq = r.s.commit().unwrap();
    assert_eq!(seq, 1);
    let lock = l::otg(0, l::OTG_LOCK);
    assert_eq!(
        r.e.writes(),
        vec![
            (lock, l::LOCK),
            (l::hubp(0, l::HUBP_ADDR_HI), (GPU1 >> 32) as u32),
            (l::hubp(0, l::HUBP_ADDR), GPU1 as u32),
            (lock, 0)
        ],
        "lock, high, low, unlock"
    );
    assert!(flip::pending(&*r.e, &attach_of(&mini_pc()).pipes[0], GPU1).unwrap(), "pending until the vblank");
    assert_eq!(r.s.wait(seq + 1), Err(FlipError::Off), "a fence not ordered yet");
    r.s.wait(seq).unwrap();
    let st = r.s.state();
    assert_eq!((st.shown, st.ordered, st.landed, st.late, st.flipping), (1, 1, 1, 0, true));
    assert!(whole_equals(&r, 1, &src), "A1 got the whole first frame (its content was unknown)");
    assert_eq!(r.e.reg(lock) & l::LOCK, 0);
}

#[test]
fn hidden_buffer_gets_exactly_what_it_lacks() {
    let r = rig();
    let mut src = frame(1);
    let b = r.s.begin(&src, W).unwrap();
    b.present(&src, W, (0, 0, W, H)).unwrap();
    let s1 = r.s.commit().unwrap();
    r.s.wait(s1).unwrap();
    // Frame 2 changes a band; it goes to A0, which lacks frame 1 entirely (it was never written).
    r.t[0].write_pixels(0, &frame(1)); // A0 = the GOP picture equal to frame 1 so far
    for p in &mut src[(100 * W) as usize..(110 * W) as usize] {
        *p = 2;
    }
    let b = r.s.begin(&src, W).unwrap();
    assert_eq!(b.name(), "dcn.a0");
    b.present(&src, W, (0, 100, W, 10)).unwrap();
    let s2 = r.s.commit().unwrap();
    r.s.wait(s2).unwrap();
    assert!(whole_equals(&r, 0, &src));
    // The console/pointer writes the shown buffer (A0) directly; frame 3 into A1 must repair band and patch.
    let mut patch = src.clone();
    for p in &mut patch[(5 * W + 5) as usize..(5 * W + 9) as usize] {
        *p = 9;
    }
    r.s.present_shown(&patch, W, (5, 5, 4, 1)).unwrap();
    assert_eq!(px(&r, 0, 6, 5), 9, "the shown buffer got the patch");
    assert_eq!(px(&r, 1, 6, 5), 1);
    let b = r.s.begin(&src, W).unwrap();
    assert_eq!(b.name(), "dcn.a1");
    assert!(whole_equals(&r, 1, &src), "A1 repaired: frame 2's band, and the patch area from the frame");
    let s3 = r.s.commit().unwrap();
    r.s.wait(s3).unwrap();
    // Next into A0: the patch drawn into A0 while it was shown is put back from the frame.
    let _ = r.s.begin(&src, W).unwrap();
    assert!(whole_equals(&r, 0, &src), "A0's own patch repaired");
    assert_eq!(r.s.written(0), None, "begin forgets what it repaired");
}

#[test]
fn stuck_latch_times_out_and_falls_back_to_a0() {
    let r = rig();
    r.e.st().stuck = true;
    let src = frame(3);
    let _ = r.s.begin(&src, W).unwrap();
    let seq = r.s.commit().unwrap();
    let t0 = r.e.st().now;
    assert_eq!(r.s.wait(seq), Err(FlipError::Late));
    assert!(r.e.st().now - t0 >= LATE_NS - timing().period_ns(), "waited about 50 ms");
    assert!(!r.s.active(), "one image now");
    let w = r.e.writes();
    let lo = l::hubp(0, l::HUBP_ADDR);
    assert_eq!(w.iter().filter(|x| x.0 == lo).map(|x| x.1).collect::<Vec<_>>(), vec![GPU1 as u32, GPU as u32], "A0 written back");
    assert_eq!(r.e.reg(l::otg(0, l::OTG_LOCK)) & l::LOCK, 0, "never left locked");
    assert_eq!(r.s.begin(&src, W).err(), Some(FlipError::Off), "fbdev copies now");
    assert!(!r.s.rearm(), "A0 is not settled while the latch is stuck");
    r.e.st().stuck = false;
    r.e.advance(timing().period_ns());
    assert!(r.s.rearm(), "flips again after A0 landed");
    assert_eq!(r.s.state().late, 1);
    let b = r.s.begin(&src, W).unwrap();
    assert_eq!(b.name(), "dcn.a1");
    let _ = r.s.commit().unwrap();
}

#[test]
fn begin_waits_for_the_last_flip() {
    let r = rig();
    let src = frame(4);
    let _ = r.s.begin(&src, W).unwrap();
    let _ = r.s.commit().unwrap();
    let t0 = r.e.st().now;
    let b = r.s.begin(&src, W).unwrap();
    assert_eq!(b.name(), "dcn.a0");
    let dt = r.e.st().now - t0;
    assert!(dt > 0 && dt < timing().period_ns() + 2_000_000, "slept to the vblank: {dt} ns");
    assert_eq!(r.s.state().landed, 1);
    assert_eq!(r.s.begin(&src, W).err(), Some(FlipError::Busy), "one frame at a time");
}

#[test]
fn lock_is_released_when_the_address_write_fails() {
    let r = rig();
    r.e.st().refuse = Some(l::hubp(0, l::HUBP_ADDR));
    let src = frame(5);
    let _ = r.s.begin(&src, W).unwrap();
    assert_eq!(r.s.commit(), Err(FlipError::Late));
    assert!(!r.s.active());
    let w = r.e.writes();
    let lock = l::otg(0, l::OTG_LOCK);
    let last_lock = w.iter().rposition(|x| x.0 == lock).unwrap();
    assert_eq!(w[last_lock].1, 0, "the last write to the lock unlocks it");
    assert_eq!(r.e.reg(lock) & l::LOCK, 0);
}

#[test]
fn immediate_type_is_cleared_for_flips_and_restored() {
    let mut img = mini_pc();
    let f = l::hubp(0, l::HUBP_FLIP);
    let v = img.get(&f).copied().unwrap_or(0) | l::FLIP_TYPE;
    img.insert(f, v);
    let r = rig_with(img).unwrap();
    let src = frame(6);
    let _ = r.s.begin(&src, W).unwrap();
    let _ = r.s.commit().unwrap();
    let w = r.e.writes();
    let ti = w.iter().position(|x| x.0 == f).unwrap();
    let hi = w.iter().position(|x| x.0 == l::hubp(0, l::HUBP_ADDR_HI)).unwrap();
    assert!(ti < hi && w[ti].1 & l::FLIP_TYPE == 0, "type cleared before the address (X4)");
    r.e.advance(timing().period_ns());
    r.s.restore().unwrap();
    assert_eq!(r.e.reg(f) & l::FLIP_TYPE, l::FLIP_TYPE, "found type back");
    assert_eq!(r.e.writes().last().copied(), Some((l::otg(0, l::OTG_LOCK), 0)));
    let lo: Vec<u32> = r.e.writes().iter().filter(|x| x.0 == l::hubp(0, l::HUBP_ADDR)).map(|x| x.1).collect();
    assert_eq!(lo, vec![GPU1 as u32, GPU as u32], "A0 back");
}

#[test]
fn restore_writes_nothing_without_a_flip() {
    let r = rig();
    r.s.restore().unwrap();
    assert!(r.e.writes().is_empty());
}

#[test]
fn settle_from_a1_and_a_stuck_settle() {
    let mut img = mini_pc();
    img.insert(l::hubp(0, l::HUBP_ADDR), GPU1 as u32);
    assert_eq!(attach_of(&img).settle, attach::Settle::OnA1);
    let r = rig_with(img.clone()).unwrap();
    assert!(r.s.state().settled);
    let lo: Vec<u32> = r.e.writes().iter().filter(|x| x.0 == l::hubp(0, l::HUBP_ADDR)).map(|x| x.1).collect();
    assert_eq!(lo, vec![GPU as u32]);
    assert!(flip::landed(&*r.e, &attach_of(&mini_pc()).pipes, GPU).unwrap());

    let a = attach_of(&img);
    let e = Engine::new(img);
    e.st().stuck = true;
    let (_, a0) = buf();
    let (_, a1) = buf();
    let err = Scanout::new(Er(e.clone()), FakeClock(e.clone()), &a, a0, a1, timing()).err();
    assert_eq!(err, Some(NewError::SettleLate));
    assert_eq!(e.reg(l::otg(0, l::OTG_LOCK)) & l::LOCK, 0);
}

#[test]
fn vblank_clock_from_counter_and_line() {
    let r = rig();
    let t = timing();
    assert_eq!(t.refresh_mhz(), 60_003);
    r.e.advance(3 * t.period_ns() + 1_000_000);
    let v = r.s.vblank().unwrap();
    let now = r.e.st().now;
    assert!(now - v.last.time_ns >= 975_000 && now - v.last.time_ns <= 1_000_000, "{} ns since", now - v.last.time_ns);
    assert_eq!(v.last.sequence, 3);
    assert_eq!(v.period_ns, t.period_ns());
    // The 24-bit counter wraps: the sequence keeps counting.
    r.e.st().frame = 0xFF_FFFE;
    r.e.advance(t.period_ns());
    assert_eq!(r.s.vblank().unwrap().last.sequence, 0xFF_FFFF);
    r.e.advance(t.period_ns());
    assert_eq!(r.s.vblank().unwrap().last.sequence, 1 << 24, "the 24-bit counter wrapped, the sequence did not");
    let next = r.s.next_vblank_ns().unwrap();
    assert!(next > r.e.st().now && next - r.e.st().now <= t.period_ns());
}

#[test]
fn crc_on_read_undo() {
    let e = Engine::new(mini_pc());
    let k = 0;
    e.st().regs.insert(l::otg(k, l::OTG_CRC_CNTL), 0);
    e.st().regs.insert(l::otg(k, l::OTG_CRC0_WINDOWA_X), 0x1234);
    let saved = flip::crc_on(&*e, k, W, H).unwrap().unwrap();
    assert_eq!(e.reg(l::otg(k, l::OTG_CRC0_WINDOWA_X)), W << 16);
    assert_eq!(e.reg(l::otg(k, l::OTG_CRC0_WINDOWB_Y)), H << 16);
    assert_eq!(e.reg(l::otg(k, l::OTG_CRC_CNTL)), l::CRC_EN | l::CRC_CONT_EN);
    assert_eq!(flip::crc_on(&*e, k, W, H).unwrap(), None, "already on: nothing written");
    e.st().regs.insert(l::otg(k, l::OTG_CRC0_DATA_RG), 0xBBBB_AAAA);
    e.st().regs.insert(l::otg(k, l::OTG_CRC0_DATA_B), 0xCCCC);
    assert_eq!(flip::crc_read(&*e, k).unwrap(), Some(0xAAAA_BBBB_CCCC));
    flip::crc_undo(&*e, k, &saved).unwrap();
    assert_eq!(e.reg(l::otg(k, l::OTG_CRC0_WINDOWA_X)), 0x1234);
    assert_eq!(flip::crc_read(&*e, k).unwrap(), None);
}

#[test]
fn order_types_found_keeps_a_vsync_pipe() {
    let e = Engine::new(mini_pc());
    let a = attach_of(&mini_pc());
    flip::order(&*e, &a.pipes, GPU, Types::Found).unwrap();
    assert!(e.writes().iter().all(|w| w.0 != l::hubp(0, l::HUBP_FLIP)), "type already right: not written");
}

#[test]
fn rect_packing() {
    assert_eq!(scanout::pack((0, 0, 0, 5)), 0);
    assert_eq!(scanout::unpack(scanout::pack((3, 4, 5, 6))), Some((3, 4, 5, 6)));
    assert_eq!(scanout::unpack(scanout::pack((0, 0, 70_000, 2))), Some((0, 0, 0xFFFF, 2)));
    assert_eq!(scanout::unpack(0), None);
}

#[test]
fn flip_pace_180_flips_one_per_frame() {
    let r = rig();
    let base = frame(0x0010_1010);
    let mut src = base.clone();
    let (e1, e2) = (r.e.clone(), r.e.clone());
    let clock = move || e1.st().now;
    let sleep = move |ns: u64| e2.advance(ns);
    let p = crate::flip_test::pace(&r.s, &clock, &sleep, &base, &mut src, 180, false, &|| false);
    let period = timing().period_ns();
    assert_eq!((p.flips, p.late, p.error), (180, 0, None), "{p:?}");
    assert_eq!(p.after[2], 0, "none landed two vblanks late: {p:?}");
    assert!(p.ns >= 179 * period && p.ns <= 181 * period, "{} ms", p.ns / 1_000_000);
    assert!(p.text(false, period).starts_with("pace: 180 flips in 3000 ms (one per frame: 2999 ms), late 0, landed after 0/1/2+ vblanks: 0/180/0"), "{}", p.text(false, period));
    // The scratch frame ends as the base with the last bar; the shown buffer equals it.
    let shown = r.s.state().shown as usize;
    assert!(whole_equals(&r, shown, &src));
    assert_eq!(src.iter().filter(|&&v| v == 0x00FF_FFFF).count(), (crate::flip_test::BAR_W * H) as usize);
}

#[test]
fn flip_pace_copy_writes_no_register() {
    let r = rig();
    let base = frame(7);
    let mut src = base.clone();
    let (e1, e2) = (r.e.clone(), r.e.clone());
    let p = crate::flip_test::pace(&r.s, &move || e1.st().now, &move |ns| e2.advance(ns), &base, &mut src, 10, true, &|| false);
    assert_eq!(p.flips, 10);
    assert!(r.e.writes().is_empty(), "the copy path flips nothing");
    assert_eq!(px(&r, 0, crate::flip_test::BAR_STEP * 9 + 1, 0), 0x00FF_FFFF, "the bar went into the shown A0");
}

#[test]
fn crc_follows_the_buffer_shown() {
    let r = rig();
    r.e.st().regs.insert(l::otg(0, l::OTG_CRC_CNTL), 0);
    let p = frame(1);
    let q = frame(0x00FF_FFFE);
    let e = r.e.clone();
    let reads = crate::flip_test::crc(&r.s, &move |ns| e.advance(ns), &p, &q, 4).unwrap();
    assert_eq!(reads.len(), 8);
    let t = crate::flip_test::crc_text(&reads);
    assert!(t.ends_with("stable per buffer true; changes with the flip true"), "{t}");
    assert_eq!(r.e.reg(l::otg(0, l::OTG_CRC_CNTL)), 0, "CRC put back off");
    assert!(whole_equals(&r, 0, &p) && whole_equals(&r, 1, &q));
}

#[test]
fn fall_back_whose_a0_order_failed_keeps_the_picture_and_retries() {
    let r = rig();
    r.e.st().stuck = true;
    let src = frame(3);
    let _ = r.s.begin(&src, W).unwrap();
    let seq = r.s.commit().unwrap();
    // The A0 order of the fall-back reads back different: what shows is not known.
    r.e.st().refuse = Some(l::hubp(0, l::HUBP_ADDR));
    assert_eq!(r.s.wait(seq), Err(FlipError::Late));
    assert!(!r.s.active() && r.s.redirects(), "not flipping, but the live GopFb still goes through present_shown");
    assert_eq!(r.e.reg(l::otg(0, l::OTG_LOCK)) & l::LOCK, 0, "never left locked");
    let patch = vec![0x77; 16];
    r.s.present_shown(&patch, 4, (0, 0, 4, 4)).unwrap();
    assert_eq!((px(&r, 0, 1, 1), px(&r, 1, 1, 1)), (0x77, 0x77), "both buffers got the picture");
    // rearm orders A0 again: still refused, then accepted; it flips once A0 landed.
    assert!(!r.s.rearm());
    assert!(r.s.redirects());
    r.e.st().refuse = None;
    r.e.st().stuck = false;
    assert!(!r.s.rearm(), "A0 ordered, not landed yet");
    assert!(!r.s.redirects(), "A0 ordered: presents go to the GOP framebuffer again");
    let lo = l::hubp(0, l::HUBP_ADDR);
    assert_eq!(r.e.writes().iter().filter(|x| x.0 == lo).map(|x| x.1).last(), Some(GPU as u32), "A0 ordered again");
    r.e.advance(timing().period_ns());
    assert!(r.s.rearm(), "flips again after A0 landed");
    assert!(r.s.active());
}

#[test]
fn frame_count_extension() {
    use crate::scanout::extend_from;
    assert_eq!(extend_from(0, 0x90_0000), 0x90_0000);
    assert_eq!(extend_from(5, 10), 10);
    assert_eq!(extend_from(0xFF_FFFF, 1), (1 << 24) | 1, "forward past the wrap");
    assert_eq!(extend_from((3 << 24) | 0x10, 0x10), (3 << 24) | 0x10, "the same frame");
}

#[test]
fn pace_is_bounded_to_sixty_seconds() {
    let p = timing().period_ns();
    assert!(crate::tools::pace_fits(180, p).is_ok());
    assert!(crate::tools::pace_fits(3500, p).is_ok());
    assert!(crate::tools::pace_fits(1_000_000_000_000, p).is_err(), "a mistyped count is refused");
    assert!(crate::tools::pace_fits(u64::MAX, 0).is_err());
}
