//! Wave 44 in the kernel: MES's two stages `mes` and `mesq` on the model (`rdna4::fakemes`), under the kernel's
//! own locks, clock and klog, held against the section mes of Linux's trace with their five declared rules;
//! then a command MES answers with a failed status, what the model refuses on its own account, a bind flushed
//! through MES and MES halted as a Reload halts it. QEMU has no RX 9070 XT: the model stands in for it. What
//! MES stands on (the firmware, the power controller, GFXHUB) is the chain's own: its stages run before it.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakemodel::{Model, STAGE_STACK, stack_at};
use crate::rdna4::guard::Mc;
use crate::rdna4::host::Host;
use crate::rdna4::ihoff::OffIo;
use crate::rdna4::linuxseq::compare::Compare;
use crate::rdna4::linuxseq::fold::Bus;
use crate::rdna4::mesoff::{self, Halted, Written};
use crate::rdna4::mesring::Sent;
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::stage::{self, Ctx, Opts, Session, SessionSlot, Stage};
use crate::rdna4::{linuxseq, mesflush, messeq};
use alloc::boxed::Box;
use alloc::vec::Vec;
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::mes12::{self, Pipe};
use rdna4_core::mes12api::{self as api, Misc, op};

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
    let mut stick = Stick::full();
    let at = Stage::CHAIN.iter().position(|st| *st == stage).ok_or("not in the chain")?;
    for st in &Stage::CHAIN[..at] {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, m, &mut stick);
        m = back;
        if !d.pass {
            return Err("the chain before MES did not pass on the model");
        }
    }
    m.writes.clear();
    Ok(m)
}

fn stages() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let mut m = before(&c, &mut s, Stage::Mes)?;
    let top = stack_at();
    let mut done = Vec::new();
    for st in [Stage::Mes, Stage::MesQ] {
        m.deepest = usize::MAX;
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut Stick::full());
        m = back;
        kassert!(d.pass && d.refused.is_empty() && d.trail_lost == 0 && m.refused.is_empty(), "{st:?}: {} {:?} {:?}", d.line, d.lines, m.refused);
        let deep = top.saturating_sub(m.deepest);
        kassert!(deep <= STAGE_STACK, "{st:?} reached the card {deep} bytes deep in the stack, over {STAGE_STACK}");
        done.push(d);
    }
    // Both stages in turn against the section, with the five declared rules.
    let sec = linuxseq::section("mes")?;
    let rules = match messeq::rules(s.mes.kiq) {
        Ok(r) => r,
        Err(why) => return ktest::fail(why),
    };
    let regs = Regs14::from(&r.1.discovery).ok_or("the table does not name the registers")?;
    let bus = Bus::of(&regs);
    let held = Compare::new(&sec, &rules).and_then(|mut cmp| cmp.feed_trail(&done[0].trail, 0, &bus).and_then(|_| cmp.feed_trail(&done[1].trail, 0, &bus)).and_then(|_| cmp.finish()));
    if let Err(e) = held {
        return ktest::fail(&alloc::format!("{e}"));
    }
    kassert!(done[0].lines.last().map(|l| l.as_str()) == Some("linuxseq mes: equal so far (Linux goes on at init.txt line 9554)") && done[1].lines.last().map(|l| l.as_str()) == Some("linuxseq mes: equal"), "{:?} {:?}", done[0].lines.last(), done[1].lines.last());
    // Twelve commands, each answered; MES runs on both pipes with the kernel's five queues on the command processor's model.
    let answered = done.iter().flat_map(|d| d.lines.iter()).filter(|l| l.starts_with("MES(") && l.contains(": answered after ")).count();
    kassert!(answered == 12 && m.mes.runs(Pipe::Sched) && m.mes.runs(Pipe::Kiq) && m.cp.queues.len() == 5 && s.mes.ready() && s.mes.kq, "{answered} answered");
    Ok(())
}
ktest::ktest!("gx.rdna4.mes.stages", Safe, stages);

/// A command MES answers with a failed status stops the stage and names the command and the pipe; one whose
/// answer Linux does not check is said and passed over.
fn stops() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    for (frame, passes, said) in [(4usize, false, "MES(1) failed to respond to msg=ADD_QUEUE"), (8, true, "MES(0) failed to respond to msg=SET_HW_RSRC_1")] {
        let mut s = Box::new(Session::default());
        let mut m = before(&c, &mut s, Stage::Mes)?;
        m.mes.status_zero = Some(frame);
        let (d, m) = stage::run(&mut s, Opts { stage: Stage::Mes }, &c, m, &mut Stick::full());
        kassert!(d.pass == passes && d.lines.iter().any(|l| l.starts_with(said)) && m.refused.is_empty(), "frame {frame}: {} {:?}", d.line, d.lines);
        kassert!(s.mes.tried && s.mes.ready() == passes, "frame {frame}");
    }
    Ok(())
}
ktest::ktest!("gx.rdna4.mes.stops", Safe, stops);

/// What the model's MES refuses on its own account, whatever a driver sends: a kernel queue mapped twice, a
/// misc command on the scheduler pipe, a frame of another size (no status: the firmware cannot place it).
fn model_refusals() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let m = before(&c, &mut s, Stage::Mes)?;
    let (_, m) = stage::run(&mut s, Opts { stage: Stage::Mes }, &c, m, &mut Stick::full());
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::MesQ }, &c, m, &mut Stick::full());
    kassert!(d.pass, "{}", d.line);
    let up = s.mes.up.as_mut().ok_or("MES does not run")?;
    let q0 = crate::rdna4::meskq::KernelQueue::of(up, 0).ok_or("no kernel queue")?.legacy(&up.place);
    let mut short = api::query_status();
    short[0] = api::header(op::QUERY_SCHEDULER_STATUS) & !0xFF000 | (32 << 12);
    let cases = [
        (Pipe::Sched, api::add_legacy(&q0), "mapped already", true),
        (Pipe::Sched, api::misc_frame(Misc::ChangeConfig { limit_single_process: false }), "MISC on the scheduler pipe", true),
        (Pipe::Sched, short, "Linux sends scheduler frames of 64 dwords only", false),
    ];
    for (pipe, frame, want, status) in cases {
        m.refused.clear();
        let mut f = frame;
        let sent = up.ports[pipe.index()].submit(&mut m, &up.place, &mut up.pool, &mut f).map_err(|_| "the frame could not be written")?;
        let failed = match sent {
            Sent::Failed { status: st, .. } => if status { st as u32 == 0 && st >> 63 == 1 } else { st == 0 },
            _ => false,
        };
        kassert!(failed && m.refused.len() == 1 && m.refused[0].contains(want), "{want}: {sent:?} {:?}", m.refused);
    }
    // After all of it MES still answers a command of Linux's.
    m.refused.clear();
    let sent = up.ports[0].submit(&mut m, &up.place, &mut up.pool, &mut api::query_status()).map_err(|_| "the frame could not be written")?;
    kassert!(sent.ok() && m.refused.is_empty(), "{sent:?} {:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.mes.model_refusals", Safe, model_refusals);

/// A bind once MES runs: three HDP flushes on the bus, one WR_WAIT_WR_REG per hub on the KIQ ring, and the
/// card's walker through GFXHUB finds the page only after them.
fn flush() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let n = nb(&r)?;
    let mut s = Box::new(Session::default());
    let m = before(&c, &mut s, Stage::Mes)?;
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Mes }, &c, m, &mut Stick::full());
    kassert!(d.pass, "{}", d.line);
    let (_, mut m) = stage::run(&mut s, Opts { stage: Stage::MesQ }, &c, m, &mut Stick::full());
    let sys = m.pages(4096).ok_or("no page")?;
    let l = s.layout.clone().ok_or("no layout")?;
    let mut card = Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes };
    let (frames, flushes) = (card.hw.mes.frames.len(), card.hw.flushes);
    let f = s.flush_regs(&n).ok_or("no flush registers")?;
    let mut tlb = mesflush::tlb(f, &mut s.mes);
    let mut out = Vec::new();
    let mc = s.nb.gart.as_mut().ok_or("no GART")?.map_with(&mut card, &mut tlb, "test page", &[sys], &mut out).map_err(|_| "the bind failed")?;
    let m = card.hw;
    let misc = m.mes.frames[frames..].iter().filter(|(p, d)| *p == Pipe::Kiq.index() && api::header_fields(d[0]).1 == op::MISC).count();
    kassert!(out.is_empty() && m.flushes - flushes == 3 && misc == 2 && m.gc_walk(mc) == Some(sys) && m.gart_walk(mc) == Some(sys) && m.refused.is_empty(), "{out:?} {} {misc} {:?}", m.flushes - flushes, m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.mes.flush", Safe, flush);

struct Regs<'a>(&'a mut Model, Vec<(u32, u32)>);

impl OffIo for Regs<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.1.push((byte, v));
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, _us: u64) {}
}

/// A Reload halts MES as Linux's `mes_v12_0_enable(adev, false)` does (mes_v12_0.c:791-818), once the stage set
/// out to write it; before that the hook touches nothing.
fn reload() -> ktest::TestResult {
    let r = rig()?;
    let c = ctx(&r);
    let n = nb(&r)?;
    let cntl = n.byte(mes12::CP_MES_CNTL).ok_or("no CP_MES_CNTL")?;
    let mut s = Box::new(Session::default());
    let mut m = before(&c, &mut s, Stage::Mes)?;
    let word = Written::default();
    s.mes.reload = Some(word.clone());
    let mut io = Regs(&mut m, Vec::new());
    let first = mesoff::halt(&mut io, &n, &word);
    kassert!(first == Halted::Untouched && io.1.is_empty(), "the hook touched a block the stage had not written: {first:?}");
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Mes }, &c, m, &mut Stick::full());
    kassert!(d.pass && word.get(), "{}", d.line);
    let (d, mut m) = stage::run(&mut s, Opts { stage: Stage::MesQ }, &c, m, &mut Stick::full());
    kassert!(d.pass, "{}", d.line);
    let mut io = Regs(&mut m, Vec::new());
    let halted = mesoff::halt(&mut io, &n, &word);
    let wrote = core::mem::take(&mut io.1);
    kassert!(halted == Halted::Halted { was: 0x0C00_0000 } && wrote == [(cntl, 0x4003_0010)], "{halted:?} {wrote:x?}");
    kassert!(!m.mes.running[0] && !m.mes.running[1] && m.mes.queue == [None, None] && m.refused.is_empty(), "MES still runs: {:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.mes.reload", Safe, reload);
