//! GX-4 against the fake engine (tests::flip's, whose viewport and scaler registers latch at the first vblank after
//! the unlock): the native GOP mode in the three scaler states the switch knows (bypass, AUTOSCALE, the real
//! manual 1:1 of 22.10.), the plan's order and values, landing, read-back, a stuck update, a stopped OTG,
//! underflow, the 15 s confirmation and its rollback, a failed way back, the software side refusing or failing,
//! Quiesce, the mode list (EDID, GOP, link) and the request parser.

use super::fake::*;
use super::flip::{Engine, Er, FakeClock, attach_of, timing};
use crate::attach::Pipe;
use crate::layout as l;
use crate::mode::{self, Gate, Offered, PipeRegs, Request, Size, Source, Unoffered};
use crate::scanout::{Clock, Scanout};
use crate::switch::{CONFIRM_NS, Follow, Outcome, Switch, SwitchError, TimerHost};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use gopfb::{GopFb, Layout, RamTarget};
use devmodel::display::DisplayDevice;
use std::sync::Mutex;

const LINK: Size = Size::new(2560, 1440);
const FHD: Size = Size::new(1920, 1080);
const RATE: u32 = 59_950;

/// The Mini-PC image under V2's native GOP mode: the viewport is the link's 2560x1440 (pitch 2560); `scaling`
/// keeps the GOP's AUTOSCALE (ratio 1), else the scaler is in bypass.
fn native(scaling: bool) -> Image {
    let mut m = mini_pc();
    for r in [l::HUBP_VIEWPORT, l::HUBP_SEC_VIEWPORT] {
        m.insert(l::hubp(0, r), LINK.dim());
    }
    m.insert(l::hubp(0, l::HUBP_PITCH), 2559);
    m.insert(l::dpp(0, l::RECOUT_SIZE), LINK.dim());
    m.insert(l::dpp(0, l::MPC_SIZE), LINK.dim());
    if !scaling {
        m.insert(l::dpp(0, l::DSCL_AUTOCAL), 0);
        m.insert(l::dpp(0, l::DSCL_CONTROL), 0);
        m.insert(l::dpp(0, l::SCL_MODE), 0);
        m.insert(l::dpp(0, l::LB_MEMORY_CTRL), 0x1010_3F02);
    }
    m
}

type S = Scanout<Er, FakeClock>;

struct Rig {
    e: Arc<Engine>,
    s: S,
    f: Fol,
    sw: Switch,
}

#[derive(Default)]
struct FolSt {
    sizes: Vec<Size>,
    refuse: Option<String>,
    fail: bool,
}

/// The software side: records what it was resized to.
#[derive(Default)]
struct Fol(Mutex<FolSt>);

impl Follow for Fol {
    fn check(&self, _to: Size) -> Result<(), String> {
        self.0.lock().unwrap().refuse.clone().map_or(Ok(()), Err)
    }
    fn resize(&self, to: Size) -> Result<(), String> {
        let mut g = self.0.lock().unwrap();
        if g.fail {
            return Err(String::from("busy"));
        }
        g.sizes.push(to);
        Ok(())
    }
}

impl Fol {
    fn sizes(&self) -> Vec<Size> {
        self.0.lock().unwrap().sizes.clone()
    }
}

fn buf() -> GopFb {
    let t = Arc::new(RamTarget::new((LINK.w * LINK.h * 4) as usize, 0));
    GopFb::new(t, LINK.w, LINK.h, LINK.w, Layout::Bgrx, "test").unwrap()
}

fn rig(img: Image) -> Rig {
    let a = attach_of(&img);
    let e = Engine::new(img);
    let s = Scanout::new(Er(e.clone()), FakeClock(e.clone()), &a, buf(), buf(), timing()).unwrap();
    Rig { e, s, f: Fol::default(), sw: Switch::new(LINK, RATE, LINK) }
}

fn pipe() -> Pipe {
    attach_of(&mini_pc()).pipes[0]
}

fn mode_regs(e: &Engine) -> (u32, u32, u32, u32, u32, u32) {
    (
        e.reg(l::hubp(0, l::HUBP_VIEWPORT)),
        e.reg(l::hubp(0, l::HUBP_SEC_VIEWPORT)),
        e.reg(l::dpp(0, l::DSCL_AUTOCAL)),
        e.reg(l::dpp(0, l::SCL_MODE)) & l::DSCL_MODE,
        e.reg(l::dpp(0, l::DSCL_CONTROL)),
        e.reg(l::dpp(0, l::LB_MEMORY_CTRL)) & 0xFFFF,
    )
}

#[test]
fn bypass_to_scaled_writes_v1_constants_under_the_lock_and_lands() {
    let mut r = rig(native(false));
    assert!(r.e.writes().is_empty());
    let o = r.sw.set(&r.s, &r.f, FHD).unwrap();
    let Outcome::Pending { serial: 1, deadline_ns } = o else { panic!("{o:?}") };
    assert!(deadline_ns >= r.s.clock().now_ns() + CONFIRM_NS - 200_000_000);
    let lock = l::otg(0, l::OTG_LOCK);
    let w = r.e.writes();
    assert_eq!(
        w,
        vec![
            (lock, l::LOCK),
            (l::dpp(0, l::SCL_MODE), 1),
            (l::dpp(0, l::LB_MEMORY_CTRL), mode::LB_MEMORY_SCALED),
            (l::dpp(0, l::DSCL_CONTROL), 1),
            (l::dpp(0, l::DSCL_AUTOCAL), 1),
            (l::hubp(0, l::HUBP_VIEWPORT), 0x0438_0780),
            (l::hubp(0, l::HUBP_SEC_VIEWPORT), 0x0438_0780),
            (lock, 0),
        ],
        "V1 §3.6 order, then the viewports, all under the lock"
    );
    assert!(!w.iter().any(|&(b, _)| b == l::hubp(0, l::HUBP_PITCH) || b == l::hubp(0, l::HUBP_ADDR)), "pitch and address stay");
    assert_eq!(r.f.sizes(), vec![FHD]);
    assert_eq!(r.s.size(), (1920, 1080));
    assert_eq!(r.s.written(0), Some((0, 0, 1920, 1080)), "both buffers whole at the new size");
    let st = r.sw.status(r.s.clock().now_ns());
    assert_eq!((st.shown, st.confirmed), (FHD, LINK));
    assert!(st.left_ns.is_some());
    // Kept: the timer finds nothing to do.
    assert_eq!(r.sw.confirm(), Ok(FHD));
    r.e.advance(CONFIRM_NS + 1);
    assert!(r.sw.expire(&r.s, &r.f, 1).is_none());
    assert_eq!(r.sw.confirm(), Err(SwitchError::NothingPending));
    // The same size again: nothing written.
    let n = r.e.writes().len();
    assert_eq!(r.sw.set(&r.s, &r.f, FHD), Ok(Outcome::Unchanged));
    assert_eq!(r.e.writes().len(), n);
}

#[test]
fn back_to_native_writes_the_found_values_in_v1_w_order() {
    let mut r = rig(native(false));
    r.sw.set(&r.s, &r.f, FHD).unwrap();
    r.sw.confirm().unwrap();
    let n = r.e.writes().len();
    let o = r.sw.set(&r.s, &r.f, LINK).unwrap();
    assert!(matches!(o, Outcome::Pending { serial: 2, .. }), "the native mode after a confirmed other one is new again: {o:?}");
    let lock = l::otg(0, l::OTG_LOCK);
    assert_eq!(
        r.e.writes()[n..],
        [
            (lock, l::LOCK),
            (l::dpp(0, l::DSCL_AUTOCAL), 0),
            (l::dpp(0, l::DSCL_CONTROL), 0),
            (l::dpp(0, l::SCL_MODE), 0),
            (l::dpp(0, l::LB_MEMORY_CTRL), 0x1010_3F02),
            (l::hubp(0, l::HUBP_VIEWPORT), LINK.dim()),
            (l::hubp(0, l::HUBP_SEC_VIEWPORT), LINK.dim()),
            (lock, 0),
        ]
    );
    assert_eq!(mode_regs(&r.e), (LINK.dim(), LINK.dim(), 0, 0, 0, 0x3F02));
}

#[test]
fn scaling_boot_state_writes_only_the_viewports() {
    let mut r = rig(native(true));
    r.sw.set(&r.s, &r.f, Size::new(1280, 720)).unwrap();
    let lock = l::otg(0, l::OTG_LOCK);
    assert_eq!(
        r.e.writes(),
        vec![(lock, l::LOCK), (l::hubp(0, l::HUBP_VIEWPORT), 0x02D0_0500), (l::hubp(0, l::HUBP_SEC_VIEWPORT), 0x02D0_0500), (lock, 0)]
    );
}

#[test]
fn no_confirmation_rolls_back_after_15_s() {
    let mut r = rig(native(false));
    let boot = mode_regs(&r.e);
    let Ok(Outcome::Pending { serial, .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    r.e.advance(CONFIRM_NS / 2);
    assert!(r.sw.expire(&r.s, &r.f, serial).is_none(), "not due yet");
    r.e.advance(CONFIRM_NS);
    assert_eq!(r.sw.expire(&r.s, &r.f, serial), Some(Ok(LINK)));
    assert_eq!(mode_regs(&r.e), boot, "the boot state is back");
    assert_eq!(r.f.sizes(), vec![FHD, LINK]);
    assert_eq!(r.s.size(), (2560, 1440));
    assert!(r.sw.status(r.s.clock().now_ns()).left_ns.is_none());
    assert!(r.sw.expire(&r.s, &r.f, serial).is_none(), "once");
}

#[test]
fn second_set_while_waiting_keeps_the_confirmed_way_back() {
    let mut r = rig(native(false));
    let Ok(Outcome::Pending { serial: s1, .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    let Ok(Outcome::Pending { serial: s2, .. }) = r.sw.set(&r.s, &r.f, Size::new(1600, 900)) else { panic!() };
    assert_ne!(s1, s2);
    r.e.advance(2 * CONFIRM_NS);
    assert!(r.sw.expire(&r.s, &r.f, s1).is_none(), "the first timer is void");
    assert_eq!(r.sw.expire(&r.s, &r.f, s2), Some(Ok(LINK)));
    // Going back to the confirmed size needs no confirmation; undo without a waiting switch is refused.
    let mut r = rig(native(false));
    r.sw.set(&r.s, &r.f, FHD).unwrap();
    assert_eq!(r.sw.set(&r.s, &r.f, LINK), Ok(Outcome::Back));
    assert_eq!(r.sw.undo(&r.s, &r.f), Err(SwitchError::NothingPending));
}

#[test]
fn undo_goes_back_at_once() {
    let mut r = rig(native(false));
    r.sw.set(&r.s, &r.f, FHD).unwrap();
    assert_eq!(r.sw.undo(&r.s, &r.f), Ok(LINK));
    assert_eq!(r.s.size(), (2560, 1440));
}

#[test]
fn read_back_mismatch_rolls_back() {
    let mut r = rig(native(false));
    let boot = mode_regs(&r.e);
    {
        let mut st = r.e.st();
        st.refuse = Some(l::hubp(0, l::HUBP_SEC_VIEWPORT));
        st.refuse_once = true;
    }
    let e = r.sw.set(&r.s, &r.f, FHD).unwrap_err();
    assert!(matches!(&e, SwitchError::RolledBack(w) if w.contains("read back")), "{e:?}");
    assert_eq!(mode_regs(&r.e), boot);
    assert!(r.f.sizes().is_empty(), "the software side never moved");
    assert_eq!(r.s.size(), (2560, 1440));
    assert_eq!(r.e.reg(l::otg(0, l::OTG_LOCK)) & l::LOCK, 0, "unlocked on the error path");
    // The next try works.
    assert!(matches!(r.sw.set(&r.s, &r.f, FHD), Ok(Outcome::Pending { .. })));
}

#[test]
fn stuck_update_rolls_back_after_50_ms() {
    let mut r = rig(native(false));
    r.e.st().mode_stuck = true;
    let t0 = r.s.clock().now_ns();
    let e = r.sw.set(&r.s, &r.f, FHD).unwrap_err();
    // The way back cannot land either while the update is stuck: the boot state is tried, then blocked.
    assert!(matches!(&e, SwitchError::Blocked(w) if w.contains("did not take effect")), "{e:?}");
    assert!(r.s.clock().now_ns() - t0 >= crate::scanout::LATE_NS);
    assert!(matches!(r.sw.set(&r.s, &r.f, FHD), Err(SwitchError::Blocked(_))), "blocked until restart");
    assert!(r.sw.status(0).blocked.is_some());
}

#[test]
fn stopped_otg_and_underflow_roll_back() {
    let mut r = rig(native(false));
    r.e.st().otg_stopped = true;
    let e = r.sw.set(&r.s, &r.f, FHD).unwrap_err();
    assert!(matches!(&e, SwitchError::Blocked(w) if w.contains("stopped counting")), "{e:?}");

    // G-1 shows the sticky status set already (0x300F1002: the GOP's own): then the switch cannot judge it.
    let mut r = rig(native(false));
    r.e.st().underflow_at_width = Some(1920);
    assert!(matches!(r.sw.set(&r.s, &r.f, FHD), Ok(Outcome::Pending { .. })));
    let mut img = native(false);
    img.insert(l::hubp(0, l::HUBP_CNTL), 0x000F_1002);
    let mut r = rig(img);
    let boot = mode_regs(&r.e);
    r.e.st().underflow_at_width = Some(1920);
    let e = r.sw.set(&r.s, &r.f, FHD).unwrap_err();
    assert!(matches!(&e, SwitchError::RolledBack(w) if w.contains("underflow")), "{e:?}");
    assert_eq!(mode_regs(&r.e), boot);
    assert!(r.f.sizes().is_empty());
}

#[test]
fn software_side_refusing_writes_nothing_and_failing_rolls_back() {
    let mut r = rig(native(false));
    r.f.0.lock().unwrap().refuse = Some(String::from("a program holds the screen"));
    assert_eq!(r.sw.set(&r.s, &r.f, FHD), Err(SwitchError::Follow(String::from("a program holds the screen"))));
    assert!(r.e.writes().is_empty(), "nothing written");

    let mut r = rig(native(false));
    let boot = mode_regs(&r.e);
    r.f.0.lock().unwrap().fail = true;
    let e = r.sw.set(&r.s, &r.f, FHD).unwrap_err();
    assert!(matches!(&e, SwitchError::RolledBack(w) if w.contains("could not follow")), "{e:?}");
    assert_eq!(mode_regs(&r.e), boot);
    assert_eq!(r.s.size(), (2560, 1440));
}

#[test]
fn forced_rollback_follows_later_when_the_software_side_is_busy() {
    let mut r = rig(native(false));
    let Ok(Outcome::Pending { serial, .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    r.f.0.lock().unwrap().fail = true;
    r.e.advance(CONFIRM_NS + 1);
    assert_eq!(r.sw.expire(&r.s, &r.f, serial), Some(Ok(LINK)), "the hardware goes back regardless");
    assert_eq!(mode_regs(&r.e).0, LINK.dim());
    assert_eq!(r.sw.status(0).follow_due, Some(LINK));
    assert!(!r.sw.tick(&r.f));
    r.f.0.lock().unwrap().fail = false;
    assert!(r.sw.tick(&r.f));
    assert_eq!(r.f.sizes(), vec![FHD, LINK]);
    assert_eq!(r.sw.status(0).follow_due, None);
}

#[test]
fn quiesce_puts_the_boot_mode_back() {
    let mut r = rig(native(false));
    let boot = mode_regs(&r.e);
    r.sw.set(&r.s, &r.f, FHD).unwrap();
    assert_ne!(mode_regs(&r.e), boot);
    let n = r.e.writes().len();
    r.s.restore().unwrap();
    assert_eq!(mode_regs(&r.e), boot, "a Reload mid-confirmation finds the GOP's own mode");
    let w = &r.e.writes()[n..];
    assert_eq!(w.first(), Some(&(l::otg(0, l::OTG_LOCK), l::LOCK)));
    assert_eq!(w.last(), Some(&(l::otg(0, l::OTG_LOCK), 0)));
    assert!(!w.iter().any(|&(b, _)| b == l::hubp(0, l::HUBP_ADDR)), "no flip was ordered: no address write");
}

#[test]
fn gates_refuse_unknown_states() {
    let p = [pipe()];
    let rd = |img: &Image, first| mode::gates(&*Engine::new(img.clone()), &p, LINK, LINK, first);
    assert!(rd(&native(false), true).unwrap().bypass());
    assert!(rd(&native(true), true).unwrap().scaling());
    let mut m = native(false);
    m.insert(l::dpp(0, l::DSCL_AUTOCAL), 2);
    assert!(matches!(rd(&m, true), Err(Gate::Scaler { .. })));
    assert!(rd(&m, false).is_ok(), "after the first switch the scaler is the switch's own");
    let mut m = native(false);
    m.insert(l::dpp(0, l::DSCL_MEM_PWR_STATUS), 2);
    assert!(matches!(rd(&m, true), Err(Gate::ScalerMemory(2))));
    let mut m = native(false);
    m.insert(l::dpp(0, l::RECOUT_SIZE), FHD.dim());
    assert!(matches!(rd(&m, true), Err(Gate::Output { .. })));
    let mut m = native(false);
    m.insert(l::hubp(0, l::HUBP_VIEWPORT_START), 4);
    assert!(matches!(rd(&m, true), Err(Gate::ViewportStart { pri: 4, .. })));
    let mut m = native(false);
    m.insert(l::otg(0, l::OTG_LOCK), 1);
    assert!(matches!(mode::gates(&reader(&m), &p, LINK, LINK, true), Err(Gate::Locked(1))));
    let two = [pipe(), Pipe { hubp: 1, ..pipe() }];
    assert_eq!(mode::gates(&*Engine::new(native(false)), &two, LINK, LINK, true), Err(Gate::Pipes(2)));
    // G-1's own image (1024x768 scaled by the GOP) is a known state too, with its framebuffer as home.
    let g1 = mode::gates(&*Engine::new(mini_pc()), &p, LINK, Size::new(1024, 768), true).unwrap();
    assert!(g1.scaling() && Size::of_dim(g1.pri_vp) == Size::new(1024, 768));
    // A viewport left by a kernel that switched and did not put it back: its way back is not known.
    let mut m = native(false);
    m.insert(l::hubp(0, l::HUBP_VIEWPORT), FHD.dim());
    assert!(matches!(rd(&m, true), Err(Gate::Viewport { pri: FHD, .. })));
}

/// GX-4b: the GOP's real native state on the Mini-PC (manual 1:1) is known: accepted by the first switch's gates;
/// other AUTOCAL / DSCL_MODE / ratio combinations stay refused.
#[test]
fn real_native_state_is_manual_one_to_one() {
    let p = [pipe()];
    let rd = |img: &Image| mode::gates(&*Engine::new(img.clone()), &p, LINK, LINK, true);
    let b = rd(&mini_pc_native()).unwrap();
    assert!(b.manual() && !b.bypass() && !b.scaling(), "{b:?}");
    assert_eq!((b.autocal, b.scl_mode, b.dscl_control, b.lb_mem), (0, 1, 0, 0x0606_3F00));
    assert_eq!(b.filter, REAL_FILTER);
    assert_eq!((Size::of_dim(b.pri_vp), Size::of_dim(b.recout), Size::of_dim(b.mpc)), (LINK, LINK, LINK));
    for (reg, v, why) in [
        (l::SCL_FILTER[0].0, 0x00C0_0000, "H ratio 0.75"),
        (l::SCL_FILTER[1].0, 0, "V ratio 0"),
        (l::DSCL_AUTOCAL, 2, "AUTOCENTER"),
        (l::DSCL_AUTOCAL, 3, "AUTOREPLICATE"),
        (l::SCL_MODE, 2, "444 YCbCr"),
        (l::SCL_MODE, 6, "DSCL bypass"),
    ] {
        let mut m = mini_pc_native();
        m.insert(l::dpp(0, reg), v);
        let e = rd(&m).unwrap_err();
        assert!(matches!(e, Gate::Scaler { .. }), "{why}: {e:?}");
        assert!(alloc::format!("{e}").contains("scaler programmed in an unknown way"), "{e}");
    }
    let mut m = mini_pc_native();
    m.insert(l::dpp(0, l::DSCL_MEM_PWR_STATUS), 0x1502);
    assert_eq!(rd(&m), Err(Gate::ScalerMemory(0x1502)), "the scaler's LUT memory must be on in manual too");
}

/// gx.mode.roundtrip's check after the way back: only the bits the plan checks count (fixer GX-4b, finding 1).
#[test]
fn way_back_compares_only_the_checked_bits() {
    let p = [pipe()];
    let b = mode::gates(&*Engine::new(mini_pc_native()), &p, LINK, LINK, true).unwrap();
    let mut a = b;
    a.lb_mem = (a.lb_mem & 0xFFFF) | 0x1010_0000;
    a.autocal |= !l::AUTOCAL_FIELDS;
    a.dscl_control |= !l::SCL_BOUNDARY_MODE;
    a.scl_mode |= !l::DSCL_MODE;
    for (v, &(_, bits)) in a.filter.iter_mut().zip(&l::SCL_FILTER) {
        *v |= !bits;
    }
    assert!(a.same_checked(&b) && b.same_checked(&a), "unchecked bits recomputed by the hardware: {a:?}");
    for i in 0..mode::SAVED {
        let mut v = b.to_saved();
        v[i] ^= mode::SAVED_CHECKED[i] & mode::SAVED_CHECKED[i].wrapping_neg();
        let c = PipeRegs { pri_start: b.pri_start, sec_start: b.sec_start, recout: b.recout, mpc: b.mpc, ..PipeRegs::from_saved(v) };
        assert!(!c.same_checked(&b), "a checked bit of saved entry {i} differs");
    }
}

/// The real snapshot's ratios, phases and taps in `l::SCL_FILTER` order (H, V, H_C, V_C ratio; H, H_C, V, V_BOT,
/// V_C, V_BOT_C init; taps).
const REAL_FILTER: [u32; 11] =
    [0x0100_0000, 0x0100_0000, 0, 0, 0, 0x0100_0000, 0, 0x0100_0000, 0x0100_0000, 0x0100_0000, 0];

fn filter_regs(e: &Engine) -> [u32; 11] {
    l::SCL_FILTER.map(|(r, _)| e.reg(l::dpp(0, r)))
}

/// From manual 1:1: the AUTOSCALE sequence (LB keeps its partitions), then back with every found register in
/// Linux's manual order, even when AUTOSCALE left other ratios behind.
#[test]
fn manual_to_scaled_and_back_restores_every_found_register() {
    let mut r = rig(mini_pc_native());
    let lock = l::otg(0, l::OTG_LOCK);
    let Ok(Outcome::Pending { .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    assert_eq!(
        r.e.writes(),
        vec![
            (lock, l::LOCK),
            (l::dpp(0, l::SCL_MODE), 1),
            (l::dpp(0, l::LB_MEMORY_CTRL), 0x0606_3F00),
            (l::dpp(0, l::DSCL_CONTROL), 1),
            (l::dpp(0, l::DSCL_AUTOCAL), 1),
            (l::hubp(0, l::HUBP_VIEWPORT), FHD.dim()),
            (l::hubp(0, l::HUBP_SEC_VIEWPORT), FHD.dim()),
            (lock, 0),
        ]
    );
    assert_eq!(r.s.size(), (1920, 1080));
    // What AUTOSCALE might leave in the manual registers (0.75 and phases of its own).
    for (i, v) in [(0, 0x00C0_0000), (1, 0x00C0_0000), (4, 0x0120_0000), (10, 0x11)] {
        r.e.st().regs.insert(l::dpp(0, l::SCL_FILTER[i].0), v);
    }
    let n = r.e.writes().len();
    assert_eq!(r.sw.undo(&r.s, &r.f), Ok(LINK));
    let mut want = vec![
        (lock, l::LOCK),
        (l::dpp(0, l::DSCL_AUTOCAL), 0),
        (l::dpp(0, l::DSCL_CONTROL), 0),
        (l::dpp(0, l::SCL_MODE), 1),
        (l::dpp(0, l::LB_MEMORY_CTRL), 0x0606_3F00),
    ];
    want.extend(l::SCL_FILTER.iter().zip(REAL_FILTER).map(|(&(reg, _), v)| (l::dpp(0, reg), v)));
    want.extend([(l::hubp(0, l::HUBP_VIEWPORT), LINK.dim()), (l::hubp(0, l::HUBP_SEC_VIEWPORT), LINK.dim()), (lock, 0)]);
    assert_eq!(r.e.writes()[n..], want[..]);
    assert_eq!(mode_regs(&r.e), (LINK.dim(), LINK.dim(), 0, 1, 0, 0x3F00));
    assert_eq!(filter_regs(&r.e), REAL_FILTER);
    assert_eq!(r.s.size(), (2560, 1440));
}

/// Quiesce (Reload inside the 15 s) and the timer's rollback put the manual state back whole.
#[test]
fn manual_state_comes_back_on_quiesce_and_timeout() {
    let mut r = rig(mini_pc_native());
    let boot = (mode_regs(&r.e), filter_regs(&r.e));
    r.sw.set(&r.s, &r.f, Size::new(1280, 720)).unwrap();
    r.e.st().regs.insert(l::dpp(0, l::SCL_FILTER[0].0), 0x00AA_AAAA);
    r.s.restore().unwrap();
    assert_eq!((mode_regs(&r.e), filter_regs(&r.e)), boot, "a Reload mid-confirmation finds the GOP's own state");
    let mut r = rig(mini_pc_native());
    let Ok(Outcome::Pending { serial, .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    r.e.advance(CONFIRM_NS + 1);
    assert_eq!(r.sw.expire(&r.s, &r.f, serial), Some(Ok(LINK)));
    assert_eq!((mode_regs(&r.e), filter_regs(&r.e)), boot);
}

#[test]
fn trial_size_is_listed_and_keeps_the_link_aspect() {
    let o = |w, h| Offered { size: Size::new(w, h), rates_mhz: vec![] };
    let list = [o(2560, 1440), o(1920, 1440), o(1920, 1200), o(1920, 1080), o(1024, 768)];
    assert_eq!(mode::trial(&list, LINK, LINK), Some(FHD), "1920x1440 is listed first but is 4:3");
    assert_eq!(mode::trial(&[o(2560, 1440), o(1920, 1440), o(1024, 768)], LINK, LINK), Some(Size::new(1920, 1440)));
    assert_eq!(mode::trial(&[o(2560, 1440), o(3840, 2160), o(200, 100)], LINK, LINK), None, "none fits / too small");
}

#[test]
fn edid_standard_and_established_timings_are_offered() {
    use display::edid::sample;
    let qhd = sample::dtd(24150, 2560, 160, 1440, 41, 48, 32, 3, 5, 0x1A);
    let mut edid = sample::edid(&[qhd], 0);
    // Established: 640x480@60 (t1 bit 5), 1024x768@60 (t2 bit 3), 1024x768@43 interlaced (t2 bit 4, skipped).
    edid[0x23] = 1 << 5;
    edid[0x24] = 1 << 3 | 1 << 4;
    edid[0x25] = 0;
    // Standard: 1920x1080@60 (16:9), 1280x1024@60 (5:4), 1600x1200@60 (4:3), 1360x765@60 (HDTV hack), unused.
    let std: [[u8; 2]; 8] = [[0xD1, 0xC0], [0x81, 0x80], [0xA9, 0x40], [0x8B, 0xC0], [1, 1], [1, 1], [0x20, 0x20], [0, 0]];
    for (i, t) in std.iter().enumerate() {
        edid[0x26 + 2 * i..0x28 + 2 * i].copy_from_slice(t);
    }
    sample::fix_checksum(&mut edid[..128]);
    let (v, src) = mode::offered(Some(&edid), &[Size::new(800, 600)], LINK);
    assert_eq!(src, Source::Edid);
    let sizes: Vec<Size> = v.iter().map(|o| o.size).collect();
    let want: Vec<Size> =
        [(2560, 1440), (1920, 1080), (1600, 1200), (1280, 1024), (1366, 768), (1024, 768), (640, 480)].iter().map(|&(w, h)| Size::new(w, h)).collect();
    assert_eq!(sizes, want, "detailed, standard, established; the GOP's list is not used");
    assert_eq!(v[1].rates_mhz, vec![60_000]);
    assert_eq!(mode::trial(&v, LINK, LINK), Some(FHD));
    // A 1.2 EDID reads aspect 0 as 1:1 (drm_mode_std).
    edid[0x13] = 2;
    edid[0x26] = 0x81;
    edid[0x27] = 0x00;
    sample::fix_checksum(&mut edid[..128]);
    let (v, _) = mode::offered(Some(&edid), &[], LINK);
    assert!(v.iter().any(|o| o.size == Size::new(1280, 1280)));
}

#[test]
fn plan_is_bounded_and_allocates_nothing() {
    let boot = PipeRegs { autocal: 0, scl_mode: 0x1000, lb_mem: 0x3F00, pri_vp: LINK.dim(), sec_vp: LINK.dim(), ..PipeRegs::default() };
    let p = mode::plan(&boot, 0, LINK, FHD);
    assert_eq!(p.writes().len(), 6);
    assert_eq!(p.writes()[0].value, 0x1001, "DSCL_MODE set, the other SCL_MODE bits kept");
    let back = mode::plan(&boot, 0, LINK, LINK);
    assert_eq!(back.writes().iter().map(|w| w.value).collect::<Vec<_>>(), vec![0, 0, 0x1000, 0x3F00, LINK.dim(), LINK.dim()]);
    assert_eq!(PipeRegs::from_saved(boot.to_saved()).to_saved(), boot.to_saved());
    // The longest plan (back to native from manual 1:1) fills the fixed array exactly: nothing is dropped.
    let manual = PipeRegs { scl_mode: 1, filter: REAL_FILTER, ..boot };
    assert!(manual.manual());
    assert_eq!(mode::plan(&manual, 0, LINK, LINK).writes().len(), mode::PLAN_MAX);
    assert_eq!(PipeRegs::from_saved(manual.to_saved()), PipeRegs { recout: 0, mpc: 0, pri_start: 0, sec_start: 0, ..manual });
}

#[test]
fn requests_parse_without_panics() {
    assert_eq!(mode::parse("1920x1080@60"), Ok(Request { size: FHD, refresh_mhz: Some(60_000) }));
    assert_eq!(mode::parse(" 2560X1440@59.95 "), Ok(Request { size: LINK, refresh_mhz: Some(59_950) }));
    assert_eq!(mode::parse("1280x720"), Ok(Request { size: Size::new(1280, 720), refresh_mhz: None }));
    assert_eq!(mode::parse("1920x1080@59.940Hz").unwrap().refresh_mhz, Some(59_940));
    for bad in ["", "x", "1920", "1920x", "x1080", "-1x5", "1920x1080@", "1920x1080@0", "1920x1080@1001", "99999x1080", "100x100",
        "1920x1080@60.0001", "1920x1080@6e1", "1920 x 1080 x 3", "１９２０x1080", "4294967296x1080", "1920x1080@60@60"]
    {
        assert!(mode::parse(bad).is_err(), "{bad:?}");
    }
    assert_eq!(mode::rate_text(59_950), "59.950");
}

#[test]
fn modes_from_edid_gop_or_link() {
    use display::edid::sample;
    // 2560x1440@59.95 (241.5 MHz, CVT-RB) and 1920x1080@60 (148.5 MHz, CEA) and an interlaced one.
    let qhd = sample::dtd(24150, 2560, 160, 1440, 41, 48, 32, 3, 5, 0x1A);
    let fhd = sample::dtd(14850, 1920, 280, 1080, 45, 88, 44, 4, 5, 0x1E);
    let edid = sample::edid(&[qhd, fhd], 0);
    let (v, src) = mode::offered(Some(&edid), &[Size::new(1024, 768)], LINK);
    assert_eq!(src, Source::Edid);
    assert_eq!(v.iter().map(|o| o.size).collect::<Vec<_>>(), vec![LINK, FHD], "EDID wins over the GOP list; largest first");
    assert_eq!(v[1].rates_mhz, vec![60_000]);
    let (g, src) = mode::offered(None, &[Size::new(1024, 768), FHD, FHD, Size::new(0, 0)], LINK);
    assert_eq!(src, Source::Gop);
    assert_eq!(g, vec![
        Offered { size: LINK, rates_mhz: vec![] },
        Offered { size: FHD, rates_mhz: vec![] },
        Offered { size: Size::new(1024, 768), rates_mhz: vec![] }
    ]);
    let (n, src) = mode::offered(Some(&[1, 2, 3]), &[], LINK);
    assert_eq!((n.len(), src), (1, Source::Link), "a broken EDID and no GOP list: the link alone");
    // choose
    let req = |s: &str| mode::parse(s).unwrap();
    assert_eq!(mode::choose(&req("1920x1080@60"), &v, LINK, RATE), Ok(FHD), "60 asks for the link's 59.950");
    assert_eq!(mode::choose(&req("1920x1080@50"), &v, LINK, RATE), Err(Unoffered::Rate { asked_mhz: 50_000, link_mhz: RATE }));
    assert_eq!(mode::choose(&req("3840x2160"), &v, LINK, RATE), Err(Unoffered::Larger { max: LINK }));
    assert_eq!(mode::choose(&req("1600x900"), &v, LINK, RATE), Err(Unoffered::NotOffered));
    assert_eq!(mode::choose(&req("2560x1440@60"), &n, LINK, RATE), Ok(LINK), "the link's own size is always on offer");
}

#[test]
fn tool_arguments() {
    use crate::tools::{ModeArgs, mode_args};
    let j = |s: &str| mcp::Json::parse(s).unwrap();
    assert_eq!(mode_args(&j("{}")), Ok(ModeArgs::State));
    assert_eq!(mode_args(&j(r#"{"set":"1920x1080@60"}"#)), Ok(ModeArgs::Set(String::from("1920x1080@60"))));
    assert_eq!(mode_args(&j(r#"{"confirm":true}"#)), Ok(ModeArgs::Confirm));
    assert_eq!(mode_args(&j(r#"{"undo":true}"#)), Ok(ModeArgs::Undo));
    assert_eq!(mode_args(&j(r#"{"confirm":false}"#)), Ok(ModeArgs::State));
    for bad in [r#"{"set":5}"#, r#"{"confirm":"yes"}"#, r#"{"set":"1920x1080","confirm":true}"#, r#"{"confirm":true,"undo":true}"#] {
        assert!(mode_args(&j(bad)).is_err(), "{bad}");
    }
}

/// V1's G-1 state: the GOP's 1024x768 framebuffer scaled to the 2560x1440 link (AUTOSCALE). A smaller size writes
/// only the viewports; the way back writes G-1's 0x03000400 again; a size above the framebuffer is refused.
#[test]
fn gop_1024_scaled_boot_state() {
    let home = Size::new(1024, 768);
    let a = attach_of(&mini_pc());
    let e = Engine::new(mini_pc());
    let small = |w: u32, h: u32| {
        let t = Arc::new(RamTarget::new((1024 * 768 * 4) as usize, 0));
        GopFb::new(t, w, h, 1024, Layout::Bgrx, "test").unwrap()
    };
    let s = Scanout::new(Er(e.clone()), FakeClock(e.clone()), &a, small(1024, 768), small(1024, 768), timing()).unwrap();
    let f = Fol::default();
    let mut sw = Switch::new(LINK, RATE, home);
    assert_eq!(sw.set(&s, &f, FHD), Err(SwitchError::Unoffered(Unoffered::Larger { max: home })));
    assert!(e.writes().is_empty());
    let Ok(Outcome::Pending { serial, .. }) = sw.set(&s, &f, Size::new(800, 600)) else { panic!() };
    let lock = l::otg(0, l::OTG_LOCK);
    assert_eq!(
        e.writes(),
        vec![(lock, l::LOCK), (l::hubp(0, l::HUBP_VIEWPORT), 0x0258_0320), (l::hubp(0, l::HUBP_SEC_VIEWPORT), 0x0258_0320), (lock, 0)]
    );
    e.advance(CONFIRM_NS + 1);
    assert_eq!(sw.expire(&s, &f, serial), Some(Ok(home)));
    assert_eq!(e.reg(l::hubp(0, l::HUBP_VIEWPORT)), 0x0300_0400);
}

#[test]
fn rollback_refused_by_a_gate_stays_due() {
    let mut r = rig(native(false));
    let Ok(Outcome::Pending { serial, .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    r.e.advance(CONFIRM_NS + 1);
    r.e.st().regs.insert(l::otg(0, l::OTG_LOCK), 1);
    assert!(matches!(r.sw.expire(&r.s, &r.f, serial), Some(Err(SwitchError::Gate(Gate::Locked(1))))));
    assert!(r.sw.pending().is_some(), "still due");
    r.e.st().regs.insert(l::otg(0, l::OTG_LOCK), 0);
    assert_eq!(r.sw.expire(&r.s, &r.f, serial), Some(Ok(LINK)));
}

// GX-4 fixer: the forced rollback, the one confirmation timer, the console command.

#[test]
fn rollback_that_does_not_land_stays_due_and_lands_on_the_retry() {
    let mut r = rig(native(false));
    let Ok(Outcome::Pending { serial, .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    r.e.advance(CONFIRM_NS + 1);
    {
        let mut st = r.e.st();
        st.refuse = Some(l::hubp(0, l::HUBP_SEC_VIEWPORT));
        st.refuse_once = true;
    }
    let e = r.sw.expire(&r.s, &r.f, serial).unwrap().unwrap_err();
    assert!(matches!(&e, SwitchError::RolledBack(w) if w.contains("read back")), "{e:?}");
    assert_eq!(mode_regs(&r.e).0, FHD.dim(), "the unconfirmed size was written back and shows");
    let now = r.s.clock().now_ns();
    assert_eq!(r.sw.wake(now), crate::switch::Wake::Expire(serial), "the rollback stays due at once");
    assert_eq!(r.sw.status(now).left_ns, Some(0));
    assert_eq!(r.sw.expire(&r.s, &r.f, serial), Some(Ok(LINK)), "the retry lands");
    assert_eq!(mode_regs(&r.e).0, LINK.dim());
    assert_eq!(r.sw.wake(r.s.clock().now_ns()), crate::switch::Wake::Done);
}

#[test]
fn rollback_to_a_confirmed_smaller_size_falls_to_the_boot_mode_when_it_does_not_land() {
    let mut r = rig(native(false));
    r.sw.set(&r.s, &r.f, FHD).unwrap();
    r.sw.confirm().unwrap();
    let Ok(Outcome::Pending { serial, .. }) = r.sw.set(&r.s, &r.f, Size::new(1600, 900)) else { panic!() };
    r.e.advance(CONFIRM_NS + 1);
    {
        let mut st = r.e.st();
        st.refuse = Some(l::hubp(0, l::HUBP_SEC_VIEWPORT));
        st.refuse_once = true;
    }
    let e = r.sw.expire(&r.s, &r.f, serial).unwrap().unwrap_err();
    assert!(matches!(&e, SwitchError::RolledBack(w) if w.contains("boot mode is back")), "{e:?}");
    let st = r.sw.status(r.s.clock().now_ns());
    assert_eq!((st.shown, st.confirmed, st.left_ns, st.follow_due), (LINK, LINK, None, Some(LINK)));
    assert_eq!(mode_regs(&r.e).0, LINK.dim());
    assert_eq!(r.sw.wake(0), crate::switch::Wake::Follow);
    assert!(r.sw.tick(&r.f));
    assert_eq!(r.sw.wake(0), crate::switch::Wake::Done);
}

#[test]
fn forced_rollback_lets_a_pending_flip_land_before_the_gates() {
    let mut r = rig(native(false));
    let Ok(Outcome::Pending { serial, .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    r.e.advance(CONFIRM_NS + 1);
    let (w, h) = r.s.size();
    let src = vec![0x0012_3456u32; (w * h) as usize];
    let back = r.s.begin(&src, w).unwrap();
    back.present(&src, w, (0, 0, 8, 8)).unwrap();
    r.s.commit().unwrap();
    assert!(r.e.reg(l::hubp(0, l::HUBP_FLIP)) & l::FLIP_PENDING != 0, "a flip is on its way");
    assert_eq!(r.sw.expire(&r.s, &r.f, serial), Some(Ok(LINK)), "the flip lands first, the gates pass");
}

/// The timer over the fake engine: `sleep_ns` advances the fake clock and runs the events due on the way.
struct Host {
    r: core::cell::RefCell<Rig>,
    events: core::cell::RefCell<Vec<(u64, fn(&mut Rig))>>,
    rolled: core::cell::RefCell<Vec<(u64, Size)>>,
    sleeps: core::cell::Cell<u32>,
}

impl Host {
    fn new(r: Rig) -> Host {
        Host { r: core::cell::RefCell::new(r), events: Default::default(), rolled: Default::default(), sleeps: Default::default() }
    }
    fn at(&self, t: u64, f: fn(&mut Rig)) {
        self.events.borrow_mut().push((t, f));
    }
}

impl crate::switch::TimerHost for Host {
    fn now_ns(&self) -> u64 {
        self.r.borrow().s.clock().now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        self.sleeps.set(self.sleeps.get() + 1);
        assert!(self.sleeps.get() < 1000, "the timer spins");
        assert!(ns <= CONFIRM_NS + 1, "a sleep of {ns} ns is longer than the window");
        let end = self.now_ns() + ns;
        loop {
            let next = {
                let mut ev = self.events.borrow_mut();
                ev.sort_by_key(|e| e.0);
                if ev.first().is_some_and(|e| e.0 <= end) { Some(ev.remove(0)) } else { None }
            };
            let Some((t, f)) = next else { break };
            let mut r = self.r.borrow_mut();
            let now = r.s.clock().now_ns();
            r.e.advance(t.saturating_sub(now));
            f(&mut r);
        }
        let now = self.now_ns();
        self.r.borrow().e.advance(end.saturating_sub(now));
    }
    fn wake(&self, now_ns: u64) -> Option<crate::switch::Wake> {
        Some(self.r.borrow().sw.wake(now_ns))
    }
    fn expire(&self, serial: u64) -> Option<Result<Size, SwitchError>> {
        let mut g = self.r.borrow_mut();
        let Rig { s, f, sw, .. } = &mut *g;
        sw.expire(s, f, serial)
    }
    fn tick(&self) -> bool {
        let mut g = self.r.borrow_mut();
        let Rig { f, sw, .. } = &mut *g;
        sw.tick(f)
    }
    fn rolled_back(&self, to: Size) {
        let t = self.now_ns();
        self.rolled.borrow_mut().push((t, to));
    }
}

fn set_on(r: &mut Rig, to: Size) {
    let Rig { s, f, sw, .. } = r;
    assert!(matches!(sw.set(s, f, to), Ok(Outcome::Pending { .. })));
}

#[test]
fn timer_rolls_back_once_after_the_window_and_ends() {
    let mut r = rig(native(false));
    set_on(&mut r, FHD);
    let t0 = r.s.clock().now_ns();
    let h = Host::new(r);
    crate::switch::run_timer(&h);
    let rolled = h.rolled.borrow().clone();
    assert_eq!(rolled.len(), 1, "{rolled:?}");
    assert!(rolled[0].0 >= t0 + CONFIRM_NS && rolled[0].0 < t0 + CONFIRM_NS + 100_000_000, "{rolled:?}");
    assert_eq!(rolled[0].1, LINK);
    assert_eq!(mode_regs(&h.r.borrow().e).0, LINK.dim());
}

#[test]
fn timer_ends_on_confirm_and_follows_a_second_set() {
    let mut r = rig(native(false));
    set_on(&mut r, FHD);
    let t0 = r.s.clock().now_ns();
    let h = Host::new(r);
    h.at(t0 + 5_000_000_000, |r| {
        r.sw.confirm().unwrap();
    });
    crate::switch::run_timer(&h);
    assert!(h.rolled.borrow().is_empty(), "confirmed: no rollback");
    assert!(h.now_ns() < t0 + CONFIRM_NS + 1_000_000_000, "the timer did not wait the window out for long");
    assert_eq!(mode_regs(&h.r.borrow().e).0, FHD.dim());

    let mut r = rig(native(false));
    set_on(&mut r, FHD);
    let t0 = r.s.clock().now_ns();
    let h = Host::new(r);
    h.at(t0 + 10_000_000_000, |r| set_on(r, Size::new(1600, 900)));
    crate::switch::run_timer(&h);
    let rolled = h.rolled.borrow().clone();
    assert_eq!(rolled.len(), 1, "one rollback for both switches: {rolled:?}");
    assert!(rolled[0].0 >= t0 + 10_000_000_000 + CONFIRM_NS, "the second set's window counts: {rolled:?}");
    assert_eq!(rolled[0].1, LINK);
}

#[test]
fn timer_retries_a_refused_rollback_with_a_growing_wait() {
    let mut r = rig(native(false));
    set_on(&mut r, FHD);
    let t0 = r.s.clock().now_ns();
    r.e.st().regs.insert(l::otg(0, l::OTG_LOCK), 1);
    let h = Host::new(r);
    h.at(t0 + CONFIRM_NS + 3_000_000_000, |r| {
        r.e.st().regs.insert(l::otg(0, l::OTG_LOCK), 0);
    });
    crate::switch::run_timer(&h);
    let rolled = h.rolled.borrow().clone();
    assert_eq!(rolled.len(), 1, "{rolled:?}");
    assert!(rolled[0].0 >= t0 + CONFIRM_NS + 3_000_000_000, "{rolled:?}");
    assert!(h.sleeps.get() < 12, "backoff, not a 200 ms poll: {} sleeps", h.sleeps.get());
    assert_eq!(crate::switch::backoff_ns(1), crate::switch::RETRY_MIN_NS);
    assert_eq!(crate::switch::backoff_ns(2), 2 * crate::switch::RETRY_MIN_NS);
    assert_eq!(crate::switch::backoff_ns(100), crate::switch::RETRY_MAX_NS);
}

#[test]
fn timer_retries_the_software_side_until_it_follows() {
    let mut r = rig(native(false));
    set_on(&mut r, FHD);
    let t0 = r.s.clock().now_ns();
    r.f.0.lock().unwrap().fail = true;
    let h = Host::new(r);
    h.at(t0 + CONFIRM_NS + 20_000_000_000, |r| r.f.0.lock().unwrap().fail = false);
    crate::switch::run_timer(&h);
    let g = h.r.borrow();
    assert_eq!(g.sw.status(0).follow_due, None);
    assert_eq!(g.f.sizes(), vec![FHD, LINK]);
    assert!(h.sleeps.get() < 20, "{} sleeps", h.sleeps.get());
}

#[test]
fn double_confirm_and_hostile_console_arguments() {
    let mut r = rig(native(false));
    assert_eq!(r.sw.confirm(), Err(SwitchError::NothingPending), "keep with nothing waiting");
    r.sw.set(&r.s, &r.f, FHD).unwrap();
    assert_eq!(r.sw.confirm(), Ok(FHD));
    assert_eq!(r.sw.confirm(), Err(SwitchError::NothingPending), "a second keep");
    assert_eq!(r.sw.undo(&r.s, &r.f), Err(SwitchError::NothingPending), "undo after keep");

    use crate::modeset::{Cmd, USAGE, cmd};
    assert_eq!(cmd(&[]), Ok(Cmd::List));
    assert_eq!(cmd(&["list"]), Ok(Cmd::List));
    for k in ["keep", "yes", "j", "ja", "confirm"] {
        assert_eq!(cmd(&[k]), Ok(Cmd::Keep), "{k}");
    }
    assert_eq!(cmd(&["undo"]), Ok(Cmd::Undo));
    assert_eq!(cmd(&["1920x1080@abc"]), Ok(Cmd::Set("1920x1080@abc")));
    assert_eq!(cmd(&["1920x1080", "now"]), Err(USAGE));
    for bad in ["1920x1080@abc", "keep2", "", "x", "@", "99999999999x1", "1920x1080@", "1920x1080@60@60", "\u{0}"] {
        assert!(mode::parse(bad).is_err(), "{bad:?}");
    }
    // The kernel glue with no engine (the host, QEMU): a clear refusal, no panic; the size is parsed first.
    assert!(crate::modeset::set("1920x1080@abc").is_err_and(|e| !e.contains("no display engine")));
    assert!(crate::modeset::confirm().is_err_and(|e| e.contains("no display engine")));
    assert!(crate::modeset::undo().is_err_and(|e| e.contains("no display engine")));
}

/// GX-4c: gx.mode.gates asks the gates' own predicate (`PipeRegs::known`) in both states the Mini-PC showed on
/// 22.10.: the fresh-boot firmware state (manual 1:1, the PipeRegs dump of the failure) and the state after a
/// switch (AUTOSCALE at 1920x1080), then manual again after the way back.
#[test]
fn gates_check_agrees_with_the_plan_builder_in_both_states() {
    let p = [pipe()];
    let fresh = mode::gates(&*Engine::new(mini_pc_native()), &p, LINK, LINK, true).unwrap();
    // The failure's dump: "pri_vp: 94374400, ... scl_mode: 1, lb_mem: 101072640, ..., filter: [16777216, ...]".
    assert_eq!((fresh.pri_vp, fresh.scl_mode, fresh.lb_mem, fresh.filter[0]), (94_374_400, 1, 101_072_640, 16_777_216));
    // The old check (AUTOSCALE or bypass) refused it: AUTOCAL_MODE 0 with DSCL_MODE 1 is neither.
    assert!(!fresh.scaling() && !fresh.bypass() && fresh.known(), "{fresh:?}");
    let mut r = rig(mini_pc_native());
    let Ok(Outcome::Pending { .. }) = r.sw.set(&r.s, &r.f, FHD) else { panic!() };
    let after = mode::gates(&*r.e, &p, LINK, LINK, false).unwrap();
    assert!(after.scaling() && after.known(), "{after:?}");
    assert_eq!((fresh.autocal & l::AUTOCAL_MODE, after.autocal & l::AUTOCAL_MODE), (0, 1), "the field that differs");
    assert_eq!((after.scl_mode, after.filter), (fresh.scl_mode, fresh.filter), "DSCL_MODE and the manual registers stay");
    assert_eq!(r.sw.undo(&r.s, &r.f), Ok(LINK));
    let back = mode::gates(&*r.e, &p, LINK, LINK, true).unwrap();
    assert!(back.manual() && back.known() && back.same_checked(&fresh), "{back:?}");
    // The other two known boot states, and one the switch does not know (AUTOCENTER).
    assert!(mode::read(&*Engine::new(native(false)), &pipe()).unwrap().known());
    assert!(mode::read(&*Engine::new(native(true)), &pipe()).unwrap().known());
    let mut m = mini_pc_native();
    m.insert(l::dpp(0, l::DSCL_AUTOCAL), 2);
    assert!(!mode::read(&*Engine::new(m), &pipe()).unwrap().known());
}

/// GX-4c: with the monitor's EDID (tag 21) the list is the monitor's, never the GOP's two sizes; without it the
/// list text says why (no tag 21, a refused EDID, or one without progressive timings).
#[test]
fn edid_list_beats_the_gop_list_and_the_fallback_says_why() {
    use display::edid::sample;
    let gop = [LINK, Size::new(1920, 1440)];
    let qhd = sample::dtd(24150, 2560, 160, 1440, 41, 48, 32, 3, 5, 0x1A);
    let mut edid = sample::edid(&[qhd], 0);
    edid[0x26..0x28].copy_from_slice(&[0xD1, 0xC0]);
    sample::fix_checksum(&mut edid[..128]);
    let (v, src) = mode::offered(Some(&edid), &gop, LINK);
    assert_eq!((src, v.iter().map(|o| o.size).collect::<Vec<_>>()), (Source::Edid, vec![LINK, FHD]));
    let (v, src) = mode::offered(None, &gop, LINK);
    assert_eq!((src, v.len()), (Source::Gop, 2), "gen 17: no tag 21");
    assert!(mode::edid_note(None).starts_with("no EDID from the loader"));
    let mut bad = edid.clone();
    bad[127] ^= 1;
    assert_eq!(mode::offered(Some(&bad), &gop, LINK).1, Source::Gop);
    assert!(mode::edid_note(Some(&bad)).starts_with("EDID refused"));
    let empty = sample::edid(&[], 0);
    assert_eq!(mode::offered(Some(&empty), &gop, LINK).1, Source::Gop);
    assert_eq!(mode::edid_note(Some(&empty)), "EDID without progressive timings");
    // The `display_mode {}` header (modeset::text) comes from the same source and EDID as the list.
    assert_eq!(mode::list_header(Source::Edid, Some(&edid)), "modes (edid):");
    assert_eq!(
        mode::list_header(Source::Gop, None),
        "modes (gop; no EDID from the loader (bitsboot before BT-edid, or the firmware has none)):"
    );
    assert_eq!(mode::list_header(Source::Gop, Some(&bad)), "modes (gop; EDID refused (short, bad header or base block checksum)):");
    assert_eq!(mode::list_header(Source::Link, Some(&empty)), "modes (link; EDID without progressive timings):");
}
