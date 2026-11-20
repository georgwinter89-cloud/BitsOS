//! Wave 44 (TH) in the kernel: the thermal taker at the IH reader on the model, under the kernel's own locks,
//! clock and klog. QEMU has no RX 9070 XT: the model stands in for it, and the reader reaches it as the IH thread
//! reaches the real card. The power-off is a recorder here; the platform's is never called by a ktest.
//!
//! Each part is a function of its own that is never inlined: a model and a session are a few KiB by value, and
//! the gate's release run holds every test to 48 KiB of stack.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::attach::{Attached, PciFacts};
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::{Model, STAGE_STACK, stack_at};
use crate::rdna4::fakethermal::Alert;
use crate::rdna4::ihring::{IhPlace, IhUp};
use crate::rdna4::session::SessionSlot;
use crate::rdna4::stage::{self, Ctx, Opts, Session, Stage};
use crate::rdna4::thermal::{self, PowerOff};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::thm14;

fn rig() -> Result<Box<(PciFacts, Attached)>, &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| Box::new((facts, a))).map_err(|_| "the measured card did not attach")
}

fn ctx(r: &(PciFacts, Attached)) -> Ctx<'_> {
    Ctx { index: 0, display: true, facts: &r.0, att: &r.1, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX }
}

/// Linux's chain up to and with `pmfw` on a cold model: the alert armed.
#[inline(never)]
fn armed(c: &Ctx, s: &mut Session, mut m: Box<Model>) -> Result<Box<Model>, String> {
    let upto = Stage::CHAIN.iter().position(|x| *x == Stage::Pmfw).map_or(0, |i| i + 1);
    for st in &Stage::CHAIN[..upto] {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, *m, &mut Stick::full());
        *m = back;
        if !d.pass {
            return Err(alloc::format!("{st:?} did not pass: {}", d.line));
        }
    }
    Ok(m)
}

/// One look at the rings by a reader that reaches the card; how deep below `top` the taker reached it.
#[inline(never)]
fn pass(s: &mut Session, m: &mut Model, top: usize) -> Result<usize, String> {
    let ih = s.nb.ih.as_mut().ok_or("no IH")?;
    let sys = ih.place.sys;
    m.deepest = usize::MAX;
    ih.reader.process(&mut ModelIo { m, sys });
    Ok(if m.deepest == usize::MAX { 0 } else { top.saturating_sub(m.deepest) })
}

/// A rising alert, the check 50 ms later through one metrics transfer, a hot card switched off; no deeper in the
/// stack than a stage may be when it reaches the card.
fn check() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let mut m = armed(&c, &mut s, Box::new(model()))?;
    let top = stack_at();
    m.raise_thermal(Alert::High)?;
    pass(&mut s, &mut m, top)?;
    let t = &s.nb.ih.as_ref().ok_or("no IH")?.reader.thermal;
    kassert!(t.count == [1, 0, 0] && t.due_us.is_some(), "{}", t.describe());
    m.delay_us(49_999);
    pass(&mut s, &mut m, top)?;
    kassert!(s.nb.ih.as_ref().is_some_and(|i| i.reader.thermal.checks == 0), "the check ran before its delay");
    m.delay_us(1);
    m.pmfw.thm.temps.hotspot = 119;
    let deep = pass(&mut s, &mut m, top)?;
    let t = &s.nb.ih.as_mut().ok_or("no IH")?.reader.thermal;
    kassert!(t.checks == 1 && t.power_off.as_deref().is_some_and(|w| w.contains("hotspot 119 C")), "{}", t.describe());
    kassert!(m.messages.iter().filter(|x| **x == (0x12, 5)).count() == 1 && m.refused.is_empty(), "{:?}", m.refused);
    kassert!(deep <= STAGE_STACK, "the thermal taker reached the card {deep} bytes deep in the stack, over {STAGE_STACK}");
    Ok(())
}
ktest::ktest!("gx.rdna4.thermal", Safe, check);

/// MP1's interrupt: acknowledged first, a fan fault moves the upper threshold; THM's falling alert and a source
/// Linux does not take are only counted; the model refuses an acknowledge nobody asked for.
fn alerts() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let mut m = armed(&c, &mut s, Box::new(model()))?;
    let top = stack_at();
    let (ctrl, thm) = (m.regs.mp1_ih_sw_int_ctrl, m.regs.thm_int_ctrl);
    let at = m.writes.len();
    m.raise_thermal(Alert::Smu(thm14::CTX_FAN_ABNORMAL))?;
    m.raise_thermal(Alert::Smu(3))?;
    m.raise_thermal(Alert::Low)?;
    m.raise(rdna4_core::ih7::Ring::Ring0, &rdna4_core::ih7::Iv { client_id: thm14::CLIENT_THM, src_id: 2, ..Default::default() })?;
    pass(&mut s, &mut m, top)?;
    let mine: Vec<(u32, u32)> = m.writes[at..].iter().copied().filter(|(b, _)| *b == ctrl || *b == thm).collect();
    kassert!(mine == [(ctrl, 0x100), (thm, 0xb8ff_0076), (ctrl, 0x100)], "{mine:x?} {:?}", m.refused);
    let t = &s.nb.ih.as_ref().ok_or("no IH")?.reader.thermal;
    kassert!(t.count == [0, 1, 2] && t.foreign.values().sum::<u64>() == 1 && t.due_us.is_none(), "{}", t.describe());
    kassert!(!m.reg_write(ctrl, 0x100) && m.refused.last().is_some_and(|w| w.contains("no power controller interrupt pending")), "{:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.thermal_alerts", Safe, alerts);

struct Recorder<'a> {
    slot: &'a SessionSlot,
    free: Option<bool>,
}

impl PowerOff for Recorder<'_> {
    fn power_off(&mut self, _why: &str) -> String {
        self.free = Some(self.slot.0.try_lock().is_some());
        String::from("recorded, not switched off")
    }
}

/// The power-off a check decided runs with the session's lock dropped, once, and its failure is said; one that
/// did not happen is tried again with the same reason (the IH is off by then), the lock dropped again; the
/// thread's wait follows a pending check. The platform's power-off is never called here.
fn power_off() -> ktest::TestResult {
    let slot = Box::new(SessionSlot::new());
    {
        let mut s = slot.0.lock();
        let mut ih = IhUp::new(IhPlace { sys: 0, ring_mc: [0, 0], wb_mc: 0 }, String::from("test"));
        ih.reader.thermal.power_off = Some(String::from("hotspot 120 C"));
        s.nb.ih = Some(ih);
    }
    let mut off = Recorder { slot: &slot, free: None };
    let mut watch = thermal::OffWatch::default();
    let take = || slot.0.lock().nb.ih.as_mut().and_then(|i| i.reader.thermal.take_power_off());
    let failed = watch.after(take, &mut off);
    kassert!(off.free == Some(true), "the power-off was called with the session's lock held: {:?}", off.free);
    kassert!(failed.as_deref().is_some_and(|f| f.contains("hold the power button")) && watch.failed.as_deref() == Some("hotspot 120 C"), "{failed:?}");
    off.free = None;
    let again = watch.after(|| None, &mut off);
    kassert!(off.free == Some(true) && watch.tries == 1, "the retry: {:?} {}", off.free, watch.tries);
    kassert!(again.as_deref().is_some_and(|f| f.contains("tried again 1 times")), "{again:?}");
    off.free = None;
    let nothing = thermal::OffWatch::default().after(|| slot.0.lock().nb.ih.as_mut().and_then(|i| i.reader.thermal.take_power_off()), &mut off);
    kassert!(nothing.is_none() && off.free.is_none(), "a second power-off");
    kassert!(thermal::wait_ns(1_000_000_000, None) == 1_000_000_000 && thermal::wait_ns(1_000_000_000, Some(50_000)) == 50_000_000, "the thread's wait");
    Ok(())
}
ktest::ktest!("gx.rdna4.thermal_poweroff", Safe, power_off);
