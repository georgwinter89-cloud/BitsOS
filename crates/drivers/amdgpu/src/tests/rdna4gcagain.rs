//! Wave 43, item HB: what holds the two stages' edges. A ring's destroy leaves the graphics block and its hub, a
//! second `fw` in one boot switches GFXHUB off first (Linux's fini before its resume) and only then voids both
//! stages, and the model refuses a firmware reload over a hub that is on; the autoload's declared pair poll
//! (`Rule::PairPoll`) folds only rounds that are the loop's own; `gchub`'s three addresses must be exactly the
//! ones MMHUB was told; and the stops and answers no other test reaches.

use super::rdna4fix::*;
use super::rdna4gc::loaded;
use super::rdna4nb::regs;
use super::rdna4nbmodel::n;
use crate::gpucore::psp::{self, Hw, LINUX_POLLS};
use crate::gpucore::trail::Access;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::host::Host;
use crate::rdna4::linuxseq::compare::{Compare, Kind};
use crate::rdna4::linuxseq::fold::Did;
use crate::rdna4::linuxseq::rules::{PairPoll, Rule};
use crate::rdna4::linuxseq::{self, What};
use crate::rdna4::stage::{Session, Stage};
use crate::rdna4::chainseq::{self, Chosen};
use crate::rdna4::gcstage;
use rdna4_core::gc12;
use rdna4_core::hub41::GC;
use rdna4_core::nbreg::member;
use std::vec::Vec;

/// The model behind a card that answers one register with a fixed value.
struct Fixed {
    m: Model,
    byte: u32,
    value: u32,
}

impl Hw for Fixed {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        let v = self.m.reg_read(byte);
        if byte == self.byte { Some(self.value) } else { v }
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.m.reg_write(byte, value)
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

impl Host for Fixed {}

/// The whole chain, the ring's destroy, and a second ring up to the stage before `fw`.
fn second_ring() -> (Rig, Session, Model) {
    let (rig, mut s, m) = loaded();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot, Stage::GcHub]);
    let n = n(&rig);
    // The destroy leaves the block and its hub: they are the card's state, not the ring's.
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy]);
    assert!(s.gc.boot.is_some() && s.gc.hub && s.passed(Stage::GcBoot) && s.passed(Stage::GcHub) && m.gc.context0);
    assert!(s.flush_regs(&n).unwrap().gc.is_some(), "a bind after the destroy still flushes both hubs");
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Ring, Stage::Toc, Stage::Tmr]);
    assert!(s.gc.hub && m.gc.context0 && s.next() == Some(Stage::Fw));
    (rig, s, m)
}

#[test]
fn a_second_fw_switches_gfxhub_off_first_and_voids_both_stages() {
    let (rig, mut s, mut m) = second_ring();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    m.writes.clear();
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Fw]);
    // gfxhub_v12_0_gart_disable (gfxhub_v12_0.c:388-409) before the first image's frame.
    let mut want: Vec<(u32, u32)> = (0..16).map(|i| (b(member(GC.context0_cntl, i, 1)), 0)).collect();
    want.extend([(b(GC.l1_tlb), 0x0000_1818), (b(GC.l2_cntl), 0x0008_0E00), (b(GC.l2_cntl3), 0)]);
    assert_eq!(m.writes[..want.len()], want[..]);
    assert!(d[0].lines.iter().any(|l| l.starts_with("fw: the graphics block starts anew: GFXHUB switched off first")), "{:?}", d[0].lines);
    // Void from here: the chain asks for the autoload and both stages again, and a bind flushes MMHUB alone.
    assert!(!m.gc.context0 && !s.gc.hub && s.gc.boot.is_none() && !s.passed(Stage::GcBoot) && !s.passed(Stage::GcHub));
    assert!(s.next() == Some(Stage::Autoload) && s.flush_regs(&n).unwrap().gc.is_none());
    // The SMU's firmware was loaded anew, so the power controller is a new one too: its start is asked again
    // (`pmfw::State::reloaded`), and until it passed the block has no power on either side.
    assert!(!s.passed(Stage::Pmfw) && !s.pmfw.gc_powered() && !m.gc_powered() && !m.gc_awake());
    let (d, m) = rig.run(&mut s, Stage::GcHub, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.lines.last().unwrap().contains("gchub needs the gcboot stage of this boot"), "{:?}", d.lines);
    // Without the power controller's second start the graphics block's stage is refused before any access.
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Autoload, Stage::FwRest, Stage::Ta]);
    let (d, m) = rig.run(&mut s, Stage::GcBoot, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.lines.last().unwrap().contains("gcboot needs the pmfw stage of this boot"), "{:?}", d.lines);
    // The chain to the hub again: the power controller's start and both stages run anew, each Linux's section
    // on a block the autoload restarted.
    let (again, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Pmfw, Stage::GcBoot, Stage::GcHub]);
    assert_eq!(again[0].lines.last().unwrap(), "linuxseq smu: equal");
    assert_eq!(again[1].lines.last().unwrap(), "linuxseq gc-boot: equal");
    assert_eq!(again[2].lines.last().unwrap(), "linuxseq gc-hub: equal");
    assert!(s.next() == Stage::CHAIN.iter().skip_while(|x| **x != Stage::GcHub).nth(1).copied() && m.gc_awake() && m.gc.context0 && s.flush_regs(&n).unwrap().gc.is_some() && m.gc.pulses == [2, 2, 2], "{:?}", m.gc.pulses);
}

/// A second `fw` that stops at its first image (the SMU's firmware) has switched GFXHUB off and voided both
/// stages already, while the old PMFW's start still stands in its record. That start must not carry the chain
/// to `gcboot` on a firmware load that stopped half-way.
#[test]
fn a_second_fw_that_stops_at_its_first_image_makes_the_power_controllers_start_due_again() {
    for kind in ["status", "hang"] {
        let (rig, mut s, mut m) = second_ring();
        match kind {
            "status" => m.expect.images[0].1[0] ^= 0xFF,
            _ => m.hang_on = Some(psp::cmd::LOAD_IP_FW),
        }
        let (d, mut m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
        assert!(!d.pass && s.loaded.is_empty() && !s.fw_done && s.gc.boot.is_none() && !s.gc.hub && !m.gc.context0, "{kind}: {}", d.line);
        // The old PMFW's record is kept (it may still run); the chain no longer stands on it.
        assert!(s.pmfw.passed && s.pmfw.gc_powered() && s.pmfw.reload_began && !s.passed(Stage::Pmfw), "{kind}: {}", s.pmfw.describe());
        assert!(s.pmfw.describe().starts_with("started, but the stage fw began to load its firmware anew"), "{kind}: {}", s.pmfw.describe());
        m.writes.clear();
        m.refused.clear();
        let (d, mut m) = rig.run(&mut s, Stage::GcBoot, m, &mut Stick::full());
        assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && m.refused.is_empty(), "{kind}: {} {:?}", d.line, d.trail);
        assert!(d.line.contains("zuerst die Energieverwaltung der Karte starten") && d.lines.last().unwrap().contains("gcboot needs the pmfw stage of this boot"), "{kind}: {:?}", d.lines);
        if kind == "status" {
            // The same ring's `fw` with the image the card expects comes through: a new PMFW, its start asked
            // again, and the chain goes on to the hub as after any second `fw`.
            m.expect.images[0].1[0] ^= 0xFF;
            let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Fw]);
            assert!(!s.pmfw.passed && !s.pmfw.reload_began && !s.pmfw.gc_powered() && s.next() == Some(Stage::Autoload));
            let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Autoload, Stage::FwRest, Stage::Ta, Stage::Pmfw, Stage::GcBoot, Stage::GcHub]);
            assert!(s.passed(Stage::Pmfw) && s.next() == Stage::CHAIN.iter().skip_while(|x| **x != Stage::GcHub).nth(1).copied() && m.gc_awake() && m.gc.context0);
        }
    }
}

#[test]
fn no_firmware_is_loaded_anew_over_a_gfxhub_that_is_on() {
    // The model: a session that forgot its hub sends the SMU's firmware over a running GFXHUB, which Linux never
    // does (its fini switched the hub off, gfx_v12_0.c:3917).
    let (rig, mut s, m) = second_ring();
    s.gc = gcstage::State::new();
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(!d.pass && m.refused.len() == 1 && m.refused[0].starts_with("LOAD_IP_FW of the SMU's firmware while GFXHUB's context 0 is on"), "{} {:?}", d.line, m.refused);
    // The stage: a hub that does not answer its switch-off stops `fw` before any image goes out, and what the
    // two stages did stands.
    let (rig, mut s, m) = second_ring();
    let byte = n(&rig).byte(GC.context0_cntl).unwrap();
    let (d, card) = rig.run_on(&mut s, Stage::Fw, Fixed { m, byte, value: u32::MAX }, &mut Stick::full());
    assert!(!d.pass && d.line.contains("angehalten: der Speicher des Grafikteils lässt sich nicht abschalten"), "{}", d.line);
    assert!(d.totals.writes == 0 && s.loaded.is_empty() && s.gc.hub && s.gc.boot.is_some() && card.m.refused.is_empty() && card.m.gc.context0, "{:?}", d.lines);
    // A hub that reads off although the session says on (the block lost it by itself): nothing is written, the
    // stages are void all the same.
    let (rig, mut s, mut m) = second_ring();
    m.writes.clear();
    let (d, card) = rig.run_on(&mut s, Stage::Fw, Fixed { m, byte, value: 0x03FF_FC00 }, &mut Stick::full());
    assert!(d.lines.iter().any(|l| l.contains("GFXHUB's context 0 was off already")) && !s.gc.hub && s.gc.boot.is_none(), "{:?}", d.lines);
    assert!(!card.m.writes.iter().any(|w| w.0 == byte), "the switch-off wrote to a hub that reads off");
}

/// gc-boot's lines as accesses: Linux's own trail.
fn linux_did(sec: &linuxseq::Section) -> Vec<Did> {
    sec.lines
        .iter()
        .map(|l| match l.what {
            What::Read { dword, value, .. } => Did::Read { dword, value: Some(value) },
            What::Write { dword, value, .. } => Did::Write { dword, value, reached: true },
            other => panic!("gc-boot holds only reads and writes: {other:?}"),
        })
        .collect()
}

#[test]
fn the_pair_poll_folds_only_rounds_that_are_the_loops_own() {
    let sec = linuxseq::section("gc-boot").unwrap();
    let rules = gcstage::linux_rules("gc-boot", None).unwrap();
    let [Rule::PairPoll(p)] = &rules[..] else { panic!("{rules:?}") };
    // The declared end is the stage's own (`gc12::autoload_complete`), and the bound is Linux's.
    for (cp, boot) in [(0, 0x8000_003F), (0, 0x8000_0000), (0, 0x3F), (1, 0x8000_003F), (0x8000_0000, 0), (u32::MAX, u32::MAX), (0, 0)] {
        assert_eq!(p.met(cp, boot), gc12::autoload_complete(cp, boot), "{cp:#x} {boot:#x}");
    }
    assert_eq!((p.max, p.first, p.second), (LINUX_POLLS, "CP_STAT", "RLC_RLCS_BOOTLOAD_STATUS"));
    let linux = linux_did(&sec);
    let (cp, boot) = (linux[0], linux[1]);
    let (Did::Read { dword: d0, .. }, Did::Read { dword: d1, .. }) = (cp, boot) else { panic!() };
    let not_yet = [Did::Read { dword: d0, value: Some(0) }, Did::Read { dword: d1, value: Some(0) }];
    let run = |did: &[Did]| {
        let mut c = Compare::new(&sec, &rules).unwrap();
        c.feed(did).and_then(|_| c.finish())
    };
    let with = |head: &[Did]| [head, &linux[..]].concat();
    // Linux's own run, and the same behind rounds that did not end the loop (all ones included).
    run(&linux).unwrap();
    run(&with(&[not_yet, not_yet, [Did::Read { dword: d0, value: Some(u32::MAX) }, Did::Read { dword: d1, value: Some(u32::MAX) }]].concat())).unwrap();
    // Half a round, or the pair the other way round, is no round: the first access that is not Linux's.
    let e = run(&with(&not_yet[..1])).unwrap_err();
    assert!(e.at == Some((3, 9152)) && e.text.contains("the stage first reads CP_STAT"), "{e}");
    let e = run(&with(&[not_yet[1], not_yet[0]])).unwrap_err();
    assert!(e.at == Some((2, 9151)) && e.kind != Kind::Answer, "{e}");
    // A stage that reads the pair again although the round before ended Linux's loop.
    let e = run(&with(&[cp, boot])).unwrap_err();
    assert!(e.kind == Kind::Condition && e.text.contains("reads the pair again after round 1 answered 0x00000000 and 0x8000003f, which ends Linux's loop"), "{e}");
    // A stage that goes on although its last round did not end the loop (a test of the fake names the answer
    // first; the stage's own line keeps answers and names this).
    let mut early = linux.clone();
    early[1] = not_yet[1];
    assert_eq!(run(&early).unwrap_err().kind, Kind::Condition);
    let mut c = Compare::new(&sec, &rules).unwrap().answers_kept();
    let e = c.feed(&early).unwrap_err();
    assert!(e.kind == Kind::Condition && e.text.contains("the stage goes on after round 1 answered 0x00000000 and 0x00000000, which does not end Linux's loop"), "{e}");
    // A loop that ran out is the card's answer, not the stage's doing: the last round against Linux's lines.
    let mut c = Compare::new(&sec, &rules).unwrap().answers_kept();
    c.feed(&[not_yet, not_yet].concat()).unwrap();
    assert_eq!((c.answers().len(), c.next_line()), (1, Some(9153)));
    // More rounds than Linux makes before it gives up.
    let short = [Rule::PairPoll(PairPoll { max: 2, ..p.clone() })];
    let mut c = Compare::new(&sec, &short).unwrap();
    assert_eq!(c.feed(&with(&[not_yet, not_yet].concat())).unwrap_err().kind, Kind::Bound);
    let mut c = Compare::new(&sec, &short).unwrap();
    c.feed(&with(&not_yet)).unwrap();
    c.finish().unwrap();
    // A pair Linux never reads one after the other in the section is an error of the rule.
    let wrong = [Rule::PairPoll(PairPoll { second: "DB_MEM_CONFIG", ..p.clone() })];
    assert_eq!(Compare::new(&sec, &wrong).err().map(|e| e.kind), Some(Kind::Rule));
    // Without the rule a second round is the first difference, and nothing behind it is compared (what the rule
    // is for).
    let mut c = Compare::new(&sec, &[]).unwrap().answers_kept();
    assert_eq!(c.feed(&with(&not_yet)).unwrap_err().at, Some((4, 9153)));
}

#[test]
fn gfxhub_must_be_told_exactly_what_mmhub_was_told() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (mut done, _) = rig.pass(&mut s, model(), &mut Stick::full(), &Stage::CHAIN);
    let (r, l, n) = (regs(&rig), s.layout.clone().unwrap(), n(&rig));
    let hub = done.iter().position(|d| d.stage == Stage::GcHub).unwrap();
    chainseq::equal(&done, &r, &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_or_else(|e| panic!("{e}"));
    // The stage's own comparison stands on the session's GART: another table, default page or dummy page there
    // and the same accesses are no longer Linux's.
    let gart = s.nb.gart.clone().unwrap();
    gcstage::equal(&done[hub], &r, Some(&gart)).unwrap();
    assert!(gcstage::equal(&done[hub], &r, None).unwrap_err().contains("no GART on record"));
    for (which, name) in [(0, "GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR"), (1, "GCMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR"), (2, "GCVM_L2_PROTECTION_FAULT_DEFAULT_ADDR")] {
        let mut other = gart.clone();
        match which {
            0 => other.table = other.table.start + 0x1000..other.table.end + 0x1000,
            1 => other.scratch = other.scratch.start + 0x1000..other.scratch.end + 0x1000,
            _ => other.dummy += 0x1000,
        }
        let e = gcstage::equal(&done[hub], &r, Some(&other)).unwrap_err();
        assert!(e.contains(name) && e.contains("does not hold") && e.contains("is not inside"), "{which}: {e}");
    }
    // The stage's line: the same, and no comparison without a GART on record.
    let ask = |told: Option<&gcstage::Told>| chainseq::verdict(&mut chainseq::Seen::new(), Stage::GcHub, true, &done[hub].trail, 0, &r, rig.att.vram_bytes, Chosen { l: Some(&l), taken: s.bump.as_ref(), told, irq: &[], mes: None, sdma: None }).unwrap();
    let told = gcstage::Told::of(&gart);
    assert_eq!(ask(Some(&told)), "linuxseq gc-hub: equal");
    assert!(ask(Some(&gcstage::Told { dummy: told.dummy + 0x1000, ..told })).starts_with("linuxseq gc-hub: differs: gc-hub.txt line "));
    assert_eq!(ask(None), "linuxseq gc-hub: not compared: no GART is on record, so what GFXHUB must be told is not known");
    // The whole chain is held to what the bus saw: a GFXHUB table one page beside the one `gmc` named to MMHUB.
    let base = n.byte(GC.ctx0_base_lo).unwrap();
    let w = done[hub].trail.iter_mut().find(|a| matches!(a, Access::Write { byte, .. } if *byte == base)).unwrap();
    let Access::Write { value, .. } = w else { unreachable!() };
    *value += 0x1000;
    let e = chainseq::equal(&done, &r, &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_err();
    assert!(e.starts_with("gc-hub, stage gchub: gc-hub.txt line ") && e.contains("GFXHUB runs on the GART table MMHUB was told") && e.contains("does not hold"), "{e}");
}

#[test]
fn the_stops_and_answers_no_stage_of_the_chain_reaches_by_itself() {
    // A session without its GART (the chain's order makes `gmc` stand before `gchub`, so only a forged one).
    let (rig, mut s, m) = loaded();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot]);
    s.nb.gart = None;
    let writes = m.writes.len();
    let (d, m) = rig.run(&mut s, Stage::GcHub, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.writes.len() == writes && d.lines.last().unwrap().contains("no GART: GFXHUB is told the table gmc made"), "{:?}", d.lines);
    assert!(!d.lines.iter().any(|l| l.starts_with("linuxseq ")), "a call refused before any access has no such line");
    // GB_ADDR_CONFIG reads 0: Linux warns and goes on, so does the stage; the read is the card's answer.
    let (rig, mut s, m) = loaded();
    let byte = n(&rig).byte(gc12::GB_ADDR_CONFIG).unwrap();
    let (d, card) = rig.run_on(&mut s, Stage::GcBoot, Fixed { m, byte, value: 0 }, &mut Stick::full());
    assert!(d.pass && s.gc.boot.is_some_and(|b| b.gb_addr_config == 0) && card.m.gc.pulses == [1, 1, 1], "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.contains("GB_ADDR_CONFIG reads 0: Linux warns (\"Invalid gb_addr_config\") and goes on")), "{:?}", d.lines);
    let line = d.lines.last().unwrap();
    assert!(line.starts_with("linuxseq gc-boot: equal; 1 answers of the card are not the trace's, the first: gc-boot.txt line 6 (init.txt line 9155): Linux reads GB_ADDR_CONFIG"), "{line}");
}
