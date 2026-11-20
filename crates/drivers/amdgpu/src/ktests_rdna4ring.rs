//! Wave 44 in the kernel: the rings' stage `gcring` and its rungs `ringtest` on the model (`rdna4::fakering`,
//! `rdna4::fakecpeng`), under the kernel's own locks, clock and klog: `gcring` held against the section gc-ring with
//! no declared rule, the rungs against Linux's forms; what the model refuses; a ring test on a queue MES did not
//! map; a Reload's hooks in their order. QEMU has no RX 9070 XT: the model stands in for it. Each test reports how
//! deep below its start the stage reached the card.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakemodel::{Model, STAGE_STACK, stack_at};
use crate::rdna4::ihoff::OffIo;
use crate::rdna4::linuxseq::compare::Compare;
use crate::rdna4::linuxseq::fold::Bus;
use crate::rdna4::meskq::{self, KernelQueue};
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::sdmaoff::{Stopped, Written};
use crate::rdna4::stage::{self, Ctx, Opts, Session, SessionSlot, Stage};
use crate::rdna4::{cpoff, linuxseq, mesoff, ringoff, ringrungs};
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::mes12api::Unmap;
use rdna4_core::{gc12, gc12ring as rg, mes12};

type Rig = (crate::rdna4::attach::PciFacts, crate::rdna4::attach::Attached);

fn rig() -> Result<Rig, &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| (facts, a)).map_err(|_| "the measured card did not attach")
}

fn ctx(r: &Rig) -> Ctx<'_> {
    Ctx { index: 0, display: true, facts: &r.0, att: &r.1, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX }
}

fn nb(r: &Rig) -> Result<NbRegs, &'static str> {
    Regs14::from(&r.1.discovery).and_then(|x| NbRegs::from(&r.1.discovery, &x)).ok_or("the table does not name the registers")
}

/// The chain on the cold model up to `stage` (not run).
fn before(c: &Ctx, s: &mut Session, stage: Stage) -> Result<Model, &'static str> {
    let mut m = model();
    let at = Stage::CHAIN.iter().position(|st| *st == stage).ok_or("not in the chain")?;
    for st in &Stage::CHAIN[..at] {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, m, &mut Stick::full());
        m = back;
        if !d.pass {
            return Err("the chain before the rings did not pass on the model");
        }
    }
    m.writes.clear();
    Ok(m)
}

/// One stage that must pass, and how deep it reached the card.
fn one(s: &mut Session, c: &Ctx, m: Model, st: Stage) -> Result<(stage::Done, Model), &'static str> {
    let top = stack_at();
    let mut m = m;
    m.deepest = usize::MAX;
    let (d, m) = stage::run(s, Opts { stage: st }, c, m, &mut Stick::full());
    if !(d.pass && d.refused.is_empty() && d.trail_lost == 0 && m.refused.is_empty()) {
        return Err("a ring stage did not pass on the model");
    }
    let deep = top.saturating_sub(m.deepest);
    if deep > STAGE_STACK {
        return Err("a ring stage reached the card deeper in the stack than STAGE_STACK");
    }
    Ok((d, m))
}

pub(crate) fn stages() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let m = before(&c, &mut s, Stage::GcRing)?;
    let (d, mut m) = one(&mut s, &c, m, Stage::GcRing)?;
    let sec = linuxseq::section("gc-ring")?;
    let regs = Regs14::from(&r.1.discovery).ok_or("the table does not name the registers")?;
    let held = Compare::new(&sec, &[]).and_then(|mut cmp| cmp.feed_trail(&d.trail, 0, &Bus::of(&regs)).and_then(|_| cmp.finish()));
    if let Err(e) = held {
        return ktest::fail(&alloc::format!("gcring is not Linux's sequence: {e}"));
    }
    kassert!(sec.lines.len() == 26 && d.lines.last().is_some_and(|l| l == "linuxseq gc-ring: equal"), "{:?}", d.lines.last());
    let n = nb(&r)?;
    let ring0 = n.byte(rg::CP_INT_CNTL_RING0).ok_or("CP_INT_CNTL_RING0")?;
    kassert!(m.reg_read(ring0) == Some(0x01C0_0000) && s.rings.wptr == [256; 5] && s.next() == Some(Stage::Sdma), "{}", s.summary());
    Ok(())
}
ktest::ktest!("gx.rdna4.ring.stages", Safe, stages);

pub(crate) fn rungs() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let m = before(&c, &mut s, Stage::GcRing)?;
    let (_, m) = one(&mut s, &c, m, Stage::GcRing)?;
    let bound = s.nb.gart.as_ref().map(|g| g.bound.len());
    let (d, m) = one(&mut s, &c, m, Stage::RingTest)?;
    kassert!(s.rings.rungs.len() == ringrungs::RUNGS.len() && s.rings.rungs.iter().all(|x| x.1), "{:?}", s.rings.rungs);
    kassert!(d.lines.last().is_some_and(|l| l.starts_with("linuxseq gc-ring: equal: the call's register accesses are Linux's forms")), "{:?}", d.lines.last());
    kassert!(m.cp.raised.iter().filter(|x| x.0 == rg::SRC_EOP).count() == 7 && s.nb.gart.as_ref().map(|g| g.bound.len()) == bound && s.rings.ib_mc.is_none(), "{:?}", m.cp.raised);
    Ok(())
}
ktest::ktest!("gx.rdna4.ring.rungs", Safe, rungs);

/// A ring test on a queue MES unmapped: the doorbell is a posted write nothing answers, the stage stops with
/// -ETIMEDOUT and looks at GRBM_STATUS; the model refuses the section's registers out of Linux's order.
pub(crate) fn refusals() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let n = nb(&r)?;
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let mut m = before(&c, &mut s, Stage::GcRing)?;
    let refuse = |m: &mut Model, reg, v| {
        let before = m.refused.len();
        n.byte(reg).is_some_and(|b| !m.reg_write(b, v)) && m.refused.len() == before + 1
    };
    kassert!(refuse(&mut m, rg::CP_DEVICE_ID, 1) && refuse(&mut m, rg::CP_MAX_CONTEXT, 8) && refuse(&mut m, rg::CP_INT_CNTL_RING0, rg::f::PRIV_INSTR_INT_ENABLE.mask), "{:?}", m.refused);
    m.refused.clear();
    let up = s.mes.up.as_mut().ok_or("MES does not run")?;
    let q = KernelQueue::of(up, 0).ok_or("no compute queue")?;
    let legacy = q.legacy(&up.place);
    let sent = meskq::unmap_legacy(up, &mut m, &legacy, Unmap::ResetQueues, (0, 0)).map_err(|_| "the unmap did not go out")?;
    kassert!(sent.0.ok(), "the unmap was not answered");
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::GcRing }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.lines.iter().any(|l| l.contains("ring comp_1.0.0 test failed (-ETIMEDOUT)")) && d.lines.iter().any(|l| l.starts_with("gcring: after the stop GRBM_STATUS")), "{} {:?}", d.line, d.lines);
    kassert!(m.refused.len() == 1 && m.refused[0].contains("no queue was placed"), "{:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.ring.refusals", Safe, refusals);

struct Rec<'a>(&'a mut Model);

impl OffIo for Rec<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, us: u64) {
        self.0.delay_us(us)
    }
}

/// After the rings and their rungs, a Reload's three hooks of the graphics block in the order the chain registers
/// them: the rings' interrupt enables cleared, MES halted, then the command processor (gfx_v12_0.c:3888-3890
/// before :3902 before :3914), all taken by the model.
pub(crate) fn reload() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let n = nb(&r)?;
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let m = before(&c, &mut s, Stage::GcRing)?;
    let (_, m) = one(&mut s, &c, m, Stage::GcRing)?;
    let (_, mut m) = one(&mut s, &c, m, Stage::RingTest)?;
    let w = Written::default();
    w.set();
    m.writes.clear();
    let rr = ringoff::OffRegs::of(&n).ok_or("no interrupt controls")?;
    let ints_off = ringoff::off(&mut Rec(&mut m), &rr, &w) == Stopped::Halted;
    let all = rg::EOP_ENABLES | rg::f::PRIV_REG_INT_ENABLE.mask | rg::f::PRIV_INSTR_INT_ENABLE.mask | rg::f::OPCODE_ERROR_INT_ENABLE.mask;
    let left: alloc::vec::Vec<u32> = [rr.ring0, rr.pipe0, rr.pipe1].iter().filter_map(|b| m.reg_read(*b)).map(|v| v & all).collect();
    kassert!(ints_off && m.refused.is_empty() && left == [0, 0, 0], "the rings' enables left {left:x?}: {:?}", m.refused);
    let mes_halted = matches!(mesoff::halt(&mut Rec(&mut m), &n, &w), mesoff::Halted::Halted { .. });
    let cp_halted = cpoff::halt(&mut Rec(&mut m), &cpoff::OffRegs::of(&n).ok_or("no halt registers")?, &w) == Stopped::Halted;
    let at = |m: &Model, reg| n.byte(reg).and_then(|b| m.writes.iter().position(|(x, _)| *x == b));
    kassert!(mes_halted && cp_halted && m.refused.is_empty() && at(&m, rg::CP_ME1_PIPE1_INT_CNTL) < at(&m, mes12::CP_MES_CNTL) && at(&m, mes12::CP_MES_CNTL) < at(&m, gc12::CP_ME_CNTL), "{:?}", m.refused);
    kassert!(!m.mes.running.iter().any(|x| *x) && !m.cp.gfx_on && !m.cp.mec_on, "the halts did not take");
    Ok(())
}
ktest::ktest!("gx.rdna4.ring.reload", Safe, reload);
