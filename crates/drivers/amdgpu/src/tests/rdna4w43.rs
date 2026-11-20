//! Wave 43 as a whole: what holds BETWEEN its three items on the model, each rule beside the step the model takes.
//! Linux's order after the loaded firmware is the power controller (`pmfw`), the graphics block's start
//! (`gcboot`), its hub (`gchub`) and the copy engines (`sdma`, with the rungs of `sdmatest` behind them):
//!
//! - one run from a cold card through the rungs equals every covered section of Linux's trace in one go;
//! - no graphics register (GFXHUB, the engines' program counters, SDMA) and no doorbell takes a write before the
//!   power controller's features are on AND the autoload completed, and no stage reaches the card before the
//!   one before it passed;
//! - the copy engines' addresses go through GFXHUB as `gchub` built it, and a bad one arrives at the fault
//!   decode in plain words, also when the stage reads the IH rings itself;
//! - every GART bind and unbind flushes both hubs once GFXHUB is on, and one flushed on MMHUB alone is not seen
//!   by the engines;
//! - the VRAM manager's reservations hold across all items and everything the rungs took is given back;
//! - a second `fw` in one boot is refused once the engines ran.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use super::rdna4nbmodel::n;
use super::rdna4sdma::{chain_before, sr, up};
use super::rdna4sdmafake::{push, took};
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::chainseq;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::sdmaq::lay;
use crate::rdna4::sdmarungs::{self, RUNGS};
use crate::rdna4::stage::{Session, Stage};
use crate::rdna4::vram::Kept;
use amdgpu_fmt::sdma as pkt;
use core::ops::Range;
use rdna4_core::hub41::{self, GC};
use rdna4_core::nbreg::member;
use rdna4_core::{gc12, sdma7};
use std::vec::Vec;

/// Wave 43's four chain stages and wave 44's command processor, in Linux's order behind `ta`.
const WAVE: [Stage; 5] = [Stage::Pmfw, Stage::GcBoot, Stage::GcHub, Stage::GcCp, Stage::Sdma];

fn overlap(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

/// One write of each kind of graphics register this wave's stages make, with a value the awake block takes: the
/// pipe's selection before a program counter, GFXHUB's table base, an engine's halt.
fn gc_writes(rig: &Rig, n: &NbRegs) -> [(&'static str, u32, u32); 3] {
    let b = |r| n.byte(r).unwrap();
    [("the pipe selection before a program counter", b(gc12::GRBM_GFX_CNTL), 0), ("GFXHUB's table base", b(GC.ctx0_base_hi), 0), ("an engine's halt", sr(rig).byte(sdma7::MCU_CNTL, 0).unwrap(), 1)]
}

#[test]
fn one_run_from_a_cold_card_through_the_rungs_equals_linux_in_one_go() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (done, m) = rig.pass(&mut s, model(), &mut Stick::full(), &Stage::CHAIN);
    let wave: Vec<Stage> = Stage::CHAIN.iter().copied().filter(|st| WAVE.contains(st)).collect();
    assert_eq!((wave.as_slice(), Stage::CHAIN.iter().position(|st| *st == WAVE[0])), (&WAVE[..], Some(11)), "the wave's stages stand in Linux's order behind ta (since wave 44 MES's two and the rings between gccp and sdma)");
    let (taken, bound) = (s.bump.as_ref().unwrap().taken.clone(), s.nb.gart.as_ref().unwrap().bound.clone());
    let (rungs, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(rungs.pass && rungs.refused.is_empty() && m.refused.is_empty(), "{}\n{}\n{:?}", rungs.line, rungs.lines.join("\n"), m.refused);
    // Every covered section, each whole, in Linux's order: wave 42's five and this wave's four.
    let l = s.layout.clone().unwrap();
    let compared = chainseq::equal(&done, &regs(&rig), &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_or_else(|e| panic!("{e}"));
    let lines: Vec<(&str, usize)> = compared.iter().map(|(name, lines, _)| (*name, *lines)).collect();
    assert_eq!(lines, [("nbif", 8), ("mmhub", 175), ("ih", 47), ("psp-boot", 47), ("psp-ring", 100), ("smu", 741), ("gc-boot", 41), ("gc-hub", 168), ("gc-cp", 135), ("mes", 64), ("gc-ring", 26), ("sdma", 68)]);
    // Each stage said so itself, and the rungs' call holds what the trace has of it: the two flushes through MES.
    for (d, section) in done.iter().filter(|d| WAVE.contains(&d.stage)).zip(["smu", "gc-boot", "gc-hub", "gc-cp", "sdma"]) {
        assert_eq!(d.lines.last().unwrap(), &std::format!("linuxseq {section}: equal"), "{:?}", d.stage);
    }
    let last = rungs.lines.last().unwrap();
    assert!(last.starts_with("linuxseq mes: equal: the call's only register accesses are 2 of the TLB flush after a GART change once MES runs"), "{last}");
    assert_eq!(s.sdma.rungs, RUNGS.map(|name| (name, true)));
    // The card at the end: the features on, the block awake, both hubs on the one table, both engines idle, no
    // fault raised, nothing left unread in the IH.
    assert!(m.gc_powered() && m.gc_awake() && m.gc.context0 && m.nb.context0 && m.sdma.e.iter().all(|e| e.on && e.halted.is_none() && e.rptr == e.wptr));
    let ih = s.nb.ih.as_ref().unwrap();
    assert!(m.faults_open() == [false, false] && ih.reader.faults.count == [0, 0] && ih.reader.unclaimed.is_empty(), "{}", ih.reader.describe());
    // What the rungs took is back: the manager and the GART hold what they held when `sdma` had passed.
    assert_eq!((&s.bump.as_ref().unwrap().taken, &s.nb.gart.as_ref().unwrap().bound, s.sdma.held.clone()), (&taken, &bound, (None, None)));
    assert!(s.next().is_none() && s.summary().contains("power controller started, graphics block powered") && s.summary().contains("its hub on (a bind flushes both hubs)") && s.summary().contains("sdma on"), "{}", s.summary());
}

/// On the model: the three kinds of graphics register and the engines' doorbell take nothing until BOTH halves
/// hold, whichever comes first. On the driver: each stage of the wave is refused before any access until the one
/// before it passed (`stage::in_order`), across the items.
#[test]
fn no_graphics_register_takes_a_write_before_the_features_are_on_and_the_autoload_completed() {
    let rig = Rig::new();
    let n = n(&rig);
    // The words both models end their refusal with, the graphics block's and the engines'.
    let why = "its autoload is";
    let all_refused = |m: &mut Model, state: &str| {
        assert!(!m.gc_awake(), "{state}");
        for (what, byte, v) in gc_writes(&rig, &n) {
            let before = m.refused.len();
            assert!(!m.reg_write(byte, v) && m.refused.len() == before + 1 && m.refused[before].contains(why), "{state}: {what}: {:?}", &m.refused[before..]);
        }
        let before = m.refused.len();
        assert!(!m.doorbell_write64(sdma7::doorbell_dw(0), 0x40) && m.refused[before].contains(why), "{state}: the doorbell: {:?}", &m.refused[before..]);
        m.refused.clear();
    };
    // The cold card.
    all_refused(&mut model(), "cold");
    // The firmware loaded and the autoload complete, the power controller not started: the chain before `pmfw`.
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::Pmfw));
    assert!(!m.gc_powered() && gc12::autoload_complete(m.cp_stat, m.bootload));
    all_refused(&mut m, "the autoload without the features");
    // The features on (forced: no chain reaches this), the autoload not sent.
    let (_, mut early) = rig.pass(&mut Session::default(), model(), &mut Stick::full(), chain_before(Stage::Autoload));
    early.pmfw.enabled = true;
    assert!(early.gc_powered() && !gc12::autoload_complete(early.cp_stat, early.bootload));
    all_refused(&mut early, "the features without the autoload");
    // The driver's side, on the card before `pmfw`: each later stage of the wave is refused before any access,
    // and names the stage that must pass first.
    for (stage, needs) in [(Stage::GcBoot, "pmfw"), (Stage::GcHub, "gcboot"), (Stage::GcCp, "gchub"), (Stage::Mes, "gccp"), (Stage::MesQ, "mes"), (Stage::GcRing, "mesq"), (Stage::Sdma, "gcring"), (Stage::SdmaTest, ""), (Stage::RingTest, "")] {
        let writes = m.writes.len();
        let (d, back) = rig.run(&mut s, stage, m, &mut Stick::full());
        m = back;
        assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && m.writes.len() == writes && m.refused.is_empty(), "{stage:?}: {}", d.line);
        if !needs.is_empty() {
            assert!(d.lines.last().unwrap().contains(&std::format!("{} needs the {needs} stage of this boot", stage.name())), "{:?}", d.lines);
        }
    }
    // The power controller's start gives the block its power, and from then on each kind is taken at its step.
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Pmfw]);
    assert!(m.gc_awake() && s.pmfw.gc_powered());
    for (what, byte, v) in gc_writes(&rig, &n) {
        assert!(m.reg_write(byte, v), "{what}: {:?}", m.refused);
    }
    // A power controller that loses its features again takes the block's power with it: one answer for all.
    m.pmfw.enabled = false;
    all_refused(&mut m, "the features gone again");
}

/// A session whose record says the power controller started, on a card whose features are not on: the order
/// lets each stage run, the model refuses its first write, and the stage stops there. No stage of the wave
/// writes a graphics register on its own word alone.
#[test]
fn a_card_without_its_features_stops_each_stage_at_its_first_write() {
    let rig = Rig::new();
    for (i, stage) in [Stage::GcHub, Stage::Sdma].into_iter().enumerate() {
        let mut s = Session::default();
        let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(stage));
        m.pmfw.enabled = false;
        let (d, m) = rig.run(&mut s, stage, m, &mut Stick::full());
        // The one write of a graphics register the stage came to did not reach the card; nothing of the block
        // changed (what reached the card before it is the bind's flush of the hub that is on, and NBIF's).
        assert!(!d.pass && m.refused.len() == 1 && m.refused[0].contains("its autoload is"), "{stage:?}: {} {:?}", d.line, m.refused);
        assert_eq!(d.trail.iter().filter(|a| matches!(a, Access::Write { reached: false, .. })).count(), 1, "{stage:?}");
        assert!(matches!(d.trail.last(), Some(Access::Write { reached: false, .. })), "{stage:?}: the stage went on after the write that did not arrive");
        assert!(!s.passed(stage) && (stage == Stage::Sdma || !m.gc.context0) && !m.sdma.any_on(), "{stage:?}");
        assert!(d.lines.iter().any(|l| l.starts_with(&std::format!("linuxseq {}: differs: ", ["gc-hub", "sdma"][i]))), "{:?}", d.lines);
    }
    // `gcboot` writes nothing at all on such a card: its wait for the autoload is Linux's loop to its end.
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcBoot));
    m.pmfw.enabled = false;
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::GcBoot, m, &mut Stick::full());
    assert!(!d.pass && m.writes.is_empty() && m.refused.is_empty() && d.totals.writes == 0 && d.line.contains("der Grafikteil der Karte ist nicht gestartet"), "{}", d.line);
}

/// The engines' `vm_hub` is GFXHUB (sdma_v7_0.c:1328): their rings are found through the hub `gchub` started,
/// and an address that hub does not know is a fault latched in GFXHUB's status and sent to its source in the
/// IH. The stage `sdmatest` reads the IH rings itself; the fault it finds there is read, cleared and said in
/// plain words through the stage's own guard, and the engine that faulted fails its rung.
#[test]
fn the_engines_addresses_go_through_gfxhub_and_a_bad_one_reaches_the_fault_decode() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let place = s.sdma.up.as_ref().unwrap().place;
    // Both walkers find the rings (the bind flushed both hubs); switch GFXHUB's context 0 off on the card and
    // the engines' walker finds nothing, while MMHUB's still does.
    assert!((0..2).all(|i| m.gc_walk(place.ring_mc(i)).is_some() && m.gart_walk(place.ring_mc(i)).is_some()));
    let mut off = up(&rig).1;
    let cntl = n(&rig).byte(GC.context0_cntl).unwrap();
    assert!(off.reg_write(cntl, 0) && off.gc_walk(place.ring_mc(0)).is_none() && off.gart_walk(place.ring_mc(0)).is_some());
    assert!(!push(&mut off, s.sdma.up.as_ref().unwrap(), 0, &[pkt::NOP]) && off.refused.pop().unwrap().contains("write-pointer poll slot holds None"), "a doorbell on a hub that is off found its slot");
    // An address no entry holds: engine 0 is told to write the stage's test pages, which are not bound.
    let unbound = place.mc + lay::TEST;
    let mut w = Vec::new();
    pkt::write_linear(&mut w, unbound, &[0x1234_5678]).unwrap();
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &w));
    assert!(m.faults_open() == [true, false] && m.sdma.e[0].halted.is_some() && m.refused.len() == 1);
    m.refused.clear();
    // The rungs: the stage's own IH reader takes the fault while it waits for the engine that stopped.
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(!d.pass && d.refused.is_empty() && m.refused.is_empty(), "{} {:?} {:?}", d.line, d.refused, m.refused);
    assert!(d.line.contains("angehalten: die Kopiereinheit hat einen Auftrag nicht beendet") && d.lines.last().unwrap().starts_with("stopped: engine 0: fence 1 did not come"), "{}\n{}", d.line, d.lines.join("\n"));
    // The call made no register access of its own (it stopped before the test pages' bind); the fault's three
    // (the status read twice, the clear's read and write) are Linux's handler's and are said as such.
    let seq = d.lines.iter().find(|l| l.starts_with("linuxseq")).unwrap();
    assert!(seq.starts_with("linuxseq mes: the same as Linux's as far as the stage came: the call's only register accesses are 0 of the TLB flush") && seq.contains("; 4 accesses of Linux's interrupt handlers"), "{seq}");
    let ih = s.nb.ih.as_ref().unwrap();
    let said = std::format!("{}", ih.reader.faults.last.expect("the fault reached the decode"));
    assert_eq!(said, std::format!("gfxhub page fault at {unbound:#018x} (vmid 0, pasid 0): SDMA0 (client id 0xd) tried to write; the page does not allow it (permission faults 0x5)"));
    assert_eq!((ih.reader.faults.count, ih.reader.faults.unread, m.faults_open()), ([1, 0], 0, [false, false]));
    // The clear went through the stage's guard: GFXHUB's fault control is on the rungs' write list, and the
    // write is in the stage's log.
    let fault_cntl = n(&rig).byte(GC.fault_cntl).unwrap();
    assert!(d.trail.iter().any(|a| matches!(a, Access::Write { byte, reached: true, .. } if *byte == fault_cntl)), "the clear of GFXHUB's fault is not in the stage's trail");
    // `state` shows it afterwards, in the same words.
    let (d, _) = rig.run(&mut s, Stage::State, m, &mut Stick::full());
    assert!(d.lines.iter().any(|l| l.contains("page faults: gfxhub 1, mmhub 0") && l.contains("SDMA0 (client id 0xd) tried to write")), "{:?}", d.lines);
}

/// From `gchub` on a bind or unbind flushes both hubs, GFXHUB first: by registers until MES runs (MES's own bind,
/// the last of that form), through MES after it (`sdma`'s bind and the rungs' bind and unbind: the CPU writes no
/// request, MES's WR_WAIT_WR_REG does), each seen by GFXHUB's own invalidation; before `gchub` a bind flushes
/// MMHUB alone.
#[test]
fn every_gart_bind_flushes_both_hubs_once_gfxhub_is_on() {
    let rig = Rig::new();
    let n = n(&rig);
    let eng = |first| n.byte(member(first, hub41::GART_ENGINE, 1)).unwrap();
    let (gc_req, mm_req) = (eng(GC.eng0_req), eng(hub41::MM.eng0_req));
    let reqs = |trail: &[Access]| -> Vec<&'static str> {
        trail
            .iter()
            .filter_map(|a| match a {
                Access::Write { byte, .. } if *byte == gc_req => Some("gfxhub"),
                Access::Write { byte, .. } if *byte == mm_req => Some("mmhub"),
                _ => None,
            })
            .collect()
    };
    // Up to the hub: `ih`'s bind went out on MMHUB alone, and the session's flush names no GFXHUB before `gchub`.
    let mut s = Session::default();
    let (done, m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcHub));
    assert_eq!(reqs(&done[2].trail), ["mmhub"]);
    assert!(s.flush_regs(&n).unwrap().gc.is_none() && m.gc.tlb_flushes == 0);
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcHub, Stage::GcCp]);
    assert!(s.flush_regs(&n).unwrap().gc.is_some() && m.gc.tlb_flushes == 1, "the command processor binds nothing");
    // `mes`: its one bind, both hubs by registers, GFXHUB first; MES does not run yet.
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Mes, Stage::MesQ, Stage::GcRing]);
    assert_eq!((reqs(&d[0].trail), reqs(&d[1].trail), m.gc.tlb_flushes, m.mes.misc_writes.len()), (std::vec!["gfxhub", "mmhub"], std::vec![], 2, 0));
    // `sdma`: one bind, both hubs, through MES: GFXHUB's and MMHUB's requests are the firmware's.
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Sdma]);
    assert_eq!((reqs(&d[0].trail), m.gc.tlb_flushes, m.gc.entries_pending(), m.mes.misc_writes.as_slice()), (std::vec![], 3, 0, &[(gc_req / 4, hub41::invalidate_req(0)), (mm_req / 4, hub41::invalidate_req(0))][..]));
    // The rungs: the test pages' bind and their unbind.
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::SdmaTest]);
    assert_eq!((reqs(&d[0].trail), m.gc.tlb_flushes, m.gc.entries_pending(), m.mes.misc_writes.len()), (std::vec![], 5, 0, 6));
    // `mes`'s list holds both requests; the lists of the stages after it hold neither (a request register on the
    // list of a stage that flushes through MES would be a hole).
    let list = crate::rdna4::messtage::allow(Stage::Mes, &s, &n);
    assert!(list.regs.contains(&gc_req) && list.regs.contains(&mm_req));
    for stage in [Stage::Sdma, Stage::SdmaTest] {
        let list = crate::rdna4::sdmastage::allow(stage, &mut s, &n, Some(&sr(&rig)));
        assert!(!list.regs.contains(&gc_req) && !list.regs.contains(&mm_req), "{stage:?}");
    }
}

/// One manager for the card's VRAM across all items: the picture, the secure processor's window and the
/// firmware's regions are kept; the GART table, the default page and the power controller's two tables are
/// taken outside them; the rungs' range comes and goes; nothing lies over anything else, on the driver's side
/// and in what the card was told.
#[test]
fn the_reservations_hold_across_all_items_and_the_rungs_give_back_what_they_took() {
    let rig = Rig::new();
    let (mut s, m, _) = up(&rig);
    let l = s.layout.clone().unwrap();
    let names = |s: &Session| -> Vec<&'static str> { s.bump.as_ref().unwrap().taken.iter().map(|t| t.0).collect() };
    assert_eq!(names(&s), ["GART table", "default page", "the PMFW's tools table", "the PMFW's driver table", "the clear-state buffer", "MES resource_1 pages"]);
    let (taken, bound) = (s.bump.as_ref().unwrap().taken.clone(), s.nb.gart.as_ref().unwrap().bound.clone());
    // While the rungs run their range is the manager's seventh: taken before the guard exists, listed by it.
    let range = sdmarungs::take_vram(&mut s).unwrap();
    assert_eq!(names(&s).last(), Some(&"SDMA test ranges"));
    let bump = s.bump.as_ref().unwrap();
    let kept: Vec<(Kept, Range<u64>)> = bump.kept().iter().map(|(k, r)| (*k, r.clone())).collect();
    assert_eq!(kept.iter().map(|k| k.0).collect::<Vec<_>>(), [Kept::Picture, Kept::PspWindow, Kept::Firmware, Kept::Firmware]);
    let mut all: Vec<(&str, Range<u64>)> = bump.taken.iter().map(|t| (t.0, t.1.clone())).collect();
    all.extend(kept.iter().map(|(k, r)| (if *k == Kept::PspWindow { "the window" } else { "kept" }, r.clone())));
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert!(!overlap(&a.1, &b.1), "{} {:x?} lies over {} {:x?}", a.0, a.1, b.0, b.1);
        }
    }
    assert_eq!((range.start, range.end - range.start), (l.mc(bump.taken[6].1.start), sdmarungs::VRAM_BYTES));
    // What the card was told lies where the manager says: the hub's table and default page, the PMFW's tables.
    let (tools, driver) = s.pmfw.tables.clone().unwrap();
    assert_eq!(m.nb.vram_held().iter().map(|h| h.1.clone()).collect::<Vec<_>>(), [taken[0].1.clone(), taken[1].1.clone()]);
    assert_eq!((taken[2].1.clone(), taken[3].1.clone()), (tools, driver));
    // The GART's address space: the IH's one bind, MES's and the engines' behind it.
    assert_eq!(bound.iter().map(|b| (b.0, b.1, b.2.len())).collect::<Vec<_>>(), [("IH rings", 1, 129), ("MES rings, MQDs and the kernel queues", 130, 38), ("SDMA rings", 168, 6)]);
    // The rungs pass and give back: the manager and the GART hold what they held after `sdma`, twice over.
    let mut m = m;
    for _ in 0..2 {
        let (d, back) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
        m = back;
        assert!(d.pass && m.refused.is_empty(), "{} {:?}", d.line, m.refused);
        assert_eq!((&s.bump.as_ref().unwrap().taken, &s.nb.gart.as_ref().unwrap().bound, s.sdma.held.clone()), (&taken, &bound, (None, None)));
    }
    // The ring's destroy gives the window back and leaves every item's own: the tables, the engines, the hub.
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy]);
    assert!(s.bump.as_ref().unwrap().given.is_empty() && s.bump.as_ref().unwrap().taken == taken && s.nb.gart.as_ref().unwrap().bound == bound);
    assert!(s.pmfw.passed && s.gc.hub && s.sdma.up.is_some() && m.sdma.any_on() && s.next() == Some(Stage::Ring));
}

/// A second `fw` in one boot would load the SMU's firmware and send the autoload under engines that run. Linux
/// stops them first (its suspend's `sdma_v7_0_hw_fini`); that stop and second start is not built, so the stage
/// refuses before any access, and the model refuses the SMU's image on its own account.
#[test]
fn a_second_fw_is_refused_once_the_engines_ran() {
    let rig = Rig::new();
    let (mut s, m, _) = up(&rig);
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy, Stage::Ring, Stage::Toc, Stage::Tmr]);
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && m.writes.is_empty() && m.refused.is_empty(), "{} {:?}", d.line, d.lines);
    assert!(d.line.contains("die Kopiereinheit der Karte wurde in diesem Start schon gestartet: erst neu starten") && d.lines.last().unwrap().contains("sdma_v7_0_hw_fini"), "{} {:?}", d.line, d.lines.last());
    assert!(s.sdma.up.is_some() && s.gc.hub && s.pmfw.passed && m.gc.context0 && m.sdma.any_on(), "nothing of the card or the session changed");
    // A session that forgot its engines: MES ran too, and refuses on its own account before any access.
    s.sdma = crate::rdna4::sdmastage::State::new();
    let (d, mut m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.refused.is_empty() && d.line.contains("der Planer der Karte wurde in diesem Start schon gestartet"), "{}", d.line);
    // The model: a session that forgot MES and the command processor too, on a card whose MES was halted as a Reload halts it, switches GFXHUB
    // off as HB's item does, sends the SMU's image, and the card refuses it while a ring is on.
    s.mes = crate::rdna4::messtage::State::new();
    s.cp = crate::rdna4::cpstage::State::new();
    let cntl = n(&rig).byte(rdna4_core::mes12::CP_MES_CNTL).unwrap();
    let v = m.reg_read(cntl).unwrap();
    took(&mut m, cntl, rdna4_core::mes12::halted(v));
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(!d.pass && !m.gc.context0, "{}", d.line);
    assert_eq!(m.refused.len(), 1, "{:?}", m.refused);
    assert!(m.refused[0].starts_with("LOAD_IP_FW of the SMU's firmware while a copy engine's ring is on"), "{:?}", m.refused);
    // An engine's own stop takes that refusal away: the rings off, as Linux's fini leaves them.
    let (mut s, m, _) = up(&rig);
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy, Stage::Ring, Stage::Toc, Stage::Tmr]);
    for i in 0..2 {
        let rb = sr(&rig).byte(sdma7::RB_CNTL, i).unwrap();
        let v = m.reg_read(rb).unwrap();
        took(&mut m, rb, sdma7::rb_cntl_stopped(v));
    }
    let cntl = n(&rig).byte(rdna4_core::mes12::CP_MES_CNTL).unwrap();
    let v = m.reg_read(cntl).unwrap();
    took(&mut m, cntl, rdna4_core::mes12::halted(v));
    (s.sdma, s.mes) = (crate::rdna4::sdmastage::State::new(), crate::rdna4::messtage::State::new());
    s.cp = crate::rdna4::cpstage::State::new();
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty() && !m.gc_powered(), "{} {:?}", d.line, m.refused);
}

/// The kernel tests of the wave's cross-item rules (`gx.rdna4.w43.gate`, `.fault`, `.again`), also here: their
/// bodies need nothing of the kernel but its log.
#[test]
fn the_kernel_tests_across_the_items_hold_on_the_host_too() {
    crate::ktests_rdna4w43::gate().unwrap();
    crate::ktests_rdna4w43::fault().unwrap();
    crate::ktests_rdna4w43::again().unwrap();
}
