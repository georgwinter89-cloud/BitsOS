//! Wave 43, item HB: the stages `gcboot` and `gchub` on the model (`rdna4::fakegc`, `fakegchub`). Each equals its
//! section of Linux's trace, `gchub` with only the three addresses `gmc` chose; the block answers nothing and
//! takes nothing before power and a complete autoload; the autoload's wait is Linux's loop and ends as Linux's;
//! and the model refuses what Linux would not do to the engines and to the hub.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use super::rdna4nbmodel::{n, refused};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakegc::BOOTLOAD_DONE;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::gcstage;
use crate::rdna4::host::Host;
use crate::rdna4::linuxseq::rules::Rule;
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::gc12;
use rdna4_core::hub41::{self, GC, MM};
use rdna4_core::nbreg::member;
use std::vec::Vec;

/// The chain's stages before `stage`, whatever stands there (stages of other blocks take their places in it).
pub fn chain_before(stage: Stage) -> &'static [Stage] {
    &Stage::CHAIN[..Stage::CHAIN.iter().position(|s| *s == stage).unwrap()]
}

/// The chain up to `gcboot` on a card whose graphics block has its power, as the two stages meet it.
pub fn loaded() -> (Rig, Session, Model) {
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcBoot));
    (rig, s, m)
}

#[test]
fn each_stage_equals_its_section_and_the_hub_gets_what_mmhub_got() {
    let (rig, mut s, mut m) = loaded();
    assert_eq!((s.next(), m.gc_awake(), m.gc.grbm), (Some(Stage::GcBoot), true, 0));
    m.writes.clear();
    let (done, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot, Stage::GcHub]);
    let r = regs(&rig);
    for d in &done {
        gcstage::equal(d, &r, s.nb.gart.as_ref()).unwrap_or_else(|e| panic!("{:?}: {e}\n{}", d.stage, d.lines.join("\n")));
        assert!(d.line.ends_with(": fertig.") && d.lines.last().unwrap() == &std::format!("linuxseq {}: equal", gcstage::section_of(d.stage).unwrap()), "{:?}", d.lines);
    }
    // The writes are the sections' and no more: 34, and 143 with the two HDP flushes' two.
    assert_eq!(done.iter().map(|d| d.totals.writes).collect::<Vec<_>>(), [34, 143 + 2]);
    assert_eq!(m.writes.len(), 34 + 145);
    // gc-boot's one rule is the autoload's loop (no difference here: the first round passed), gc-hub's are the
    // three addresses MMHUB was told, and without them the section has no rules at all.
    let told = gcstage::Told::of(s.nb.gart.as_ref().unwrap());
    assert!(matches!(gcstage::linux_rules("gc-boot", None).as_deref(), Some([Rule::PairPoll(_)])));
    assert_eq!(gcstage::linux_rules("gc-hub", Some(&told)).map(|r| r.len()), Some(3));
    assert!(gcstage::linux_rules("gc-hub", None).is_none());
    // What GFXHUB was told is what MMHUB was told: one table, one default page, one dummy page.
    let n = n(&rig);
    let mut same = 0;
    for (g, mm) in [(GC.ctx0_base_lo, MM.ctx0_base_lo), (GC.ctx0_base_hi, MM.ctx0_base_hi), (GC.default_addr_lsb, MM.default_addr_lsb), (GC.default_addr_msb, MM.default_addr_msb), (GC.fault_default_lo, MM.fault_default_lo), (GC.fault_default_hi, MM.fault_default_hi), (GC.ctx0_end_lo, MM.ctx0_end_lo), (GC.aperture_high, MM.aperture_high)] {
        assert_eq!(m.reg_read(n.byte(g).unwrap()), m.reg_read(n.byte(mm).unwrap()), "{}", g.name);
        same += 1;
    }
    let g = s.nb.gart.as_ref().unwrap();
    assert_eq!((same, m.reg_read(n.byte(GC.ctx0_base_lo).unwrap())), (8, Some(g.table.start as u32 | 1)));
    assert!(s.nb.gart.as_ref().unwrap().bound.len() == 1, "nothing was bound or planned by either stage");
    // The session: both passed, the chain stands, and a flush after a bind now names both hubs.
    assert!(s.passed(Stage::GcBoot) && s.passed(Stage::GcHub) && s.next() == Stage::CHAIN.iter().skip_while(|x| **x != Stage::GcHub).nth(1).copied());
    assert!(s.flush_regs(&n).unwrap().gc.is_some() && s.summary().contains("graphics block started (autoload complete after 1 rounds, GB_ADDR_CONFIG 0x8200545, engines halted), its hub on"));
    // A second run of either is refused before any access.
    for st in [Stage::GcBoot, Stage::GcHub] {
        let writes = m.writes.len();
        let (d, back) = rig.run(&mut s, st, m, &mut Stick::full());
        m = back;
        assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && m.writes.len() == writes && d.lines.last().unwrap().contains("a second run is refused"), "{:?}", d.lines);
    }
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

#[test]
fn the_block_answers_nothing_and_takes_nothing_before_power_and_autoload() {
    let rig = Rig::new();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    let known = [gc12::CP_ME_CNTL, gc12::CP_MEC_RS64_CNTL, gc12::DB_MEM_CONFIG, gc12::GB_ADDR_CONFIG, gc12::CP_DEBUG, GC.context0_cntl, GC.l1_tlb, GC.fault_cntl, GC.fault_status, gc12::CP_STAT, gc12::RLC_RLCS_BOOTLOAD_STATUS];
    // No power: the cold card; the card with its firmware loaded and the power controller not started (the
    // chain's state before `pmfw`); and a card whose power controller lost its features behind the driver's
    // back (the model's one power answer is the power controller's model, `Model::gc_powered`).
    let (_, mut unstarted) = rig.pass(&mut Session::default(), model(), &mut Stick::full(), chain_before(Stage::Pmfw));
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcBoot));
    assert!(m.gc_powered() && m.gc_awake() && s.pmfw.gc_powered());
    m.pmfw.enabled = false;
    for m in [&mut model(), &mut unstarted, &mut m] {
        // A fault status the block holds is not answered either, and such a read is no read of the status.
        m.gc.fault_status[0] = 0x123;
        assert!(known.iter().all(|r| m.reg_read(b(*r)) == Some(0)) && !m.gc_awake() && m.gc.fault_reads[0] == 0);
        m.gc.fault_status[0] = 0;
        refused(m, b(gc12::GRBM_GFX_CNTL), 0, "before the graphics block has power and its autoload is complete");
        refused(m, b(GC.ctx0_base_lo), 1, "GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32 written 0x1 before the graphics block has power");
    }
    // The stage on it: Linux's loop runs out after 100000 rounds of two reads 1 us apart and nothing was written.
    let (writes, before) = (m.writes.len(), m.card.now_us);
    m.refused.clear();
    let (d, mut m) = rig.run(&mut s, Stage::GcBoot, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("angehalten: der Grafikteil der Karte ist nicht gestartet"), "{}", d.line);
    assert!(d.lines.iter().any(|l| l.starts_with("stopped: rlc autoload: gc ucode autoload timeout after 100000 rounds of two reads 1 us apart (last CP_STAT 0x0, RLC_RLCS_BOOTLOAD_STATUS 0x0)")), "{:?}", d.lines.last());
    assert_eq!((d.trail.len() as u64 + d.trail_lost, m.card.now_us - before, m.writes.len(), d.totals.writes), (200_000, 100_000, writes, 0));
    // A trail that long lost accesses: the stage's line says it proves nothing.
    assert!(d.trail_lost > 0 && d.lines.iter().any(|l| l.starts_with("linuxseq gc-boot: differs: gc-boot: the stage's trail lost ")), "{:?}", d.lines.iter().rev().take(2).collect::<Vec<_>>());
    assert!(!s.passed(Stage::GcBoot) && m.refused.is_empty());
    // Power, but the autoload not complete: the two registers of the wait answer, nothing else does.
    let mut s2 = Session::default();
    let (_, mut early) = rig.pass(&mut s2, model(), &mut Stick::full(), chain_before(Stage::Autoload));
    early.pmfw.enabled = true;
    assert_eq!((early.reg_read(b(gc12::CP_STAT)), early.reg_read(b(gc12::RLC_RLCS_BOOTLOAD_STATUS)), early.reg_read(b(gc12::CP_ME_CNTL))), (Some(0x8000_0000), Some(0), Some(0)));
    refused(&mut early, b(gc12::DB_MEM_CONFIG), 0x8035, "before the graphics block has power and its autoload is complete");
    // Power arrives: the same session's stage passes, and the block answers as it answered Linux.
    m.pmfw.enabled = true;
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot]);
    assert_eq!((m.reg_read(b(gc12::RLC_RLCS_BOOTLOAD_STATUS)), m.reg_read(b(gc12::CP_ME_CNTL)), m.reg_read(b(gc12::CP_MEC_RS64_CNTL)), m.reg_read(b(gc12::DB_MEM_CONFIG))), (Some(BOOTLOAD_DONE), Some(0x1500_A000), Some(0x4000_0000), Some(0x8035)));
    // The halts stand: nothing this wave does unhalts an engine.
    assert_eq!(m.reg_read(b(gc12::CP_ME_CNTL)).unwrap() & gc12::GFX_HALTS, gc12::GFX_HALTS);
}

/// An autoload that needs more rounds than Linux's one: the stage passes as Linux would, says the rounds, and the
/// whole stage still equals Linux's section, since the rounds before the last are the loop's own
/// (`Rule::PairPoll`): every access after the loop is compared.
#[test]
fn an_autoload_that_takes_longer_passes_and_still_equals_linux() {
    let (rig, mut s, mut m) = loaded();
    m.gc.not_yet = 3;
    let (d, m) = rig.run(&mut s, Stage::GcBoot, m, &mut Stick::full());
    assert!(d.pass && d.trail_lost == 0 && s.gc.boot.is_some_and(|b| b.rounds == 4), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.contains("at round 4 of Linux's loop")), "{:?}", d.lines);
    assert_eq!(d.lines.last().unwrap(), "linuxseq gc-boot: equal");
    gcstage::equal(&d, &regs(&rig), None).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(d.trail.len(), 6 + 41, "three rounds of two reads before Linux's 41 lines");
    assert!(m.refused.is_empty() && m.gc.pulses == [1, 1, 1]);
    // A block that answers all ones while it comes up is "not yet" too, never "gone".
    struct Ones(Model, u32);
    impl Hw for Ones {
        fn reg_read(&mut self, byte: u32) -> Option<u32> {
            if self.1 > 0 && (byte == self.0.regs.cp_stat || byte == self.0.regs.bootload) {
                self.1 -= 1;
                return Some(u32::MAX);
            }
            self.0.reg_read(byte)
        }
        fn reg_write(&mut self, byte: u32, value: u32) -> bool {
            self.0.reg_write(byte, value)
        }
        fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
            self.0.mem_read(mc, buf)
        }
        fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
            self.0.mem_write(mc, data)
        }
        fn now_us(&mut self) -> u64 {
            self.0.now_us()
        }
        fn delay_us(&mut self, us: u64) {
            self.0.delay_us(us)
        }
    }
    impl Host for Ones {}
    let (rig, mut s, m) = loaded();
    let (d, ones) = rig.run_on(&mut s, Stage::GcBoot, Ones(m, 10), &mut Stick::full());
    assert!(d.pass && s.gc.boot.is_some_and(|b| b.rounds == 6) && ones.0.refused.is_empty(), "{:?}", d.lines);
    assert_eq!(d.lines.last().unwrap(), "linuxseq gc-boot: equal");
}

#[test]
fn the_model_refuses_what_linux_would_not_do_to_the_engines() {
    let (rig, _, mut m) = loaded();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    let (grbm, me_cntl, mec_cntl) = (b(gc12::GRBM_GFX_CNTL), b(gc12::CP_ME_CNTL), b(gc12::CP_MEC_RS64_CNTL));
    let (pfp, mec) = (b(gc12::CP_PFP_PRGRM_CNTR_START), b(gc12::CP_MEC_RS64_PRGRM_CNTR_START));
    // A selection Linux never makes.
    // The VMID selections (0x10..0xf0) are the command processor's constants' (`fakecp`), not this wave's.
    for v in [2, 3, 8, 0x14, 0x104] {
        refused(&mut m, grbm, v, "Linux selects pipe 0 and 1 of ME 0 and pipe 0 to 3 of ME 1");
    }
    // A reset pulse before the engine's starts are written on every pipe.
    refused(&mut m, me_cntl, 0x151C_A000, "a reset pulse of an engine whose program counter start is not written on every pipe");
    assert!(m.reg_write(pfp, 0xC00) && m.reg_write(b(gc12::CP_PFP_PRGRM_CNTR_START_HI), 0x1_C000));
    refused(&mut m, me_cntl, 0x151C_A000, "not written on every pipe");
    // A program counter under the wrong selection: MEC's under ME 0, PFP's and ME's under ME 1.
    refused(&mut m, mec, 0xC00, "CP_MEC_RS64_PRGRM_CNTR_START written while GRBM_GFX_CNTL selects 0x0: Linux writes it under 0x4 to 0x7 only");
    assert!(m.reg_write(grbm, 4));
    refused(&mut m, pfp, 0xC00, "CP_PFP_PRGRM_CNTR_START written while GRBM_GFX_CNTL selects 0x4: Linux writes it under 0x0 to 0x1 only");
    refused(&mut m, b(gc12::CP_ME_PRGRM_CNTR_START_HI), 0x1_C000, "Linux writes it under 0x0 to 0x1 only");
    // A pipe left selected: every other graphics register refuses until pipe 0 is selected again.
    refused(&mut m, mec_cntl, 0x400F_0000, "CP_MEC_RS64_CNTL written while GRBM_GFX_CNTL still selects 0x4: Linux selects pipe 0 again first");
    refused(&mut m, b(gc12::DB_MEM_CONFIG), 0x8035, "still selects 0x4");
    refused(&mut m, b(GC.ctx0_base_lo), 1, "still selects 0x4");
    assert_eq!(m.gc.grbm, 4);
    assert!(m.reg_write(grbm, 0));
    // A write that clears a HALT bit is the command processor's unhalt (`fakecp`): only in Linux's form, and
    // not before the engines' starts are written and pulsed in on every pipe.
    refused(&mut m, me_cntl, 0x1114_A000, "Linux's unhalt writes 0x114a000");
    refused(&mut m, me_cntl, 0x0514_A000, "Linux's unhalt writes 0x114a000");
    refused(&mut m, mec_cntl, 0x0003_0000, "Linux's unhalt writes 0x3c000000");
    refused(&mut m, me_cntl, 0x0114_A000, "an unhalt of an engine whose program counter start is not written and pulsed in on every pipe");
    refused(&mut m, mec_cntl, 0x3C00_0000, "an unhalt of an engine whose program counter start is not written and pulsed in on every pipe");
    refused(&mut m, me_cntl, 0x1414_A000, "CE_HALT cleared; GFX 12 has no constant engine");
    // Nothing but the pipes' reset bits otherwise.
    refused(&mut m, me_cntl, 0x1514_A010, "Linux changes only the pipes' reset bits here");
    refused(&mut m, mec_cntl, 0x4003_0010, "Linux changes only the pipes' reset bits here");
    // The golden register and CP_DEBUG take Linux's one change; GB_ADDR_CONFIG is only read.
    refused(&mut m, b(gc12::DB_MEM_CONFIG), 0x8034, "Linux's golden setting sets bit 15 and nothing else");
    refused(&mut m, b(gc12::CP_DEBUG), 0x2040_8001, "Linux sets CPG_UTCL1_ERROR_HALT_DISABLE and nothing else");
    refused(&mut m, b(gc12::GB_ADDR_CONFIG), 0x0820_0545, "Linux only reads it here");
    assert!(m.reg_write(b(gc12::DB_MEM_CONFIG), 0x8035) && m.reg_write(b(gc12::CP_DEBUG), 0x2040_8000));
    assert_eq!(m.gc.pulses, [0, 0, 0]);
}

/// A card that swallows one write of a register (it answers as if it took it).
struct Swallow {
    m: Model,
    byte: u32,
    nth: usize,
    seen: usize,
}

impl Hw for Swallow {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.m.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.seen += usize::from(byte == self.byte);
        (byte == self.byte && self.seen == self.nth) || self.m.reg_write(byte, value)
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

impl Host for Swallow {}

/// The model keeps GRBM_GFX_CNTL's selection, so a stage whose return to pipe 0 does not arrive is caught at the
/// very next write, after each of the three loops.
#[test]
fn a_selection_left_on_a_pipe_is_caught() {
    for (nth, left, at) in [(3, 1, "CP_ME_CNTL"), (6, 1, "CP_ME_CNTL"), (11, 7, "CP_MEC_RS64_CNTL")] {
        let (rig, mut s, m) = loaded();
        let byte = n(&rig).byte(gc12::GRBM_GFX_CNTL).unwrap();
        let (d, card) = rig.run_on(&mut s, Stage::GcBoot, Swallow { m, byte, nth, seen: 0 }, &mut Stick::full());
        assert!(!d.pass && d.line.contains("angehalten: ein Schreibzugriff war nicht erlaubt"), "{nth}: {}", d.line);
        assert_eq!((card.m.gc.grbm, card.m.refused.len()), (left, 1), "{nth}: {:?}", card.m.refused);
        assert!(card.m.refused[0].starts_with(&std::format!("{at} written")) && card.m.refused[0].contains(&std::format!("still selects {left:#x}")), "{:?}", card.m.refused);
        // The stage's own line names the write that did not reach the card.
        let said = d.lines.iter().find(|l| l.starts_with("linuxseq ")).unwrap();
        assert!(!s.passed(Stage::GcBoot) && said.starts_with("linuxseq gc-boot: differs: gc-boot.txt line ") && said.contains(&std::format!("to {at}")), "{said}");
        // The stage did not pass. While the pipe stays selected the model takes nothing else, the stage's first
        // write included (Linux's sequence selects only where a loop begins); with pipe 0 selected again the
        // stage runs from its start and every write arrives.
        let mut m = card.m;
        m.refused.clear();
        let (again, mut m) = rig.run(&mut s, Stage::GcBoot, m, &mut Stick::full());
        assert!(!again.pass && m.refused.len() == 1 && m.refused[0].starts_with("DB_MEM_CONFIG written") && m.refused[0].contains("still selects"), "{:?}", m.refused);
        m.refused.clear();
        assert!(m.reg_write(byte, 0));
        let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot]);
        assert!(s.passed(Stage::GcBoot) && m.gc.grbm == 0 && m.gc.pulses.iter().all(|p| *p >= 1), "{:?}", m.gc.pulses);
    }
}

#[test]
fn the_model_refuses_what_linux_would_not_do_to_gfxhub() {
    let (rig, mut s, m) = loaded();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    let eng = |r| b(member(r, hub41::GART_ENGINE, 1));
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot]);
    let (cntl, req, ack) = (b(GC.context0_cntl), eng(GC.eng0_req), eng(GC.eng0_ack));
    // Context 0 before its base and range; five of the six halves are not enough.
    refused(&mut m, cntl, 0x03FF_FC01, "GFXHUB's context 0 enabled before its page table base and range were written");
    let table = s.nb.gart.as_ref().unwrap().table.start;
    let halves = [(GC.ctx0_base_lo, table as u32 | 1), (GC.ctx0_base_hi, (table >> 32) as u32), (GC.ctx0_start_lo, 0), (member(GC.ctx0_start_lo, 1, 1), 0), (GC.ctx0_end_lo, 0x1_FFFF), (member(GC.ctx0_end_lo, 1, 1), 0)];
    for (r, v) in &halves[..5] {
        assert!(m.reg_write(b(*r), *v));
    }
    refused(&mut m, cntl, 0x03FF_FC01, "enabled before its page table base and range");
    assert!(m.reg_write(b(halves[5].0), 0));
    // An invalidation before context 0 is on.
    refused(&mut m, req, hub41::invalidate_req(0), "an invalidation request on GFXHUB before its context 0 is on");
    // Another table than the one MMHUB runs on.
    assert!(m.reg_write(b(GC.ctx0_base_lo), (table + 0x1000) as u32 | 1));
    refused(&mut m, cntl, 0x03FF_FC01, "which is not the GART table MMHUB runs on");
    assert!(m.reg_write(b(GC.ctx0_base_lo), table as u32 | 1) && m.reg_write(cntl, 0x03FF_FC01) && m.gc.context0);
    // Only Linux's request, and its acknowledge answers that one request (bit 0 alone, where MMHUB's reads 0x10001).
    refused(&mut m, req, 0x00F8_0002, "GFXHUB invalidation request 0xf80002: Linux flushes VMID 0 with 0xf80001");
    refused(&mut m, req, 0x00FA_0001, "Linux flushes VMID 0 with 0xf80001");
    assert_eq!(m.reg_read(ack), Some(0), "a refused request is not acknowledged");
    assert!(m.reg_write(req, hub41::invalidate_req(0)));
    assert_eq!((m.reg_read(ack), m.reg_read(ack), m.gc.tlb_flushes), (Some(1), Some(0), 1));
    // GFXHUB has no semaphore in Linux's flush (gmc_v12_0.c:189-194): the model knows no such write.
    refused(&mut m, eng(GC.eng0_sem), 0, "a register G2 does not know");
    // A system address before the DMAR survey said yes: a card whose survey was never asked.
    let mut fresh = model();
    (fresh.cp_stat, fresh.bootload, fresh.pmfw.enabled) = (0, BOOTLOAD_DONE, true);
    refused(&mut fresh, b(GC.fault_default_lo), 0x6_3FFF, "a system address handed to GFXHUB before the DMAR survey allowed it");
    // The stage on a hub that was set up by hand passes all the same: it writes everything again.
    m.refused.clear();
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcHub]);
    assert!(d[0].lines.last().unwrap().starts_with("linuxseq gc-hub: equal"), "{:?}", d[0].lines.last());
    assert!(m.refused.is_empty());
}

/// The acknowledge that never comes: Linux logs the wait and goes on, so does the stage.
#[test]
fn gfxhubs_flush_that_is_not_acknowledged_is_logged_and_passed() {
    let (rig, mut s, mut m) = loaded();
    m.gc.ack_never = true;
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot, Stage::GcHub]);
    assert!(d[1].lines.iter().any(|l| l.contains("the wait for the acknowledge of the graphics hub ran out after 100000 reads")), "{:?}", d[1].lines);
    let line = d[1].lines.last().unwrap();
    assert!(line.starts_with("linuxseq gc-hub: equal; 1 answers of the card are not the trace's, the first: gc-hub.txt line "), "{line}");
    assert!(s.gc.hub && m.gc.tlb_flushes == 1);
}

/// On the second card nothing of either stage happens, and a machine whose survey says no gets no hub.
#[test]
fn the_second_card_and_a_machine_that_says_no() {
    let mut rig = Rig::new();
    rig.display = false;
    for st in [Stage::GcBoot, Stage::GcHub] {
        let (d, m) = rig.run(&mut Session::default(), st, model(), &mut Stick::full());
        assert!(!d.pass && d.line.contains("die zweite Karte bleibt unberührt") && d.trail.is_empty() && m.writes.is_empty(), "{}", d.line);
    }
    let (rig, mut s, m) = loaded();
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot]);
    m.nb.survey = Err("TranslationOn");
    let writes = m.writes.len();
    let (d, m) = rig.run(&mut s, Stage::GcHub, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("der Rechner lässt die Karte nicht an den Arbeitsspeicher") && d.trail.is_empty() && m.writes.len() == writes, "{}", d.line);
    // A stick without one of the three files ends gcboot before the card is touched.
    let (rig, mut s, m) = loaded();
    let mut stick = Stick::full();
    stick.0.retain(|k, _| !k.ends_with("gc_12_0_1_mec.bin"));
    let writes = m.writes.len();
    let (d, m) = rig.run(&mut s, Stage::GcBoot, m, &mut stick);
    assert!(!d.pass && d.line.contains("eine Datei fehlt auf dem Stick") && d.trail.is_empty() && m.writes.len() == writes, "{}", d.line);
}
