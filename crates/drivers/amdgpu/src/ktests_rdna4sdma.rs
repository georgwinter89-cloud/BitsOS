//! Wave 43 in the kernel: the copy engines' stage `sdma` on the model (`rdna4::fakesdma`), under the kernel's own
//! locks, clock and klog, held against its section of Linux's trace with its seven declared rules; then the rungs
//! of `sdmatest`, with the trap arriving through the IH reader the stage runs itself; then the stage without the
//! GC block's power, what the stage refuses before any access and what the model refuses on its own account;
//! then the engines stopped as a Reload stops them. QEMU has no RX 9070 XT: the model stands in for
//! it. The GC block's power and its hub are the chain's own: the stages `pmfw`, `gcboot`, `gchub`, `mes` and
//! `mesq` run before `sdma` here as on the card.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakemodel::{Model, STAGE_STACK, stack_at};
use crate::rdna4::host::Host;
use crate::rdna4::ihoff::OffIo;
use crate::rdna4::linuxseq::compare::Compare;
use crate::rdna4::linuxseq::fold::Bus;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::sdmaoff::{self, OffRegs, Stopped, Written};
use crate::rdna4::sdmaq::lay;
use crate::rdna4::sdmaregs::SdmaRegs;
use crate::rdna4::stage::{self, Ctx, Opts, Session, SessionSlot, Stage};
use crate::rdna4::{linuxseq, sdmaih, sdmarungs, sdmaseq};
use alloc::format;
use alloc::vec::Vec;
use amdgpu_fmt::sdma as pkt;
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::sdma7;

type Rig = (crate::rdna4::attach::PciFacts, crate::rdna4::attach::Attached);

fn rig() -> Result<Rig, &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| (facts, a)).map_err(|_| "the measured card did not attach")
}

fn ctx(r: &Rig) -> Ctx<'_> {
    Ctx { index: 0, display: true, facts: &r.0, att: &r.1, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX }
}

/// The chain on the cold model up to the stage before `sdma` (loaded firmware, the power controller started,
/// the graphics block's start, its hub on, MES running with the kernel's queues): the card as `sdma` meets it.
fn loaded(c: &Ctx, s: &mut Session) -> Result<Model, &'static str> {
    let mut m = model();
    let mut stick = Stick::full();
    let at = Stage::CHAIN.iter().position(|st| *st == Stage::Sdma).ok_or("sdma is not in the chain")?;
    for st in &Stage::CHAIN[..at] {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, m, &mut stick);
        m = back;
        if !d.pass {
            return Err("the chain before the copy engines did not pass on the model");
        }
    }
    m.writes.clear();
    Ok(m)
}

fn stage_equals_linux() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    // The device's own slot type, so the session lives under the lock the live tool and the IH thread take.
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let mut m = loaded(&c, &mut s)?;
    let top = stack_at();
    m.deepest = usize::MAX;
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(d.pass && d.refused.is_empty() && d.trail_lost == 0 && m.refused.is_empty(), "{} {:?} {:?}", d.line, d.lines, m.refused);
    let deep = top.saturating_sub(m.deepest);
    kassert!(deep <= STAGE_STACK, "sdma reached the card {deep} bytes deep in the stack, over {STAGE_STACK}");
    // The stage alone against its section, with the seven declared rules.
    let sec = linuxseq::section("sdma")?;
    let rules = sdmaseq::rules(s.sdma.bound);
    let regs = Regs14::from(&r.1.discovery).ok_or("the table does not name the registers")?;
    let held = Compare::new(&sec, &rules).and_then(|mut cmp| cmp.feed_trail(&d.trail, d.trail_lost, &Bus::of(&regs)).and_then(|_| cmp.finish()));
    if let Err(e) = held {
        return ktest::fail(&format!("sdma is not Linux's sequence: {e}"));
    }
    kassert!(rules.len() == 7 && sec.lines.len() == 68 && d.lines.last().is_some_and(|l| l == "linuxseq sdma: equal"), "{:?}", d.lines.last());
    // Both ring tests ran as Linux's: the doorbells with 16 dwords in bytes, each read pointer written back.
    kassert!(m.sdma.doorbells == [(0x200, 0x40), (0x214, 0x40)] && m.sdma.e.iter().all(|e| e.on && e.rptr == 0x40 && e.halted.is_none()), "{:?}", m.sdma.doorbells);
    let up = s.sdma.up.as_ref().ok_or("the engines are not up")?;
    let mut b = [0u8; 8];
    kassert!(m.sys_read(up.place.slot_sys(up.q[1].slots.rptr), &mut b) && u64::from_le_bytes(b) == 0x40, "engine 1's read pointer slot");
    kassert!(s.passed(Stage::Sdma) && s.next().is_none() && up.ring_test == [1, 1], "{}", s.summary());
    Ok(())
}
ktest::ktest!("gx.rdna4.sdma.stage", Safe, stage_equals_linux);

fn rungs() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let m = loaded(&c, &mut s)?;
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(d.pass, "sdma: {} {:?}", d.line, d.lines);
    let taken = s.bump.as_ref().map_or(0, |b| b.taken.len());
    let bound = s.nb.gart.as_ref().map_or(0, |g| g.bound.len());
    let top = stack_at();
    m.deepest = usize::MAX;
    m.writes.clear();
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::SdmaTest }, &c, m, &mut Stick::full());
    kassert!(d.pass && d.refused.is_empty() && m.refused.is_empty(), "{} {:?} {:?}", d.line, d.lines, m.refused);
    let deep = top.saturating_sub(m.deepest);
    kassert!(deep <= STAGE_STACK, "sdmatest reached the card {deep} bytes deep in the stack, over {STAGE_STACK}");
    kassert!(s.sdma.rungs.len() == sdmarungs::RUNGS.len() && s.sdma.rungs.iter().zip(sdmarungs::RUNGS).all(|(got, name)| *got == (name, true)), "{:?}", s.sdma.rungs);
    // The traps came through the reader the stage ran itself, and nothing is left unhandled.
    let ih = s.nb.ih.as_ref().ok_or("no IH")?;
    kassert!(sdmaih::traps(ih, 0) == 7 && sdmaih::traps(ih, 1) == 2 && ih.reader.unclaimed.is_empty(), "{}", ih.reader.describe());
    // Everything the rungs took is back, and no register of the engines was written.
    kassert!(s.bump.as_ref().map_or(0, |b| b.taken.len()) == taken && s.nb.gart.as_ref().map_or(0, |g| g.bound.len()) == bound && s.sdma.held == (None, None), "the rungs left something taken");
    let sr = SdmaRegs::from(&r.1.discovery).ok_or("no SDMA registers")?;
    kassert!(m.writes.iter().all(|(b, _)| sr.reg_at(*b).is_none()), "sdmatest wrote a register of the engines");
    // The answer ends in the stage's own word on the trace: its two flushes through MES are Linux's, the rungs stand in no section.
    kassert!(d.lines.last().is_some_and(|l| l.starts_with("linuxseq mes: equal: the call's only register accesses are 2 of the TLB flush after a GART change once MES runs") && l.contains("the rungs themselves stand in no section")), "{:?}", d.lines.last());
    Ok(())
}
ktest::ktest!("gx.rdna4.sdma.rungs", Safe, rungs);

/// One submission straight to the model: the words at the write pointer padded to the fetch size, the poll slot,
/// the doorbell.
pub(crate) fn push(m: &mut Model, sys: u64, wptr_slot: u64, i: u32, words: &[u32]) -> bool {
    let at = m.sdma.e[i as usize].wptr;
    let mut all = words.to_vec();
    all.resize(words.len().next_multiple_of(16), pkt::NOP);
    let bytes: Vec<u8> = all.iter().flat_map(|d| d.to_le_bytes()).collect();
    let new = at + bytes.len() as u64;
    m.sys_write(sys + lay::ring(i) + at % u64::from(sdma7::RING_BYTES), &bytes) && m.sys_write(wptr_slot, &new.to_le_bytes()) && m.doorbell_write64(sdma7::doorbell_dw(i), new)
}

/// Without the GC block's power: the stage stops at its first write and is spent for this boot.
fn no_power() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let mut s = Session::default();
    let mut m = loaded(&c, &mut s)?;
    // The card's power controller lost its features behind the driver's back: the model's one power answer.
    m.pmfw.enabled = false;
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.len() == 2 && m.refused.len() == 1 && m.refused[0].contains("while the GC block has no power") && s.sdma.tried, "{} {:?}", d.line, m.refused);
    m.pmfw.enabled = true;
    m.refused.clear();
    m.writes.clear();
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.is_empty() && m.writes.is_empty(), "a second run after a half-way stop touched the card: {}", d.line);
    Ok(())
}
ktest::ktest!("gx.rdna4.sdma.no_power", Safe, no_power);

/// The stage's own checks, before any access: no firmware; the rungs without the engines; no word of the power
/// controller that the GC block has power.
fn refusals() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let sr = SdmaRegs::from(&r.1.discovery).ok_or("no SDMA registers")?;
    let b = |reg, i| sr.byte(reg, i).unwrap_or(u32::MAX);
    let mut s = Session::default();
    let m = loaded(&c, &mut s)?;
    s.autoload = false;
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && m.nb.sys_given.len() == 3, "no autoload: {}", d.line);
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::SdmaTest }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && s.sdma.held == (None, None), "sdmatest without sdma: {}", d.line);
    // The power controller's features are not on by this boot's record: nothing is read or written although
    // the order allows the stage.
    (s.autoload, s.pmfw.facts.enabled) = (true, false);
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && !s.sdma.tried, "no GC power on record: {}", d.line);
    // None of these stops counts as half-way: the same session's stage then passes.
    s.pmfw.facts.enabled = true;
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(d.pass && m.refused.is_empty() && m.writes.iter().all(|(byte, _)| *byte != b(sdma7::CNTL, 0)), "sdma: {} {:?}", d.line, m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.sdma.refusals", Safe, refusals);

/// What the model's engines refuse on their own account, and how a stopped engine ends the rungs.
fn model_refusals() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let sr = SdmaRegs::from(&r.1.discovery).ok_or("no SDMA registers")?;
    let b = |reg, i| sr.byte(reg, i).unwrap_or(u32::MAX);
    let mut s = Session::default();
    let mut m = loaded(&c, &mut s)?;
    // Before the stage: no ring without its pointers and base, no doorbell.
    kassert!(!m.reg_write(b(sdma7::RB_CNTL, 0), 0x0084_1817) && !m.doorbell_write64(0x200, 0x40), "a ring or a doorbell passed on a card nobody set up");
    kassert!(m.refused.len() == 2 && m.refused[0].contains("before its base, its size and its four pointers") && m.refused[1].contains("before the aperture and NBIF's doorbell entry"), "{:?}", m.refused);
    m.refused.clear();
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(d.pass && m.refused.is_empty(), "sdma: {} {:?}", d.line, m.refused);
    let up = s.sdma.up.as_ref().ok_or("the engines are not up")?;
    let (sys, slot) = (up.place.sys, [up.place.slot_sys(up.q[0].slots.wptr), up.place.slot_sys(up.q[1].slots.wptr)]);
    // A doorbell that is not Linux's: another dword, a value the poll slot does not hold; a pointer on a running ring.
    kassert!(!m.doorbell_write64(0x202, 0x80) && !m.doorbell_write64(0x200, 0x80) && !m.reg_write(b(sdma7::RB_WPTR, 0), 0), "a wrong doorbell or a pointer write passed");
    kassert!(m.refused.len() == 3 && m.refused[1].contains("poll slot holds") && m.refused[2].contains("while the ring runs"), "{:?}", m.refused);
    m.refused.clear();
    // A packet Linux does not send (6.0's copy, seven dwords, no CPV) stops engine 1 at it.
    let mut old = Vec::new();
    if pkt::copy_linear(&mut old, up.place.ib_mc(), up.place.ib_mc() + 64, 64, false).is_err() {
        return ktest::fail("the packet did not encode");
    }
    kassert!(push(&mut m, sys, slot[1], 1, &old), "the doorbell: {:?}", m.refused);
    kassert!(m.sdma.e[1].halted.is_some() && m.sdma.e[1].rptr == 0x40 && m.reg_read(b(sdma7::STATUS_REG, 1)) == Some(0) && m.refused.len() == 1, "{:?}", m.refused);
    // An address with no entry (the test pages are not bound) is a fault raised to the hub, and stops engine 0.
    let mut w = Vec::new();
    if pkt::write_linear(&mut w, up.place.mc + lay::TEST, &[1]).is_err() {
        return ktest::fail("the packet did not encode");
    }
    kassert!(push(&mut m, sys, slot[0], 0, &w) && m.sdma.hub_faults == [(up.place.mc + lay::TEST, true)] && m.sdma.e[0].halted.is_some(), "{:?}", m.sdma.hub_faults);
    // With an engine stopped the rungs end at its first fence, bounded, and keep what they took.
    let (d, _) = stage::run(&mut s, Opts { stage: Stage::SdmaTest }, &c, m, &mut Stick::full());
    kassert!(!d.pass && s.sdma.rungs.is_empty() && s.sdma.held.0.is_some() && d.lines.last().is_some_and(|l| l.contains("engine 0: fence 1 did not come within 10000 polls")), "{} {:?}", d.line, d.lines.last());
    Ok(())
}
ktest::ktest!("gx.rdna4.sdma.model_refusals", Safe, model_refusals);

/// The Reload hook's side of the card: the writes it made, and how many reads.
struct Regs<'a>(&'a mut Model, Vec<(u32, u32)>, u32);

impl OffIo for Regs<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.2 += 1;
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.1.push((byte, v));
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, _us: u64) {}
}

/// A Reload: nothing read or written until the stage sets out to write the block; from then on the engines
/// stopped as Linux's `sdma_v7_0_hw_fini` stops them.
fn reload() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let sr = SdmaRegs::from(&r.1.discovery).ok_or("no SDMA registers")?;
    let off = OffRegs::of(&sr).ok_or("no stop registers")?;
    let mut s = Session::default();
    let mut m = loaded(&c, &mut s)?;
    let hook = Written::default();
    s.sdma.reload = Some(hook.clone());
    let mut io = Regs(&mut m, Vec::new(), 0);
    kassert!(sdmaoff::stop(&mut io, &off, &hook) == Stopped::Untouched && io.1.is_empty() && io.2 == 0, "a card whose engines the stage never wrote was touched: {} reads, {:x?}", io.2, io.1);
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::Sdma }, &c, m, &mut Stick::full());
    kassert!(d.pass && hook.get(), "sdma: {}", d.line);
    let mut io = Regs(&mut m, Vec::new(), 0);
    kassert!(sdmaoff::stop(&mut io, &off, &hook) == Stopped::Halted && io.1.len() == 6, "{:x?}", io.1);
    let b = |reg, i| sr.byte(reg, i).unwrap_or(u32::MAX);
    kassert!(io.1[0] == (b(sdma7::RB_CNTL, 0), 0x0084_1816) && io.1[1] == (b(sdma7::IB_CNTL, 0), 0x100) && io.1[4] == (b(sdma7::MCU_CNTL, 0), 1) && io.1[5] == (b(sdma7::MCU_CNTL, 1), 1), "{:x?}", io.1);
    kassert!(m.refused.is_empty() && !m.sdma.any_on() && !m.doorbell_write64(0x200, 0x80), "{:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.sdma.reload", Safe, reload);
