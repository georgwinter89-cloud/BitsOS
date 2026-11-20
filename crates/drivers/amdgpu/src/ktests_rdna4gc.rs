//! Wave 43, item HB in the kernel: the graphics block's start and its hub (`gcboot`, `gchub`) on the model
//! (`rdna4::fakegc`, `fakegchub`), under the kernel's own locks, clock and klog, each held against its section of
//! Linux's trace with only the declared rules (the autoload's loop, the three addresses MMHUB was told); then
//! what the model refuses, a card whose autoload never completes, page faults of both hubs through the IH, the
//! flush of both hubs after a bind, GFXHUB switched off for a Reload, and a second `fw` in one boot. QEMU has no
//! RX 9070 XT: the model stands in for it.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::{Model, STAGE_STACK, stack_at};
use crate::rdna4::gcstage;
use crate::rdna4::guard::Mc;
use crate::rdna4::ihoff::{Off, OffIo};
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::stage::{self, Ctx, Opts, Session, Stage};
use crate::rdna4::{gart12, gchuboff};
use alloc::format;
use alloc::vec::Vec;
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::fault12::st;
use rdna4_core::gc12;
use rdna4_core::hub41::{self, GC, Kind};
use rdna4_core::nbreg::member;

type Rig = (crate::rdna4::attach::PciFacts, crate::rdna4::attach::Attached);

fn rig() -> Result<Rig, &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| (facts, a)).map_err(|_| "the measured card did not attach")
}

fn ctx(r: &Rig) -> Ctx<'_> {
    Ctx { index: 0, display: true, facts: &r.0, att: &r.1, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX }
}

/// The chain up to `gcboot` on `m` (whatever stands before it), then `more`: every stage must pass.
fn up(c: &Ctx, s: &mut Session, mut m: Model, more: &[Stage]) -> Result<Model, &'static str> {
    let at = Stage::CHAIN.iter().position(|st| *st == Stage::GcBoot).ok_or("gcboot is not in the chain")?;
    for st in Stage::CHAIN[..at].iter().chain(more) {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, m, &mut Stick::full());
        m = back;
        if !d.pass {
            return Err("a stage of the chain did not pass on the model");
        }
    }
    Ok(m)
}

fn names(r: &Rig) -> Result<(Regs14, NbRegs), &'static str> {
    let regs = Regs14::from(&r.1.discovery).ok_or("the table does not name the registers")?;
    let n = NbRegs::from(&r.1.discovery, &regs).ok_or("the table does not name the registers")?;
    Ok((regs, n))
}

fn stages() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (regs, n) = names(&r)?;
    let mut s = Session::default();
    let mut m = up(&c, &mut s, model(), &[])?;
    kassert!(m.gc_awake() && s.next() == Some(Stage::GcBoot), "the block is not awake after ta: next {:?}", s.next());
    let top = stack_at();
    for (st, writes) in [(Stage::GcBoot, 34), (Stage::GcHub, 145)] {
        m.deepest = usize::MAX;
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut Stick::full());
        m = back;
        kassert!(d.pass && d.refused.is_empty() && d.trail_lost == 0 && d.totals.writes == writes, "{st:?}: {} {:?}", d.line, d.lines);
        if let Err(e) = gcstage::equal(&d, &regs, s.nb.gart.as_ref()) {
            return ktest::fail(&format!("{st:?} is not Linux's sequence: {e}"));
        }
        let deep = top.saturating_sub(m.deepest);
        kassert!(deep <= STAGE_STACK, "{st:?} reached the card {deep} bytes deep in the stack, over {STAGE_STACK}");
    }
    kassert!(m.refused.is_empty() && m.gc.context0 && m.gc.grbm == 0 && m.gc.pulses == [1, 1, 1] && m.gc.starts.len() == 16, "{:?} {:?}", m.refused, m.gc.pulses);
    kassert!(s.gc.hub && s.next() == Stage::CHAIN.iter().skip_while(|x| **x != Stage::GcHub).nth(1).copied() && s.flush_regs(&n).is_some_and(|f| f.gc.is_some()), "the session after both stages: next {:?}", s.next());
    // GFXHUB was told the table MMHUB runs on, and the engines are still halted.
    let rd = |m: &mut Model, x| n.byte(x).and_then(|b| m.reg_read(b));
    kassert!(rd(&mut m, GC.ctx0_base_lo) == rd(&mut m, hub41::MM.ctx0_base_lo) && rd(&mut m, GC.fault_default_lo) == rd(&mut m, hub41::MM.fault_default_lo), "GFXHUB's addresses are not MMHUB's");
    kassert!(rd(&mut m, gc12::CP_ME_CNTL) == Some(0x1500_A000) && rd(&mut m, gc12::CP_MEC_RS64_CNTL) == Some(0x4000_0000), "an engine's halt moved");
    // A second run of either: refused before any access.
    for st in [Stage::GcBoot, Stage::GcHub] {
        let writes = m.writes.len();
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut Stick::full());
        m = back;
        kassert!(!d.pass && d.trail.is_empty() && m.writes.len() == writes, "{st:?} ran a second time: {}", d.line);
    }
    Ok(())
}
ktest::ktest!("gx.rdna4.gc.stages", Safe, stages);

/// One write the model must refuse, with its reason.
fn refused(m: &mut Model, byte: Option<u32>, v: u32, why: &str) -> ktest::TestResult {
    let (byte, before) = (byte.ok_or("a register has no place")?, m.refused.len());
    kassert!(!m.reg_write(byte, v) && m.refused.len() == before + 1 && m.refused[before].contains(why), "{byte:#x} = {v:#x}: {:?}", &m.refused[before..]);
    Ok(())
}

fn refusals() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (_, n) = names(&r)?;
    let b = |x| n.byte(x);
    let eng = |x| n.byte(member(x, hub41::GART_ENGINE, 1));
    // No power (the card's power controller lost its features behind the driver's back; the model's one power
    // answer is the power controller's model): the block answers nothing and takes nothing, and the stage's wait
    // is Linux's loop to its end.
    let mut s = Session::default();
    let mut m = up(&c, &mut s, model(), &[])?;
    kassert!(m.gc_powered() && s.pmfw.gc_powered(), "the chain's pmfw gave the block its power");
    m.pmfw.enabled = false;
    kassert!(b(gc12::CP_ME_CNTL).and_then(|x| m.reg_read(x)) == Some(0) && !m.gc_awake(), "a block without power answered");
    refused(&mut m, b(gc12::GRBM_GFX_CNTL), 0, "before the graphics block has power and its autoload is complete")?;
    m.refused.clear();
    let (writes, t) = (m.writes.len(), m.card.now_us);
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::GcBoot }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.totals.writes == 0 && m.writes.len() == writes && d.trail.len() as u64 + d.trail_lost == 200_000 && m.card.now_us - t == 100_000, "{}: {} accesses, {} us", d.line, d.trail.len() as u64 + d.trail_lost, m.card.now_us - t);
    kassert!(d.lines.iter().any(|l| l.starts_with("stopped: rlc autoload: gc ucode autoload timeout after 100000 rounds")), "{:?}", d.lines.last());
    // Power: the stage passes, and then the model's own refusals.
    m.pmfw.enabled = true;
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::GcBoot }, &c, m, &mut Stick::full());
    kassert!(d.pass && m.refused.is_empty(), "{} {:?}", d.line, m.refused);
    refused(&mut m, b(gc12::GRBM_GFX_CNTL), 2, "Linux selects pipe 0 and 1 of ME 0 and pipe 0 to 3 of ME 1")?;
    refused(&mut m, b(gc12::CP_MEC_RS64_PRGRM_CNTR_START), 0xC00, "Linux writes it under 0x4 to 0x7 only")?;
    kassert!(b(gc12::GRBM_GFX_CNTL).is_some_and(|x| m.reg_write(x, 5)), "pipe 1 of ME 1 was not taken");
    refused(&mut m, b(gc12::CP_PFP_PRGRM_CNTR_START), 0xC00, "Linux writes it under 0x0 to 0x1 only")?;
    refused(&mut m, b(gc12::CP_MEC_RS64_CNTL), 0x400F_0000, "still selects 0x5")?;
    refused(&mut m, b(GC.ctx0_base_lo), 1, "still selects 0x5")?;
    kassert!(m.gc.grbm == 5 && b(gc12::GRBM_GFX_CNTL).is_some_and(|x| m.reg_write(x, 0)), "the selection is not kept");
    // A write that clears a HALT bit is the command processor's unhalt (`fakecp`, wave 44): Linux's form only.
    refused(&mut m, b(gc12::CP_ME_CNTL), 0x0500_A000, "Linux's unhalt writes 0x100a000")?;
    refused(&mut m, b(gc12::CP_MEC_RS64_CNTL), 0, "Linux's unhalt writes 0x3c000000")?;
    refused(&mut m, b(gc12::CP_ME_CNTL), 0x1500_A001, "Linux changes only the pipes' reset bits here")?;
    // The hub: context 0 before its base and range, an invalidation before context 0, another request.
    refused(&mut m, b(GC.context0_cntl), 0x03FF_FC01, "GFXHUB's context 0 enabled before its page table base and range were written")?;
    refused(&mut m, eng(GC.eng0_req), hub41::invalidate_req(0), "an invalidation request on GFXHUB before its context 0 is on")?;
    m.refused.clear();
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::GcHub }, &c, m, &mut Stick::full());
    kassert!(d.pass && m.refused.is_empty(), "{} {:?}", d.line, m.refused);
    refused(&mut m, eng(GC.eng0_req), 0x00F8_0002, "Linux flushes VMID 0 with 0xf80001")?;
    refused(&mut m, eng(GC.eng0_sem), 0, "a register G2 does not know")?;
    Ok(())
}
ktest::ktest!("gx.rdna4.gc.refusals", Safe, refusals);

fn faults() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (_, n) = names(&r)?;
    let mut s = Session::default();
    let mut m = up(&c, &mut s, model(), &[Stage::GcBoot, Stage::GcHub])?;
    let ih = s.nb.ih.as_mut().ok_or("the IH is not on")?;
    let sys = ih.place.sys;
    // GFXHUB's fault comes through ring 1 with the IH's own entry on ring 0; MMHUB's through ring 0.
    let gfx = st::CID.put(0, 13) | st::RW.put(0, 1) | st::PERMISSION_FAULTS.put(0, 2);
    if let Err(e) = m.raise_fault(Kind::Gc, 0x0000_8000_1234_5000, gfx, true) {
        return ktest::fail(&e);
    }
    let passes = ih.reader.process(&mut ModelIo { m: &mut m, sys });
    let f = ih.reader.faults.last.ok_or("the fault was not taken")?;
    kassert!(passes[0].entries == 1 && passes[1].entries == 1 && ih.reader.faults.count == [1, 0] && ih.reader.unclaimed.is_empty(), "{}", ih.reader.describe());
    kassert!(f.addr == 0x0000_8000_1234_5000 && f.client() == "SDMA0" && f.status_write() && f.hub == Kind::Gc, "{f}");
    kassert!(m.gc.fault_reads == [2, 0] && m.gc.faults_cleared == [1, 0] && m.faults_open() == [false, false], "reads {:?}, cleared {:?}", m.gc.fault_reads, m.gc.faults_cleared);
    if let Err(e) = m.raise_fault(Kind::Mm, 0x4000, st::CID.put(0, 19) | st::MAPPING_ERROR.put(0, 1), false) {
        return ktest::fail(&e);
    }
    ih.reader.process(&mut ModelIo { m: &mut m, sys });
    let f = ih.reader.faults.last.ok_or("the fault was not taken")?;
    kassert!(ih.reader.faults.count == [1, 1] && f.client() == "VCNU" && f.hub == Kind::Mm && m.gc.fault_reads == [2, 1] && m.faults_open() == [false, false], "{}", ih.reader.describe());
    // Past Linux's ten in five seconds every fault is still read, cleared and counted.
    for i in 0..20u64 {
        if let Err(e) = m.raise_fault(Kind::Gc, i << 12, gfx, false) {
            return ktest::fail(&e);
        }
        ih.reader.process(&mut ModelIo { m: &mut m, sys });
    }
    kassert!(ih.reader.faults.count == [21, 1] && ih.reader.faults.rate.missed_total == 12 && m.gc.faults_cleared == [21, 1] && m.faults_open() == [false, false], "{}", ih.reader.describe());
    kassert!(m.refused.is_empty(), "{:?}", m.refused);
    // A bind after gchub flushes both hubs, GFXHUB first, and GFXHUB's walker sees the page only after its own
    // invalidation.
    let f = s.flush_regs(&n).ok_or("no flush registers")?;
    let mm_only = gart12::FlushRegs::of(&n, s.nb.hdp_flush(n.hdp_flush)).ok_or("no flush registers")?;
    let l = s.layout.clone().ok_or("no layout")?;
    let gart = s.nb.gart.as_mut().ok_or("no GART")?;
    let mut card = Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes };
    let page = crate::rdna4::fakenb::SYS_BASE + (80 << 20);
    let mut out = Vec::new();
    let mc = gart.bind(&mut card, "one page", &[page]).map_err(|_| "the bind failed")?;
    kassert!(gart12::invalidate_tlb(&mut card, &mm_only, &mut out).is_ok() && card.hw.gc_walk(mc).is_none(), "GFXHUB saw a page MMHUB's flush alone made visible");
    let (gc0, mm0, at) = (card.hw.gc.tlb_flushes, card.hw.nb.tlb_flushes, card.hw.writes.len());
    kassert!(gart12::invalidate_tlb(&mut card, &f, &mut out).is_ok() && card.hw.gc_walk(mc) == Some(page), "GFXHUB does not see the page after both flushes");
    let (gc_req, mm_req) = (f.gc.map(|g| g.0), Some(f.req));
    let order: Vec<bool> = card.hw.writes[at..].iter().filter(|w| Some(w.0) == gc_req || Some(w.0) == mm_req).map(|w| Some(w.0) == gc_req).collect();
    kassert!(order == [true, false] && card.hw.gc.tlb_flushes == gc0 + 1 && card.hw.nb.tlb_flushes == mm0 + 1 && out.is_empty(), "the requests' order (GFXHUB first): {order:?}");
    kassert!(gart.unmap(&mut card, &f, mc, &mut out).is_ok() && card.hw.gc_walk(mc).is_none() && card.hw.gc.tlb_flushes == gc0 + 2, "the unbind did not flush GFXHUB");
    kassert!(card.hw.refused.is_empty(), "{:?}", card.hw.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.gc.faults", Safe, faults);

/// A card whose autoload takes four rounds still equals Linux's section (the declared pair poll); a ring's
/// destroy leaves the block and its hub; a second `fw` switches GFXHUB off before its first image and voids
/// both stages; the model takes no firmware reload over a hub that is on.
fn again() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (regs, n) = names(&r)?;
    let run = |s: &mut Session, m: Model, st: Stage| stage::run(s, Opts { stage: st }, &c, m, &mut Stick::full());
    let mut s = Session::default();
    let mut m = up(&c, &mut s, model(), &[])?;
    m.gc.not_yet = 3;
    let (d, mut m) = run(&mut s, m, Stage::GcBoot);
    kassert!(d.pass && s.gc.boot.is_some_and(|b| b.rounds == 4) && d.lines.last().is_some_and(|l| l == "linuxseq gc-boot: equal"), "{} {:?}", d.line, d.lines.last());
    if let Err(e) = gcstage::equal(&d, &regs, None) {
        return ktest::fail(&format!("gcboot after four rounds is not Linux's sequence: {e}"));
    }
    for st in [Stage::GcHub, Stage::Destroy, Stage::Ring, Stage::Toc, Stage::Tmr] {
        let (d, back) = run(&mut s, m, st);
        m = back;
        kassert!(d.pass, "{st:?}: {}", d.line);
    }
    kassert!(s.gc.hub && s.gc.boot.is_some() && m.gc.context0 && s.flush_regs(&n).is_some_and(|f| f.gc.is_some()), "the destroy took the block or its hub");
    // The second `fw`: the switch-off comes first, then both stages are void.
    let at = m.writes.len();
    let (d, mut m) = run(&mut s, m, Stage::Fw);
    let first = n.byte(GC.context0_cntl).ok_or("a register has no place")?;
    kassert!(d.pass && m.writes.get(at) == Some(&(first, 0)) && !m.gc.context0, "fw did not switch GFXHUB off first: {} {:x?}", d.line, m.writes.get(at));
    kassert!(!s.gc.hub && s.gc.boot.is_none() && s.next() == Some(Stage::Autoload) && s.flush_regs(&n).is_some_and(|f| f.gc.is_none()), "the stages are not void: next {:?}", s.next());
    // The SMU's firmware was loaded anew: the power controller's start is asked again too, and until it passed
    // the block has no power on the card or in the session.
    kassert!(!s.passed(Stage::Pmfw) && !s.pmfw.gc_powered() && !m.gc_powered(), "the second fw left the power controller's start standing");
    // The chain to its end again, a third ring, and a session that forgot its hub: the model takes no firmware
    // over a GFXHUB that is on.
    for st in [Stage::Autoload, Stage::FwRest, Stage::Ta, Stage::Pmfw, Stage::GcBoot, Stage::GcHub, Stage::Destroy, Stage::Ring, Stage::Toc, Stage::Tmr] {
        let (d, back) = run(&mut s, m, st);
        m = back;
        kassert!(d.pass && m.refused.is_empty(), "{st:?} again: {} {:?}", d.line, m.refused);
    }
    kassert!(s.gc.hub && m.gc.context0, "the hub is not on again");
    s.gc = gcstage::State::new();
    let (d, m) = run(&mut s, m, Stage::Fw);
    kassert!(!d.pass && m.refused.len() == 1 && m.refused[0].starts_with("LOAD_IP_FW of the SMU's firmware while GFXHUB's context 0 is on"), "{} {:?}", d.line, m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.gc.again", Safe, again);

/// The model's registers as the Reload hook reaches them.
struct ModelOff<'a>(&'a mut Model);

impl OffIo for ModelOff<'_> {
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

fn reload() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let (_, n) = names(&r)?;
    // While the hub is off, whatever else runs: not one write.
    let mut s = Session::default();
    let mut m = up(&c, &mut s, model(), &[Stage::GcBoot])?;
    let writes = m.writes.len();
    kassert!(gchuboff::gart_disable(&mut ModelOff(&mut m), &n, &GC) == Off::NotOn && m.writes.len() == writes, "a Reload wrote to a hub that is off");
    kassert!(gchuboff::gart_disable(&mut ModelOff(&mut model()), &n, &GC) == Off::NotOn, "a Reload wrote to a block without power");
    // On: Linux's gfxhub_v12_0_gart_disable, in its order.
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::GcHub }, &c, m, &mut Stick::full());
    kassert!(d.pass, "{}", d.line);
    let writes = m.writes.len();
    kassert!(gchuboff::gart_disable(&mut ModelOff(&mut m), &n, &GC) == Off::Done, "the hub was not switched off");
    let b = |x| n.byte(x).unwrap_or(u32::MAX);
    let mut want: Vec<(u32, u32)> = (0..16).map(|i| (b(member(GC.context0_cntl, i, 1)), 0)).collect();
    want.extend([(b(GC.l1_tlb), 0x0000_1818), (b(GC.l2_cntl), 0x0008_0E00), (b(GC.l2_cntl3), 0)]);
    kassert!(m.writes[writes..] == want[..] && !m.gc.context0 && m.refused.is_empty(), "{:x?} {:?}", &m.writes[writes..], m.refused);
    let writes = m.writes.len();
    kassert!(gchuboff::gart_disable(&mut ModelOff(&mut m), &n, &GC) == Off::NotOn && m.writes.len() == writes, "a second Reload wrote");
    Ok(())
}
ktest::ktest!("gx.rdna4.gc.reload", Safe, reload);
