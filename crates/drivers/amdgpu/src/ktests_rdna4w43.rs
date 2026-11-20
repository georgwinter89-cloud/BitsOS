//! Wave 43 in the kernel, across its items: on the model under the kernel's own locks, clock and klog, the rules
//! that hold BETWEEN the power controller (`pmfw`), the graphics block's start and hub (`gcboot`, `gchub`) and
//! the copy engines (`sdma`, `sdmatest`). One run from a cold card through the rungs with every covered section
//! equal to Linux's trace is `gx.rdna4.linuxseq`; here: no graphics register before the features are on and the
//! autoload completed; a bad address of an engine arrives at the fault decode through the IH reader a stage
//! runs itself; a second `fw` is refused once the engines ran. QEMU has no RX 9070 XT: the model stands in.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakemodel::Model;
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::sdmaq::lay;
use crate::rdna4::sdmaregs::SdmaRegs;
use crate::rdna4::stage::{self, Ctx, Opts, Session, SessionSlot, Stage};
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use amdgpu_fmt::sdma as pkt;
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::hub41::GC;
use rdna4_core::{gc12, sdma7};

type Rig = (crate::rdna4::attach::PciFacts, crate::rdna4::attach::Attached);

fn rig() -> Result<Rig, &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| (facts, a)).map_err(|_| "the measured card did not attach")
}

fn ctx(r: &Rig) -> Ctx<'_> {
    Ctx { index: 0, display: true, facts: &r.0, att: &r.1, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX }
}

/// The chain on the cold model up to and without `until`: every stage must pass.
fn chain_before(c: &Ctx, s: &mut Session, until: Stage) -> Result<Model, &'static str> {
    let at = Stage::CHAIN.iter().position(|st| *st == until).ok_or("no stage of the chain")?;
    pass(c, s, model(), &Stage::CHAIN[..at])
}

fn pass(c: &Ctx, s: &mut Session, mut m: Model, stages: &[Stage]) -> Result<Model, &'static str> {
    let mut stick = Stick::full();
    for st in stages {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, m, &mut stick);
        m = back;
        if !d.pass || !m.refused.is_empty() {
            return Err("a stage of the chain did not pass on the model");
        }
    }
    Ok(m)
}

/// One write of each kind of graphics register the wave's stages make (the pipe's selection before a program
/// counter, GFXHUB's table base, an engine's halt) and an engine's doorbell: all refused, each with the words
/// both models share.
#[inline(never)]
fn all_refused(m: &mut Model, bytes: &[u32; 3], state: &str) -> ktest::TestResult {
    m.refused.clear();
    kassert!(!m.gc_awake(), "{state}: the block is awake");
    for (i, b) in bytes.iter().enumerate() {
        kassert!(!m.reg_write(*b, u32::from(i == 2)) && m.refused.len() == i + 1 && m.refused[i].contains("its autoload is"), "{state}: write {i}: {:?}", m.refused);
    }
    kassert!(!m.doorbell_write64(sdma7::doorbell_dw(0), 0x40) && m.refused.len() == 4 && m.refused[3].contains("its autoload is"), "{state}: the doorbell: {:?}", m.refused);
    m.refused.clear();
    Ok(())
}

/// No graphics register and no doorbell before the features are on AND the autoload completed; no stage of the
/// wave before the one before it passed; and the power controller's start is what opens the block.
pub(crate) fn gate() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let n = Regs14::from(&r.1.discovery).and_then(|x| NbRegs::from(&r.1.discovery, &x)).ok_or("the table does not name the registers")?;
    let sr = SdmaRegs::from(&r.1.discovery).ok_or("no SDMA registers")?;
    let bytes = [n.byte(gc12::GRBM_GFX_CNTL).ok_or("no place")?, n.byte(GC.ctx0_base_hi).ok_or("no place")?, sr.byte(sdma7::MCU_CNTL, 0).ok_or("no place")?];
    all_refused(&mut model(), &bytes, "cold")?;
    // The features forced on without the autoload (no chain reaches this).
    let mut s = Box::new(Session::default());
    let mut early = chain_before(&c, &mut s, Stage::Autoload)?;
    early.pmfw.enabled = true;
    all_refused(&mut early, &bytes, "the features without the autoload")?;
    drop(early);
    // The autoload without the features: the chain's own state before `pmfw`.
    let mut s = Box::new(Session::default());
    let mut m = chain_before(&c, &mut s, Stage::Pmfw)?;
    all_refused(&mut m, &bytes, "the autoload without the features")?;
    for st in [Stage::GcBoot, Stage::GcHub, Stage::GcCp, Stage::Sdma, Stage::SdmaTest] {
        let writes = m.writes.len();
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut Stick::full());
        m = back;
        kassert!(!d.pass && d.trail.is_empty() && m.writes.len() == writes && m.refused.is_empty(), "{st:?} before pmfw: {}", d.line);
    }
    let mut m = pass(&c, &mut s, m, &[Stage::Pmfw])?;
    kassert!(m.gc_awake() && s.pmfw.gc_powered() && bytes.iter().enumerate().all(|(i, b)| m.reg_write(*b, u32::from(i == 2))), "after pmfw: {:?}", m.refused);
    m.pmfw.enabled = false;
    all_refused(&mut m, &bytes, "the features gone again")
}
ktest::ktest!("gx.rdna4.w43.gate", Safe, gate);

/// An engine told to write a page no entry holds: the fault is latched in GFXHUB's status, arrives through ring 1
/// of the IH, and the reader the stage `sdmatest` runs itself reads it, clears it and says it in plain words.
pub(crate) fn fault() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    // The device's own slot type, so the session lives under the lock the live tool and the IH thread take.
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let m = chain_before(&c, &mut s, Stage::Sdma)?;
    let mut m = pass(&c, &mut s, m, &[Stage::Sdma])?;
    let up = s.sdma.up.as_ref().ok_or("the engines are not up")?;
    let (sys, wptr_slot, unbound) = (up.place.sys, up.place.slot_sys(up.q[0].slots.wptr), up.place.mc + lay::TEST);
    kassert!(m.gc_walk(up.place.ring_mc(0)).is_some() && m.gc_walk(unbound).is_none(), "GFXHUB's walker: the ring bound, the test pages not");
    let mut w = Vec::new();
    if pkt::write_linear(&mut w, unbound, &[0x1234_5678]).is_err() {
        return ktest::fail("the packet did not encode");
    }
    kassert!(crate::ktests_rdna4sdma::push(&mut m, sys, wptr_slot, 0, &w), "the doorbell: {:?}", m.refused);
    kassert!(m.faults_open() == [true, false] && m.sdma.e[0].halted.is_some(), "the fault is not latched in GFXHUB's status: {:?}", m.refused);
    m.refused.clear();
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::SdmaTest }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.refused.is_empty() && m.refused.is_empty() && m.faults_open() == [false, false], "{} {:?} {:?}", d.line, d.refused, m.refused);
    let ih = s.nb.ih.as_ref().ok_or("no IH")?;
    let said = ih.reader.faults.last.map(|f| format!("{f}")).unwrap_or_default();
    kassert!(ih.reader.faults.count == [1, 0] && said.ends_with("SDMA0 (client id 0xd) tried to write; the page does not allow it (permission faults 0x5)"), "{} {said}", ih.reader.describe());
    kassert!(d.lines.iter().any(|l| l.starts_with("linuxseq mes: the same as Linux's as far as the stage came") && l.contains("4 accesses of Linux's interrupt handlers")), "{:?}", d.lines);
    Ok(())
}
ktest::ktest!("gx.rdna4.w43.fault", Safe, fault);

/// A second `fw` in one boot once the engines ran: refused before any access; and the model refuses the SMU's
/// image while a ring is on (the session made to forget the copy engines and the command processor).
pub(crate) fn again() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let m = pass(&c, &mut s, model(), &Stage::CHAIN)?;
    let mut m = pass(&c, &mut s, m, &[Stage::Destroy, Stage::Ring, Stage::Toc, Stage::Tmr])?;
    m.writes.clear();
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Fw }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && m.refused.is_empty() && d.line.contains("erst neu starten"), "{}", d.line);
    kassert!(s.sdma.up.is_some() && s.gc.hub && s.pmfw.passed && m.gc.context0 && m.sdma.any_on(), "the refused fw changed the card or the session");
    s.sdma = crate::rdna4::sdmastage::State::new();
    // Wave 44: MES ran too, and refuses the second fw on its own account, before any access.
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::Fw }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.is_empty() && m.refused.is_empty() && d.line.contains("der Planer der Karte wurde in diesem Start schon gestartet"), "{}", d.line);
    // A session that forgot MES and the command processor too, on a card whose MES was halted as a Reload halts
    // it: the engines' rings still
    // stop the SMU's image on the model.
    s.mes = crate::rdna4::messtage::State::new();
    s.cp = crate::rdna4::cpstage::State::new();
    let cntl = NbRegs::from(&r.1.discovery, &Regs14::from(&r.1.discovery).ok_or("regs")?).ok_or("nb regs")?.byte(rdna4_core::mes12::CP_MES_CNTL).ok_or("CP_MES_CNTL")?;
    let v = m.reg_read(cntl).ok_or("CP_MES_CNTL does not read")?;
    kassert!(m.reg_write(cntl, rdna4_core::mes12::halted(v)) && !m.mes.running.iter().any(|r| *r), "MES did not take its halt: {:?}", m.refused);
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Fw }, &c, m, &mut Stick::full());
    kassert!(!d.pass && m.refused.len() == 1 && m.refused[0].starts_with("LOAD_IP_FW of the SMU's firmware while a copy engine's ring is on"), "{} {:?}", d.line, m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.w43.again", Safe, again);
