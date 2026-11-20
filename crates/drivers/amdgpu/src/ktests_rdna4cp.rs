//! Wave 44, item CP in the kernel: the command processor's resume (`gccp`) on the model (`rdna4::fakecp`), under
//! the kernel's own locks, clock and klog, held against the trace's section gc-cp with its one declared address;
//! what the model refuses; the gfx queue MES mapped runs Linux's ring test and fence and halts at a CE packet;
//! the doorbells of the CP and of SDMA kept apart; and the Reload hook's halt. QEMU has no RX 9070 XT: the model
//! stands in for it. Each test reports how deep below its start the stage reached the card.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::cpoff::{self, OffRegs};
use crate::rdna4::cpseq;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::meskq::KernelQueue;
use crate::rdna4::ringops;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakemodel::{Model, STAGE_STACK, stack_at};
use crate::rdna4::ihoff::OffIo;
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::sdmaoff::{Stopped, Written};
use crate::rdna4::stage::{self, Ctx, Opts, Session, Stage};
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use amdgpu_fmt::{gfx12, pm4};
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::{doorbell12 as db, gc12, gc12cp as cp};

type Rig = (crate::rdna4::attach::PciFacts, crate::rdna4::attach::Attached);

fn rig() -> Result<Box<Rig>, &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| Box::new((facts, a))).map_err(|_| "the measured card did not attach")
}

fn ctx(r: &Rig) -> Ctx<'_> {
    Ctx { index: 0, display: true, facts: &r.0, att: &r.1, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX }
}

fn names(r: &Rig) -> Result<(Regs14, NbRegs), &'static str> {
    let regs = Regs14::from(&r.1.discovery).ok_or("the table does not name the registers")?;
    let n = NbRegs::from(&r.1.discovery, &regs).ok_or("the table does not name the registers")?;
    Ok((regs, n))
}

/// The chain up to `gccp`, every stage passing.
fn hub_up(c: &Ctx, s: &mut Session) -> Result<Model, &'static str> {
    let at = Stage::CHAIN.iter().position(|st| *st == Stage::GcCp).ok_or("gccp is not in the chain")?;
    let mut m = model();
    for st in &Stage::CHAIN[..at] {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, m, &mut Stick::full());
        m = back;
        if !d.pass {
            return Err("a stage of the chain did not pass on the model");
        }
    }
    m.writes.clear();
    Ok(m)
}

fn stage_on(c: &Ctx, s: &mut Session) -> Result<Model, &'static str> {
    let m = hub_up(c, s)?;
    let (d, m) = stage::run(s, Opts { stage: Stage::GcCp }, c, m, &mut Stick::full());
    if !d.pass {
        return Err("gccp did not pass on the model");
    }
    Ok(m)
}

fn stages() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (regs, n) = names(&r)?;
    let mut s = Box::new(Session::default());
    let mut m = hub_up(&c, &mut s)?;
    m.deepest = usize::MAX;
    let top = stack_at();
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::GcCp }, &c, m, &mut Stick::full());
    let deep = top.saturating_sub(m.deepest);
    kassert!(d.pass && d.refused.is_empty() && d.trail_lost == 0 && d.totals.writes == 82 && m.refused.is_empty(), "gccp: {} {:?} {:?}", d.line, d.lines, m.refused);
    kassert!(deep <= STAGE_STACK, "gccp reached the card {deep} bytes deep in the stack, over {STAGE_STACK}");
    let (l, b) = (s.layout.clone().ok_or("no layout")?, s.bump.as_ref().ok_or("no VRAM manager")?);
    if let Err(e) = cpseq::equal(&d, &regs, &l, b) {
        return ktest::fail(&format!("gccp is not Linux's sequence: {e}"));
    }
    kassert!(d.lines.last().map(|x| x.as_str()) == Some("linuxseq gc-cp: equal"), "{:?}", d.lines.last());
    let rd = |m: &mut Model, x| n.byte(x).and_then(|b| m.reg_read(b));
    kassert!(rd(&mut m, gc12::CP_ME_CNTL) == Some(0x0100_A000) && rd(&mut m, gc12::CP_MEC_RS64_CNTL) == Some(0x3C00_0000) && rd(&mut m, cp::RLC_CP_SCHEDULERS) == Some(0x30E8), "the engines or the KIQ's place");
    kassert!(s.passed(Stage::GcCp) && s.next() == Some(Stage::Mes), "next {:?}", s.next());
    let writes = m.writes.len();
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::GcCp }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.is_empty() && m.writes.len() == writes, "gccp ran a second time: {}", d.line);
    Ok(())
}
ktest::ktest!("gx.rdna4.cp.stages", Safe, stages);

fn refused(m: &mut Model, byte: Option<u32>, v: u32, why: &str) -> ktest::TestResult {
    let before = m.refused.len();
    let byte = byte.ok_or("a register has no place in BAR5")?;
    kassert!(!m.reg_write(byte, v), "{byte:#x} = {v:#x} was taken ({why})");
    kassert!(m.refused.len() == before + 1 && m.refused[before].contains(why), "{:?}, wanted {why}", &m.refused[before..]);
    Ok(())
}

fn refusals() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (_, n) = names(&r)?;
    let b = |x| n.byte(x);
    // Before power: no unhalt.
    let mut cold = model();
    refused(&mut cold, b(gc12::CP_MEC_RS64_CNTL), 0x3C00_0000, "before the graphics block has power and its autoload is complete")?;
    drop(cold);
    let mut s = Box::new(Session::default());
    let mut m = hub_up(&c, &mut s)?;
    refused(&mut m, b(gc12::CP_MEC_RS64_CNTL), 0x3C00_0000, "before its doorbell range holds Linux's values")?;
    refused(&mut m, b(cp::CP_MEC_DOORBELL_RANGE_UPPER), db::MEC_UPPER + 8, "Linux writes")?;
    refused(&mut m, b(gc12::CP_ME_CNTL), 0x0000_A000, "CE_HALT cleared")?;
    refused(&mut m, b(cp::GRBM_GFX_INDEX), cp::select_se_sa(Some(4), Some(0)), "one shader array of the card's at a time")?;
    refused(&mut m, b(cp::SH_MEM_BASES), cp::SH_MEM_BASES_GFX, "under VMID 0")?;
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4C, "0x4b dwords")?;
    refused(&mut m, b(cp::RLC_SRM_CNTL), 3, "before the clear-state buffer")?;
    refused(&mut m, b(db::ENTRY_0), 0, "Linux writes 0x30000007")?;
    refused(&mut m, b(cp::RLC_CP_SCHEDULERS), 0x3038, "Linux writes 0x30e8")?;
    Ok(())
}
ktest::ktest!("gx.rdna4.cp.refusals", Safe, refusals);

/// The gfx queue MES mapped (the chain through `mesq`) on the model's unhalted CP: Linux's ring test and a fence
/// run, a CE packet halts it with its bad-opcode entry; the CP's doorbells and SDMA's keep apart.
fn queue() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (_, n) = names(&r)?;
    let mut s = Box::new(Session::default());
    let mut m = stage_on(&c, &mut s)?;
    for st in [Stage::Mes, Stage::MesQ] {
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut Stick::full());
        m = back;
        kassert!(d.pass, "{}", d.line);
    }
    let up = s.mes.up.as_ref().ok_or("MES does not run")?;
    let q = KernelQueue::of(up, 4).ok_or("no gfx queue")?;
    kassert!(m.cp.queues.iter().any(|x| x.at.doorbell_dw == db::GFX_RING0_DW && x.at.gfx), "MES did not hand the gfx queue to the CP's model");
    let scratch = n.byte(cp::SCRATCH_REG0).ok_or("SCRATCH_REG0 has no place")?;
    kassert!(m.reg_write(scratch, gfx12::RING_TEST_POISON), "the poison");
    let mut v = Vec::new();
    gfx12::ring_test(&mut v, scratch / 4).map_err(|_| "ring test")?;
    gfx12::fence(&mut v, up.place.slot_mc(q.slots.fence), 7, false, false).map_err(|_| "fence")?;
    let ce = v.len() as u32;
    v.push(pm4::packet3(0x84, 0));
    v.push(0);
    let wptr = ringops::commit(&mut m, &up.place, &q, 0, &v).map_err(|_| "the frame did not reach the ring")?;
    let fence = crate::rdna4::sdmaq::sys_read32(&mut m, up.place.slot_sys(q.slots.fence)).map_err(|_| "fence slot")?;
    let rptr = crate::rdna4::cpring::get_rptr(&mut m, up.place.slot_sys(q.slots.rptr)).ok_or("rptr slot")?;
    kassert!(wptr == 256 && m.reg_read(scratch) == Some(0xDEAD_BEEF) && fence == 7, "ring test or fence");
    kassert!(rptr == u64::from(ce) && m.cp.queues.iter().any(|x| x.halted.is_some()), "the CE packet did not halt the queue at itself: rptr {rptr}");
    m.refused.clear();
    kassert!(!m.doorbell_write64(0x200, 0x40) && m.refused.first().is_some_and(|x| x.contains("for the engines")), "0x200 is SDMA's: {:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.cp.queue", Safe, queue);

struct Off<'a>(&'a mut Model);

impl OffIo for Off<'_> {
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

/// The Reload hook's halt (`cpoff`): nothing before the stage set out to write the block, Linux's fini after it.
fn reload() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (_, n) = names(&r)?;
    let mut s = Box::new(Session::default());
    let mut m = stage_on(&c, &mut s)?;
    let regs = OffRegs::of(&n).ok_or("no registers for the halt")?;
    let w = Written::default();
    m.writes.clear();
    kassert!(cpoff::halt(&mut Off(&mut m), &regs, &w) == Stopped::Untouched && m.writes.is_empty(), "the hook wrote before the stage's word");
    w.set();
    kassert!(cpoff::halt(&mut Off(&mut m), &regs, &w) == Stopped::Halted && !m.cp.gfx_on && !m.cp.mec_on && m.refused.is_empty(), "the halt: {:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.cp.reload", Safe, reload);
