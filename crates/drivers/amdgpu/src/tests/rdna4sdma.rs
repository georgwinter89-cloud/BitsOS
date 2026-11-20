//! Wave 43 (piece 12): the stage `sdma` on the model (`rdna4::fakesdma`). It equals the trace's section `sdma`
//! with its seven declared rules and nothing else; it refuses, before any access, what is its own to refuse; the
//! ring tests run as Linux's; and the stage's guard lets through only its own doorbells and its own system
//! memory. The Reload hook is `rdna4sdmaoff`.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::guard::{Allow, Guard, HostAsk, Refusal};
use crate::rdna4::host::Host;
use crate::rdna4::linuxseq::compare::Compare;
use crate::rdna4::linuxseq::fold::Bus;
use crate::rdna4::linuxseq::rules::Rule;
use crate::rdna4::linuxseq::section;
use crate::rdna4::sdmaq::lay;
use crate::rdna4::sdmaregs::SdmaRegs;
use crate::rdna4::stage::{Done, Session, Stage};
use crate::rdna4::chainseq::{self, Chosen};
use crate::rdna4::sdmaseq;
use rdna4_core::{layout, sdma7};
use std::vec::Vec;

/// The chain on a cold model up to the stage before `sdma`: loaded firmware, the power controller started (the
/// graphics block has power), the graphics block's start and its hub on. The card as the stage `sdma` meets it.
pub fn loaded(rig: &Rig) -> (Session, Model) {
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::Sdma));
    m.writes.clear();
    (s, m)
}

/// The chain's stages before `stage`.
pub fn chain_before(stage: Stage) -> &'static [Stage] {
    let at = Stage::CHAIN.iter().position(|x| *x == stage).expect("a stage of the chain");
    &Stage::CHAIN[..at]
}

/// `loaded`, then the stage.
pub fn up(rig: &Rig) -> (Session, Model, Done) {
    let (mut s, m) = loaded(rig);
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(d.pass && d.refused.is_empty() && m.refused.is_empty(), "{}\n{}\n{:?}", d.line, d.lines.join("\n"), m.refused);
    (s, m, d)
}

pub fn sr(rig: &Rig) -> SdmaRegs {
    SdmaRegs::from(&rig.att.discovery).unwrap()
}

#[test]
fn the_stage_equals_its_section_with_seven_declared_rules() {
    let rig = Rig::new();
    let (s, m, d) = up(&rig);
    let sec = section("sdma").unwrap();
    let rules = sdmaseq::rules(s.sdma.bound);
    assert_eq!((sec.lines.len(), sec.lines[0].init, sec.lines.last().unwrap().init), (68, 9604, 9671));
    let mut c = Compare::new(&sec, &rules).unwrap();
    c.feed_trail(&d.trail, d.trail_lost, &Bus::of(&regs(&rig))).unwrap_or_else(|e| panic!("{e}\n{}", d.lines.join("\n")));
    c.finish().unwrap_or_else(|e| panic!("{e}"));
    // One bind and six addresses, each exactly the object the stage bound; nothing else differs from Linux.
    assert_eq!(rules.len(), 7);
    let b = s.sdma.bound.unwrap();
    let up = s.sdma.up.as_ref().unwrap();
    assert_eq!(b, sdmaseq::Bound::of(&up.place, &[up.q[0].slots, up.q[1].slots]));
    let objects = [b.wptr[0], b.rptr[0], b.ring[0], b.wptr[1], b.rptr[1], b.ring[1]];
    let addrs: Vec<(u64, u64)> = rules.iter().filter_map(|r| if let Rule::Address(a) = r { Some((a.within.start, a.within.end - a.within.start)) } else { None }).collect();
    assert_eq!(addrs, objects.iter().zip([8, 8, u64::from(sdma7::RING_BYTES), 8, 8, u64::from(sdma7::RING_BYTES)]).map(|(a, n)| (*a, n)).collect::<Vec<_>>());
    // The bind is one made once MES runs, so its flush is Linux's form after MES: three HDP flushes on the bus.
    assert_eq!(rules.iter().filter(|r| matches!(r, Rule::BindMesBefore { .. })).count(), 1);
    // Without the stage's record only the bind's rule: an address written then differs.
    assert!(matches!(sdmaseq::rules(None).as_slice(), [Rule::BindMesBefore { .. }]));
    assert!(rules.iter().all(|r| match r {
        Rule::Address(a) => a.within.end <= layout::GART_BYTES && a.keep == 0 && a.hi.is_some() && a.within.end - a.within.start == a.bytes,
        Rule::BindMesBefore { reg, why } => *reg == "SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO" && why.contains("when MES runs"),
        _ => false,
    }));
    // The stage says so itself, as its answer's last line.
    assert_eq!(d.lines.last().unwrap(), "linuxseq sdma: equal");
    assert!(d.line.ends_with("Kopiereinheit der Karte starten: fertig."), "{}", d.line);
    // Linux's 49 writes, and the bind's three HDP flushes (the bind's own, GFXHUB's, MMHUB's: the hubs' requests
    // are MES's writes, not the CPU's); 19 reads of the section and the flushes' three memory size reads.
    assert_eq!(d.totals.writes, 49 + 3);
    assert_eq!(d.trail.iter().filter(|a| matches!(a, Access::Read { .. })).count(), 19 + 3, "the section's reads, the three flushes' memory size reads");
    // GFXHUB saw the bind: its own invalidation went out with it, by MES (once at its start, once with MES's own
    // bind by registers, once here).
    assert_eq!(m.gc.tlb_flushes, 3);
    assert_eq!(m.mes.misc_writes.len(), 2, "one WR_WAIT_WR_REG per hub");
    assert!(s.passed(Stage::Sdma) && s.next().is_none());
    let _ = m;
}

#[test]
fn what_the_stage_leaves_behind() {
    let rig = Rig::new();
    let (s, mut m, d) = up(&rig);
    let up = s.sdma.up.as_ref().unwrap();
    let g = s.nb.gart.as_ref().unwrap();
    // One piece of system memory, its first six pages bound behind the IH's rings; the test pages are not.
    let sys = s.sdma.piece.unwrap();
    assert_eq!(m.nb.sys_given.last().unwrap(), &(sys..sys + lay::PIECE));
    assert_eq!(g.bound.last().map(|b| (b.0, b.2.len() as u64 * 4096, b.2[0])), Some(("SDMA rings", lay::BOUND, sys)));
    assert_eq!((up.place.sys, g.system_of(up.place.mc), g.system_of(up.place.mc + lay::TEST)), (sys, Some(sys), None));
    // What the card was told: each ring's base, size and slots, as Linux computes them.
    let r = sr(&rig);
    for i in 0..2u32 {
        let e = &m.sdma.e[i as usize];
        let cntl = e.val(sdma7::RB_CNTL);
        assert_eq!((cntl, 4u32 << sdma7::f::RB_SIZE.get(cntl)), (0x0084_1817, sdma7::RING_BYTES), "engine {i}");
        assert_eq!(u64::from(e.val(sdma7::RB_BASE)) << 8, up.place.ring_mc(i));
        assert_eq!(u64::from(e.val(sdma7::RB_RPTR_ADDR_LO)), up.place.slot_mc(up.q[i as usize].slots.rptr));
        assert_eq!(u64::from(e.val(sdma7::RB_WPTR_POLL_ADDR_LO)), up.place.slot_mc(up.q[i as usize].slots.wptr));
        assert_eq!((e.val(sdma7::DOORBELL), e.val(sdma7::DOORBELL_OFFSET) >> 2), (0x1000_0000, sdma7::doorbell_dw(i)));
        assert_eq!((e.val(sdma7::WATCHDOG_CNTL), e.val(sdma7::UTCL1_CNTL), e.val(sdma7::UTCL1_PAGE), e.val(sdma7::IB_CNTL), e.val(sdma7::MCU_CNTL)), (1, 0x3B00_0689, 0x000C_EC20, 0x101, 0));
        // Linux's slots in Linux's order: rptr, wptr, fence, trail, cond_exe, five per ring.
        let q = &up.q[i as usize];
        assert_eq!([q.slots.rptr, q.slots.wptr, q.slots.fence, q.slots.trail, q.slots.cond_exe].map(|x| x.index()), [0, 1, 2, 3, 4].map(|k| k + 5 * i));
        // The ring test: one WRITE_LINEAR padded to the fetch size, so the doorbell said 16 dwords, in bytes; the
        // slot read 0xDEADBEEF at the first read; the engine wrote its read pointer back as 64 bits of bytes.
        assert_eq!((q.rung.as_slice(), up.ring_test[i as usize]), (&[(sdma7::doorbell_dw(i), 0x40)][..], 1));
        assert_eq!((e.rptr, e.wptr, e.on, e.halted.clone()), (0x40, 0x40, true, None));
        let mut b = [0u8; 8];
        assert!(m.sys_read(up.place.slot_sys(q.slots.rptr), &mut b));
        assert_eq!(u64::from_le_bytes(b), 0x40);
        // The conditional-execution slot holds 1, and the test's slot was given back.
        assert!(m.sys_read(up.place.slot_sys(q.slots.cond_exe), &mut b));
        assert_eq!(u64::from_le_bytes(b), 1);
        assert_eq!(m.reg_read(r.byte(sdma7::STATUS_REG, i).unwrap()), Some(crate::rdna4::fakesdma::STATUS_IDLE));
    }
    assert_eq!((m.sdma.doorbells.as_slice(), up.pool.used(), m.sdma.entry2), (&[(0x200, 0x40), (0x214, 0x40)][..], 10, 0x3051_001D));
    // The counted NOP only from the firmware's feature version 20 on: the fixture file says 0.
    assert_eq!(up.feature, 0);
    assert!(d.lines.iter().any(|l| l.contains("firmware feature version 0, so the NOP is plain")), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.starts_with("sdma1: ring at mc ") && l.contains("ring test passed after 1 reads")), "{:?}", d.lines);
    assert!(s.summary().contains("sdma on (rings at mc "), "{}", s.summary());
}

#[test]
fn the_counted_nop_follows_the_firmwares_feature_version() {
    let rig = Rig::new();
    let (mut s, m) = loaded(&rig);
    let mut stick = Stick::full();
    let path = crate::gpucore::fwload::stick_path("amdgpu/sdma_7_0_1.bin");
    stick.0.get_mut(&path).unwrap()[32..36].copy_from_slice(&20u32.to_le_bytes());
    let (d, mut m) = rig.run(&mut s, Stage::Sdma, m, &mut stick);
    assert!(d.pass && m.refused.is_empty(), "{} {:?}", d.line, m.refused);
    let up = s.sdma.up.as_ref().unwrap();
    assert_eq!(up.feature, 20);
    // The ring test's pad is one counted NOP of 11 dwords behind the five of the WRITE_LINEAR.
    let mut b = [0u8; 4];
    assert!(m.sys_read(up.place.sys + lay::ring(0) + 5 * 4, &mut b));
    assert_eq!(u32::from_le_bytes(b), 10 << 16);
    assert_eq!(d.lines.last().unwrap(), "linuxseq sdma: equal");
    // A file too short for its header stops the stage before any access.
    let (mut s, m) = loaded(&rig);
    stick.0.get_mut(&path).unwrap().truncate(34);
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut stick);
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && m.nb.sys_given.len() == 3, "{}", d.line);
}

#[test]
fn the_stage_refuses_before_any_access_what_is_its_own_to_refuse() {
    let rig = Rig::new();
    let untouched = |d: &Done, m: &Model, what: &str| {
        assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && d.refused.is_empty() && m.writes.is_empty() && m.refused.is_empty(), "{what}: {} {:?}", d.line, d.lines);
        assert!(d.lines.iter().all(|l| !l.starts_with("linuxseq ")), "{what}: a call refused before any access says nothing about the trace");
    };
    // A second run.
    let (mut s, mut m, _) = up(&rig);
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    untouched(&d, &m, "a second run");
    assert!(d.line.contains("läuft schon"), "{}", d.line);
    // The firmware: no image of the engine, or no autoload.
    for cut in 0..2 {
        let (mut s, m) = loaded(&rig);
        if cut == 0 {
            s.loaded.retain(|l| l.0 != crate::rdna4::fwlist::Id::SdmaRs64);
        } else {
            s.autoload = false;
        }
        // The chain's own order check asks for `ta` only, which stays passed: this is the stage's own.
        let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
        untouched(&d, &m, "no firmware");
        assert!(d.line.contains("zuerst die Steuerprogramme laden") && m.nb.sys_given.len() == 3, "{}", d.line);
    }
    // No GART.
    let (mut s, m) = loaded(&rig);
    s.nb.gart = None;
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    untouched(&d, &m, "no GART");
    // The power controller's features are not on by this boot's record (a session whose record lost them): the
    // chain's order still allows the stage (`pmfw` and `gchub` count as passed) and the model would take every
    // write, and still no register is read or written.
    let (mut s, m) = loaded(&rig);
    s.pmfw.facts.enabled = false;
    assert!(crate::rdna4::stage::in_order(&s, Stage::Sdma).is_ok() && m.gc_powered());
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    untouched(&d, &m, "no GC power on record");
    assert!(d.line.contains("zuerst die Energieverwaltung der Karte starten") && !s.sdma.tried && s.sdma.piece.is_none() && m.nb.sys_given.len() == 3, "{}", d.line);
    // With the fact the same session's stage passes.
    s.pmfw.facts.enabled = true;
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty(), "{} {:?}", d.line, d.lines);
    // The kernel rings are not on record (wave 44: the stage before `sdma` in the chain, which stands on MES,
    // the command processor and GFXHUB): the engines' bind flushes through MES, and the chain's order is what
    // holds the stage to it, so the stage neither binds nor writes.
    let (mut s, m) = loaded(&rig);
    s.rings.passed = false;
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    untouched(&d, &m, "no kernel rings on record");
    assert!(d.line.contains("zuerst die Befehlsringe prüfen") && !s.sdma.tried && s.sdma.piece.is_none(), "{}", d.line);
    // The machine's side: the survey says no, or there is no memory. Nothing was taken or bound.
    let (mut s, mut m) = loaded(&rig);
    m.nb.survey = Err("TranslationOn");
    let (d, mut m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    untouched(&d, &m, "the survey");
    assert!(d.line.contains("lässt die Karte nicht an den Arbeitsspeicher") && m.nb.sys_given.len() == 3, "{}", d.line);
    (m.nb.survey, m.nb.pages_refused) = (Ok(()), true);
    let (d, mut m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    untouched(&d, &m, "no pages");
    assert_eq!((s.sdma.piece, s.sdma.tried, s.nb.gart.as_ref().unwrap().bound.len()), (None, false, 2), "the IH's bind and MES's, none of the engines'");
    // Neither stop counts as half-way: with memory the same session's stage passes, and equals Linux.
    m.nb.pages_refused = false;
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty() && d.lines.last().unwrap() == "linuxseq sdma: equal", "{} {:?}", d.line, d.lines);
    // The second card is told that it stays untouched.
    let mut second = Rig::new();
    second.display = false;
    let (mut s, m) = loaded(&rig);
    let (d, m) = second.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    untouched(&d, &m, "the second card");
    assert!(d.line.contains("die zweite Karte bleibt unberührt"), "{}", d.line);
    // Out of the chain's order: without any of the stages between the loaded firmware and the engines, the
    // session's one order refuses before any access and names the stage that must pass first (the chain's one
    // right before `sdma`; since wave 44 the rings').
    let prev = chain_before(Stage::Sdma).last().copied().unwrap();
    for missing in [Stage::Pmfw, Stage::GcBoot, Stage::GcHub, Stage::GcCp, Stage::Mes, Stage::MesQ, Stage::GcRing] {
        let mut s = Session::default();
        let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(missing));
        m.writes.clear();
        let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
        untouched(&d, &m, "out of order");
        assert!(d.line.contains("zuerst die Befehlsringe prüfen") && d.lines.last().unwrap().contains(&std::format!("sdma needs the {} stage of this boot", prev.name())), "{missing:?}: {} {:?}", d.line, d.lines);
    }
}

/// The GC block's power is the power controller's doing. Without it the model takes no write of the block: the
/// stage stops at its first one, says so, and does not run a second time in this boot.
#[test]
fn without_the_gc_blocks_power_the_first_write_is_refused_and_the_stage_is_spent() {
    let rig = Rig::new();
    let (mut s, mut m) = loaded(&rig);
    // The card's power controller lost its features behind the driver's back (the session's record still says
    // they are on): the model's one power answer is the power controller's model itself.
    m.pmfw.enabled = false;
    let (d, mut m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("ein Schreibzugriff war nicht erlaubt"), "{}", d.line);
    // One read (MCU_CNTL, answered as the trace's), one write that did not reach the card.
    assert_eq!(d.trail.len(), 2);
    assert!(matches!(d.trail[1], Access::Write { reached: false, value: 0, .. }), "{:?}", d.trail);
    assert_eq!(m.refused, ["SDMA0_MCU_CNTL written 0x0 while the GC block has no power or its autoload is not complete (the power controller's start and the autoload come first)"]);
    assert!(d.lines.last().unwrap().starts_with("stopped: ") && d.lines.iter().any(|l| l.starts_with("linuxseq sdma: differs: ")), "{:?}", d.lines);
    assert!(s.sdma.tried && s.sdma.up.is_none() && !s.passed(Stage::Sdma));
    assert!(s.summary().contains("sdma stopped half-way"), "{}", s.summary());
    // With power the same boot's stage is refused all the same: its registers were written.
    (m.pmfw.enabled, m.refused) = (true, Vec::new());
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && d.line.contains("erst neu starten"), "{}", d.line);
}

/// An engine that never answers: Linux's ring test gives up after 100000 reads 1 us apart, and so does the stage.
#[test]
fn a_ring_test_that_runs_out_stops_the_stage_as_linux_gives_up() {
    let rig = Rig::new();
    for dead in 0..2usize {
        let (mut s, mut m) = loaded(&rig);
        m.sdma.dead[dead] = true;
        let t0 = m.card.now_us;
        let (d, m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
        assert!(!d.pass && d.line.contains("die Kopiereinheit antwortet nicht"), "{}", d.line);
        let why = d.lines.last().unwrap();
        assert!(why.contains(&std::format!("engine {dead}: the ring test's slot reads 0xcafedead after 100000 reads")), "{why}");
        // 100000 steps of 1 us and no sleep: the trail holds none.
        assert!(m.card.now_us - t0 >= 100_000 && d.trail.iter().all(|a| !matches!(a, Access::Sleep { .. })));
        // Every access up to there was Linux's; the other engine was reached only if it comes first.
        assert!(d.lines.iter().any(|l| l.starts_with("linuxseq sdma: the same as Linux's as far as the stage came")), "{:?}", d.lines);
        assert_eq!((m.sdma.e[0].on, m.sdma.e[1].on), (true, dead == 1));
        assert!(s.sdma.tried && s.sdma.up.is_none() && m.refused.is_empty());
    }
}

/// The stage's guard: a doorbell passes only at a listed dword, system memory only inside the stage's own.
#[test]
fn the_guard_lets_through_only_listed_doorbells_and_the_stages_own_system_memory() {
    let rig = Rig::new();
    let (s, m, _) = up(&rig);
    let r = regs(&rig);
    let sys = s.sdma.piece.unwrap();
    let ih = s.nb.ih.as_ref().unwrap().place.sys;
    let list = Allow { stage: "test", regs: Vec::new(), mem: Vec::new() };
    // No list: every doorbell is refused before the card, as in wave 42.
    let mut g = Guard::new(m, list.clone(), r.clone());
    assert!(!g.doorbell_write64(0x200, 0x40) && !g.doorbell_write32(0x2F0, 0));
    assert_eq!(g.refused, [Refusal::Doorbell { index_dw: 0x200 }, Refusal::Doorbell { index_dw: 0x2F0 }]);
    // Wave 42's rule for system memory stays for a stage that names none of its own: any address, with pages listed.
    assert!(!g.sys_write(sys, &[1]));
    let mut g = Guard::new(g.hw, list.clone(), r.clone()).with_host(std::vec![HostAsk::Pages]);
    assert!(g.sys_write(ih, &[0]) && g.refused.is_empty());
    // A stage that names its own: its doorbells, its piece, what it takes itself; nothing else.
    let mut g = Guard::new(g.hw, list, r).with_host(std::vec![HostAsk::Pages]).with_doorbells(std::vec![0x200, 0x214]).with_sys(std::vec![sys..sys + lay::PIECE]);
    assert!(!g.doorbell_write64(0x202, 0x40) && !g.doorbell_write32(0x2F0, 0), "a neighbour's doorbell passed");
    assert!(!g.sys_write(ih, &[0]) && !g.sys_write(sys + lay::PIECE - 1, &[0, 0]) && !g.sys_write(u64::MAX, &[0]), "another block's memory passed");
    assert_eq!(g.refused.len(), 5);
    assert!(matches!(g.refused[2], Refusal::Sys { bytes: 1, .. }) && g.refused[3] == Refusal::Sys { at: sys + lay::PIECE - 1, bytes: 2 });
    assert!(g.sys_write(sys + lay::PIECE - 1, &[0]) && g.hw.sdma.doorbells.len() == 2, "the stage's own memory, and no doorbell reached the card");
    let more = g.pages(4096).unwrap();
    assert!(g.sys_write(more, &[7; 4096]) && !g.sys_write(more + 4096, &[7]), "what the stage took itself, and not a byte behind it");
    // The stage's own lists are exactly these (`sdmastage::guarded`): the two engines' doorbells for `sdma`.
    let mut s = s;
    let n = crate::rdna4::nbregs::NbRegs::from(&rig.att.discovery, &regs(&rig)).unwrap();
    let allow = crate::rdna4::sdmastage::allow(Stage::Sdma, &mut s, &n, Some(&sr(&rig)));
    assert_eq!(allow.mem, [s.nb.gart.as_ref().unwrap().table_mc()]);
    assert_eq!(allow.regs.len(), 1 + 2 * 19 + 1, "the flush's HDP flush register (the hubs' requests are MES's writes), nineteen per engine, NBIF's entry");
    let g = crate::rdna4::sdmastage::guarded(Guard::new(g.hw, allow, regs(&rig)), Stage::Sdma, &s);
    let mut g = g;
    assert!(!g.doorbell_write32(0x2F0, 0) && !g.sys_write(ih, &[0]) && g.sys_write(sys, &[0]));
    // And no stage of wave 42's gets either from it.
    let mut g = crate::rdna4::sdmastage::guarded(Guard::new(g.hw, Allow::default(), regs(&rig)), Stage::Ta, &s);
    assert!(!g.doorbell_write64(0x200, 0x40) && !g.sys_write(sys, &[0]));
}

/// The whole chain's verdict knows the section: a stage's trail with one access taken out is named.
#[test]
fn a_stage_that_is_not_linuxs_is_named_by_its_own_line() {
    let rig = Rig::new();
    let (s, _, d) = up(&rig);
    let (r, l) = (regs(&rig), s.layout.clone().unwrap());
    let ask = |trail: &[Access]| chainseq::verdict(&mut chainseq::Seen::new(), Stage::Sdma, true, trail, 0, &r, rig.att.vram_bytes, Chosen { l: Some(&l), sdma: s.sdma.bound, ..Chosen::default() }).unwrap();
    assert_eq!(ask(&d.trail), "linuxseq sdma: equal");
    // The bind left out: Linux's flush is missing before the poll address. Through MES it is three HDP flushes
    // (each a write and the memory size's read) right before that register.
    let poll = sr(&rig).byte(sdma7::RB_WPTR_POLL_ADDR_LO, 0).unwrap();
    let at = d.trail.iter().position(|a| matches!(a, Access::Write { byte, .. } if *byte == poll)).unwrap();
    let mut no_bind = d.trail.clone();
    no_bind.drain(at - 6..at);
    assert!(ask(&no_bind).starts_with("linuxseq sdma: differs: sdma.txt line 12 (init.txt line 9614): BitsOS binds GART pages before the first write of SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO"), "{}", ask(&no_bind));
    // A value that is not Linux's and no address: the watchdog.
    let watchdog = sr(&rig).byte(sdma7::WATCHDOG_CNTL, 1).unwrap();
    let mut wrong = d.trail.clone();
    for a in wrong.iter_mut() {
        if let Access::Write { byte, value, .. } = a
            && *byte == watchdog
        {
            *value = 2;
        }
    }
    assert!(ask(&wrong).contains("Linux writes 0x00000001 to SDMA1_WATCHDOG_CNTL; the stage writes 0x00000002, and no rule declares a difference"), "{}", ask(&wrong));
    // An address outside the GART is no declared difference either.
    let base = sr(&rig).byte(sdma7::RB_BASE, 0).unwrap();
    let mut far = d.trail.clone();
    for a in far.iter_mut() {
        if let Access::Write { byte, value, .. } = a
            && *byte == base
        {
            *value = (layout::GART_BYTES >> 8) as u32;
        }
    }
    assert!(ask(&far).contains("the rule for it (engine 0's ring, where BitsOS bound it in the GART) does not hold"), "{}", ask(&far));
    // Inside the GART but on another object: each of the six pairs, pointed at the page behind its own, differs.
    let pairs = [(sdma7::RB_WPTR_POLL_ADDR_LO, 0), (sdma7::RB_RPTR_ADDR_LO, 0), (sdma7::RB_BASE, 8), (sdma7::RB_WPTR_POLL_ADDR_LO, 0), (sdma7::RB_RPTR_ADDR_LO, 0), (sdma7::RB_BASE, 8)];
    for (k, (reg, shift)) in pairs.into_iter().enumerate() {
        let byte = sr(&rig).byte(reg, (k / 3) as u32).unwrap();
        let mut moved = d.trail.clone();
        for a in moved.iter_mut() {
            if let Access::Write { byte: b, value, .. } = a
                && *b == byte
            {
                *value = value.wrapping_add((rdna4_core::pte12::PAGE >> shift) as u32);
            }
        }
        let said = ask(&moved);
        assert!(said.starts_with("linuxseq sdma: differs:") && said.contains("does not hold"), "pair {k}: {said}");
    }
}

/// The look `state` reads nothing of the engines before the stage `sdma` wrote them (the GC block may be without
/// power until then, and Linux has not touched it); from then on it shows what tells a stopped engine from a
/// running one.
#[test]
fn the_look_state_reads_the_engines_only_once_the_stage_wrote_them() {
    let rig = Rig::new();
    let r = sr(&rig);
    let engines = |d: &Done| d.trail.iter().filter(|a| matches!(a, Access::Read { byte, .. } if r.reg_at(*byte).is_some())).count();
    let (mut s, m) = loaded(&rig);
    let (d, m) = rig.run(&mut s, Stage::State, m, &mut Stick::full());
    assert!(d.pass && engines(&d) == 0 && d.lines.iter().all(|l| !l.starts_with("sdma registers:")), "{:?}", d.lines);
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Sdma]);
    let (d, m) = rig.run(&mut s, Stage::State, m, &mut Stick::full());
    let line = d.lines.iter().find(|l| l.starts_with("sdma registers: ")).unwrap_or_else(|| panic!("{:?}", d.lines));
    assert!(line.contains("sdma0 MCU_CNTL 0x0 STATUS_REG 0x5 RB_CNTL 0x841817 RPTR 0x40 WPTR 0x40 RPTR_FETCH 0x40 IB_CNTL 0x101") && line.contains("; sdma1 MCU_CNTL 0x0") && line.ends_with("CNTL 0x2441"), "{line}");
    assert!(d.pass && d.totals.writes == 0 && engines(&d) == 22 && m.refused.is_empty());
}
