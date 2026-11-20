//! Wave 44, item CP: the stage `gccp` on the model (`rdna4::fakecp`). It equals the trace's section gc-cp with
//! its one declared address; it refuses a second run, a run after a stop, a card without GC info and a run out of
//! order before any access; a CP_STAT that never settles is Linux's logged error, after which the stage stops; a second
//! `fw` after it is refused; `gcboot` refuses a card of another revision before any access.

use super::rdna4fix::*;
use super::rdna4gc::chain_before;
use super::rdna4nb::regs;
use crate::gpucore::psp::Hw;
use crate::rdna4::cpseq;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::host::Host;
use crate::rdna4::linuxseq::rules::Rule;
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::{clearstate12, gc12, gc12cp as cp};
use std::string::String;
use std::vec::Vec;

/// The chain up to the command processor: GFXHUB on, the engines' starts set and pulsed.
pub fn hub_up() -> (Rig, Session, Model) {
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcCp));
    m.writes.clear();
    (rig, s, m)
}

/// `hub_up`, then the stage.
pub fn cp_up() -> (Rig, Session, Model) {
    let (rig, mut s, m) = hub_up();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcCp]);
    (rig, s, m)
}

#[test]
fn the_stage_equals_gc_cp_with_its_one_declared_address() {
    let (rig, mut s, m) = hub_up();
    let (d, mut m) = rig.run(&mut s, Stage::GcCp, m, &mut Stick::full());
    assert!(d.pass && d.refused.is_empty() && m.refused.is_empty(), "{}\n{}\n{:?}", d.line, d.lines.join("\n"), m.refused);
    let (l, b) = (s.layout.clone().unwrap(), s.bump.as_ref().unwrap());
    cpseq::equal(&d, &regs(&rig), &l, b).unwrap_or_else(|e| panic!("{e}\n{}", d.lines.join("\n")));
    assert_eq!(d.lines.last().unwrap(), "linuxseq gc-cp: equal");
    // Linux's 82 writes and nothing else; the buffer's 300 bytes into VRAM before its registers.
    assert_eq!((d.totals.writes, m.writes.len()), (82, 82));
    // The one rule: the buffer's address, exactly the manager's page.
    let at = cpseq::placed(&l, b).unwrap();
    assert!(matches!(cpseq::linux_rules(at).as_slice(), [Rule::Address(a)] if a.lo == "RLC_CSIB_ADDR_LO" && a.within == (at..at + 4096)));
    assert_eq!(s.cp.csb.clone().map(|r| l.mc(r.start)), Some(at));
    assert_eq!(m.cp.csb, Some((at, 0x4B)));
    let mut buf = std::vec![0u8; 300];
    m.vram_peek(at - l.mc_base, &mut buf);
    assert_eq!(buf, clearstate12::bytes());
    // The engines run, CE stays halted, the KIQ's place is MES's, the doorbells are routed.
    let n = n_of(&rig);
    let rd = |m: &mut Model, r| m.reg_read(n.byte(r).unwrap()).unwrap();
    assert_eq!((rd(&mut m, gc12::CP_ME_CNTL), rd(&mut m, gc12::CP_MEC_RS64_CNTL), rd(&mut m, cp::RLC_CP_SCHEDULERS)), (0x0100_A000, 0x3C00_0000, 0x30E8));
    assert!(m.cp.mec_on && m.cp.gfx_on && m.cp.entries == [Some(0x3000_0007), Some(0x3000_000D)]);
    assert!(s.passed(Stage::GcCp) && s.next() == Some(Stage::Mes) && s.cp.cp_stat == Some(Ok(20)));
    assert!(s.summary().contains("command processor unhalted (CP_STAT 0)"), "{}", s.summary());
    assert!(d.lines.iter().any(|l| l.starts_with("WAIT ") && l.contains("CP_STAT until (value & 0xffffffff) == 0x00000000: 21 reads")), "{:?}", d.lines);
    // A second run is refused before any access.
    let (d, m) = rig.run(&mut s, Stage::GcCp, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && m.writes.len() == 82 && d.lines.last().unwrap().contains("a second run is refused"), "{:?}", d.lines);
    // The state look names the block's registers once the stage wrote them.
    let (d, _) = rig.run(&mut s, Stage::State, m, &mut Stick::full());
    assert!(d.lines.iter().any(|l| l == "cp registers: CP_STAT 0x0, CP_ME_CNTL 0x100a000, CP_MEC_RS64_CNTL 0x3c000000, RLC_CP_SCHEDULERS 0x30e8, CP_INT_CNTL_RING0 0x0, RLC_CSIB_LENGTH 0x4b"), "{:?}", d.lines);
}

pub fn n_of(rig: &Rig) -> crate::rdna4::nbregs::NbRegs {
    super::rdna4nbmodel::n(rig)
}

/// A card that refuses the `nth` write of one register (as the model would refuse a value).
struct Refuse {
    m: Model,
    byte: u32,
    nth: usize,
    seen: usize,
}

impl Hw for Refuse {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.m.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.seen += usize::from(byte == self.byte);
        !(byte == self.byte && self.seen == self.nth) && self.m.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.m.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.m.mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        self.m.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.m.delay_us(us)
    }
}

impl Host for Refuse {}

#[test]
fn a_stop_half_way_spends_the_stage_for_this_boot() {
    let (rig, mut s, m) = hub_up();
    let byte = n_of(&rig).byte(gc12::CP_ME_CNTL).unwrap();
    let (d, card) = rig.run_on(&mut s, Stage::GcCp, Refuse { m, byte, nth: 1, seen: 0 }, &mut Stick::full());
    let m = card.m;
    assert!(!d.pass && s.cp.tried && !s.cp.passed && m.cp.mec_on && !m.cp.gfx_on, "{}", d.line);
    // The write that did not arrive is the call's difference: Linux's write reached the card.
    assert!(d.lines.iter().any(|l| l.starts_with("linuxseq gc-cp: differs: gc-cp.txt line 134 (init.txt line 9493)")), "{:?}", d.lines);
    assert_eq!(s.next(), Some(Stage::GcCp));
    // Not again in this boot (the engines may run), and nothing after it either.
    let (d, m) = rig.run(&mut s, Stage::GcCp, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.line.contains("erst neu starten"), "{}", d.line);
    let (d, _) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.lines.last().unwrap().contains("sdma needs the gcring stage of this boot"), "{:?}", d.lines);
}

/// A stop inside the constants' loops puts GRBM_GFX_INDEX back on all arrays and GRBM_GFX_CNTL on VMID 0, as
/// Linux's loops end: what is written after them (the Reload hook's halt) stands on Linux's selection.
#[test]
fn a_stop_inside_the_loops_puts_the_selections_back() {
    let rig = Rig::new();
    let n = n_of(&rig);
    for (reg, nth) in [(cp::GRBM_GFX_INDEX, 3), (cp::SH_MEM_BASES, 2)] {
        let (_, mut s, m) = hub_up();
        let byte = n.byte(reg).unwrap();
        let (d, card) = rig.run_on(&mut s, Stage::GcCp, Refuse { m, byte, nth, seen: 0 }, &mut Stick::full());
        let m = card.m;
        assert!(!d.pass && s.cp.tried && !m.cp.mec_on, "{}: {}", reg.name, d.line);
        assert_eq!((m.cp.val(&n, cp::GRBM_GFX_INDEX), m.gc.grbm), (cp::GFX_INDEX_ALL, 0), "{}", reg.name);
        assert!(m.refused.is_empty(), "{:?}", m.refused);
    }
}

#[test]
fn a_cp_stat_that_never_settles_is_linuxs_logged_error_and_the_stage_stops_after_linuxs_accesses() {
    let (rig, mut s, mut m) = hub_up();
    m.cp.stat_busy = u32::MAX;
    let t0 = m.card.now_us;
    let (d, m) = rig.run(&mut s, Stage::GcCp, m, &mut Stick::full());
    assert!(!d.pass && m.refused.is_empty(), "{} {:?}", d.line, m.refused);
    assert_eq!(s.cp.cp_stat, Some(Err(0x8802_8000)));
    assert!(d.lines.iter().any(|l| l.starts_with("gccp: failed to unhalt cp gfx: CP_STAT still 0x88028000 after 100000 reads; Linux logs it and goes on")), "{:?}", d.lines);
    assert!(d.line.contains("kommt nach dem Start nicht zur Ruhe"), "{}", d.line);
    // Linux's bound: 100000 reads 1 us apart, Linux's udelay(50) before them.
    assert!(m.card.now_us - t0 >= 100_050);
    // Linux's accesses whole, the KIQ's place included (a stopped stage's line says "as far as it came"): the
    // wait's last answer is not the trace's 0.
    let seq = d.lines.iter().find(|l| l.starts_with("linuxseq gc-cp: ")).unwrap();
    assert!(seq.starts_with("linuxseq gc-cp: the same as Linux's as far as the stage came; 1 answers of the card are not the trace's"), "{seq}");
    assert!(d.lines.last().unwrap().starts_with("stopped: CP_STAT still 0x88028000"), "{:?}", d.lines);
    assert_eq!(d.totals.writes, 82);
    // Spent for the boot, nothing after it, and the measurement run's stop rule stops there.
    let took = crate::rdna4::messung::Took { d: d.clone(), text: String::new(), lowest: usize::MAX, us: 0 };
    assert!(matches!(crate::rdna4::messung::next(&Ok(took)), crate::rdna4::messung::Next::Stop(..)));
    assert!(s.cp.tried && !s.cp.passed && s.next() == Some(Stage::GcCp));
    assert!(s.summary().contains("CP_STAT still 0x88028000 after Linux's loop: gccp stopped"), "{}", s.summary());
    let (d, _) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty(), "{:?}", d.lines);
}

#[test]
fn what_the_stage_stands_on_is_checked_before_any_access() {
    // No GC info in the card's table: the constants' loops have no bounds, nothing is taken or touched.
    let (mut rig, mut s, m) = hub_up();
    rig.att.discovery.gc = None;
    let taken = s.bump.as_ref().unwrap().taken.len();
    let (d, m) = rig.run(&mut s, Stage::GcCp, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && d.line.contains("beschreibt den Grafikteil nicht"), "{}", d.line);
    assert!(s.bump.as_ref().unwrap().taken.len() == taken && s.cp.csb.is_none() && !s.cp.tried);
    // Out of order: without GFXHUB, the session's one order refuses and names it.
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcHub));
    let (d, m) = rig.run(&mut s, Stage::GcCp, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.line.contains("zuerst den Speicher des Grafikteils einrichten") && d.lines.last().unwrap().contains("gccp needs the gchub stage of this boot"), "{:?}", d.lines);
    assert!(m.refused.is_empty() && s.cp.csb.is_none());
    // The second card is told that it stays untouched.
    let (mut rig, mut s, m) = hub_up();
    rig.display = false;
    let (d, m) = rig.run(&mut s, Stage::GcCp, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && d.line.contains("die zweite Karte bleibt unberührt"), "{}", d.line);
}

#[test]
fn a_second_fw_after_the_command_processor_is_refused_before_any_access() {
    let (rig, mut s, m) = cp_up();
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy, Stage::Ring, Stage::Toc, Stage::Tmr]);
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty(), "{}", d.line);
    assert!(d.line.contains("der Befehlsprozessor der Karte wurde in diesem Start schon gestartet") && d.lines.last().unwrap().contains("gfx_v12_0.c:3914-3915"), "{:?}", d.lines);
    assert!(s.cp.passed && m.cp.gfx_on, "nothing changed");
}

#[test]
fn gcboot_refuses_a_card_of_another_revision_before_any_access() {
    // The attach reads the revision as Linux does (STRAP0 bits 27:24, discovery.txt line 6).
    let (card, _) = crate::rdna4::Card::attach(super::rdna4::facts(&rdna4_core::card::CARD_A), crate::rdna4::fakecard::FakeCard { strap0: 0x300C_7550, ..crate::rdna4::fakecard::FakeCard::measured() });
    let att = card.state.unwrap();
    assert_eq!(att.rev_id, Some(0));
    for rev in [Some(0), Some(2), None] {
        let mut rig = Rig::new();
        rig.att.rev_id = rev;
        let mut s = Session::default();
        let (_, m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcBoot));
        let writes = m.writes.len();
        let (d, m) = rig.run(&mut s, Stage::GcBoot, m, &mut Stick::full());
        assert!(!d.pass && d.trail.is_empty() && m.writes.len() == writes, "{rev:?}: {}", d.line);
        assert!(d.line.contains("eine andere Ausführung") && d.lines.last().unwrap().contains("only the golden list of revision 1 is built"), "{:?}", d.lines);
    }
}

/// The writes of the section the stage makes, in order, as the model saw them: Linux's 82.
#[test]
fn the_writes_are_the_sections_in_its_order() {
    let (rig, _, m) = cp_up();
    let want: Vec<u32> = crate::rdna4::linuxseq::section("gc-cp").unwrap().lines.iter().filter_map(|l| match l.what {
        crate::rdna4::linuxseq::What::Write { dword, .. } => Some(dword * 4),
        _ => None,
    }).collect();
    let got: Vec<u32> = m.writes.iter().map(|(b, _)| *b).collect();
    assert_eq!(got, want);
    let _ = rig;
}
