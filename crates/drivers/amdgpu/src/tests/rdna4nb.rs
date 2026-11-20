//! Wave 42: the stages `nbif`, `gmc` and `ih` on the model (`rdna4::fakenb`). Each equals its section of Linux's
//! trace with only the declared address rules; out of order, a second time or on the second card nothing is
//! written; the flush moves to its remapped place; a wait that runs out is logged and passed, as Linux does.

use super::rdna4fix::*;
use crate::gpucore::trail::Access;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::guard::Refusal;
use crate::rdna4::nbseq;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::stage::{Done, Session, Stage};
use rdna4_core::{ih7, layout, nbreg, pte12};
use std::vec::Vec;

pub fn regs(rig: &Rig) -> Regs14 {
    Regs14::from(&rig.att.discovery).unwrap()
}

/// The three stages in Linux's order; panics with the stage's lines when one does not pass.
pub fn up(rig: &Rig, s: &mut Session, mut m: Model) -> (Vec<Done>, Model) {
    let mut done = Vec::new();
    for st in NB {
        let (d, back) = rig.run(s, st, m, &mut Stick::full());
        m = back;
        assert!(d.pass && d.refused.is_empty(), "{st:?}: {}\n{}", d.line, d.lines.join("\n"));
        done.push(d);
    }
    (done, m)
}

#[test]
fn each_stage_equals_its_section_of_linuxs_trace() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (done, m) = up(&rig, &mut s, model());
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    let r = regs(&rig);
    for d in &done {
        assert_eq!(d.trail_lost, 0);
        nbseq::equal(d, &r, rig.att.vram_bytes).unwrap_or_else(|e| panic!("{:?}: {e}\n{}", d.stage, d.lines.join("\n")));
        assert!(d.line.ends_with(": fertig."), "{}", d.line);
    }
    // The writes are the sections' and no more: 5 through 4 + 2 * 3 bus writes (two index-pair writes), 144, and
    // 32 with the bind's flush (two HDP flushes, the request, the semaphore's release, the private invalidation).
    assert_eq!(done.iter().map(|d| d.totals.writes).collect::<Vec<_>>(), [3 + 2 * 2 + 2, 144 + 2, 32 + 5]);
    // Nothing of nbif is an address BitsOS chooses: no rule at all.
    assert!(nbseq::rules(Stage::Nbif, rig.att.vram_bytes).is_empty());
    assert_eq!((nbseq::rules(Stage::Gmc, 1).len(), nbseq::rules(Stage::Ih, 1).len()), (3, 5));
}

#[test]
fn what_the_stages_leave_behind() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (done, mut m) = up(&rig, &mut s, model());
    // nbif: both registers past BAR5 through the index pair, the remap, the aperture.
    assert_eq!((m.nb.strap2, m.nb.df, m.nb.remapped), (0x0313_2000, 0x0A41_0031, [true, true]));
    assert!(s.nb.nbif);
    // gmc: the table and the default page come from the VRAM manager, outside every kept range; Linux's fill.
    let g = s.nb.gart.as_ref().expect("the GART is on");
    let l = s.layout.as_ref().unwrap();
    assert_eq!((g.l.vram_start, g.l.vram_bytes, g.l.gart_start, g.l.gart_bytes), (0x80_0000_0000, 16304 << 20, 0, 512 << 20));
    assert_eq!(g.table.end - g.table.start, 1 << 20);
    assert!(g.table.start >= l.window.end && g.scratch.start >= g.table.end && g.scratch.end <= l.vram_bytes / 2, "{:?} {:?}", g.table, g.scratch);
    let bump = s.bump.as_ref().unwrap();
    assert_eq!(bump.taken.iter().map(|t| t.0).collect::<Vec<_>>(), ["GART table", "default page"]);
    assert_eq!(m.nb.table(), Some(g.table.start));
    assert!(m.nb.context0 && m.nb.survey_asked);
    // An entry the stage never bound still holds Linux's fill; the bound ones are GFX 12's.
    let entry = |m: &mut Model, i: u64| {
        let mut e = [0u8; 8];
        assert!(crate::gpucore::psp::Hw::mem_read(m, g.table.start + i * 8, &mut e));
        u64::from_le_bytes(e)
    };
    assert_eq!(entry(&mut m, 0), 0x1010_1010_1010_1010, "page 0 stays unbound");
    assert_eq!(entry(&mut m, 4000), 0x1010_1010_1010_1010);
    // ih: both rings and the write-back page bound from page 1 on, 129 pages of one piece of system memory.
    let ih = s.nb.ih.as_ref().expect("the IH is on");
    assert_eq!((ih.place.ring_mc, ih.place.wb_mc), ([0x1000, 0x1000 + (256 << 10)], 0x1000 + (512 << 10)));
    assert_eq!(g.bound.len(), 1);
    assert_eq!((g.bound[0].0, g.bound[0].1, g.bound[0].2.len()), ("IH rings", 1, 129));
    for i in [0u64, 64, 128] {
        assert_eq!(entry(&mut m, 1 + i), pte12::bound(ih.place.sys + i * 4096).unwrap());
    }
    assert_eq!(g.system_of(ih.place.wb_mc + 12), Some(ih.place.sys + (512 << 10) + 12));
    assert_eq!(m.nb.ih_on, [true, true]);
    assert_eq!(m.nb.host_changes, ["AtomicOp requester enable", "bus master on", "MSI vector"]);
    assert!(m.nb.atomics_req && done[0].lines[0].contains("nbif: AtomicOps to the root port granted"), "{:?}", done[0].lines);
    assert_eq!(m.nb.tlb_flushes, 2, "gmc's first flush and the bind's");
    // Each table write was followed by a store barrier that reached the card through the owner, the guard and
    // the MC translation: gmc's fill, ih's bind.
    assert_eq!(m.nb.barriers, 2);
    // The answers name what Georg's round compares: each wait with its reads.
    assert_eq!(done[1].lines.iter().filter(|l| l.starts_with("WAIT ")).count(), 2);
    assert!(done[1].lines.iter().any(|l| l.contains("# MMVM_INVALIDATE_ENG17_SEM until (value & 0x00000001) == 0x00000001: 1 reads")), "{:?}", done[1].lines);
    assert!(done[0].lines.iter().any(|l| l.contains("# BIF_BX0_REMAP_HDP_MEM_FLUSH_CNTL")), "{:?}", done[0].lines);
}

#[test]
fn out_of_order_and_a_second_run_write_nothing() {
    let rig = Rig::new();
    // gmc without nbif, ih without gmc (with and without nbif).
    for (before, st) in [(&[][..], Stage::Gmc), (&[][..], Stage::Ih), (&[Stage::Nbif][..], Stage::Ih)] {
        let mut s = Session::default();
        let mut m = model();
        for b in before {
            let (d, back) = rig.run(&mut s, *b, m, &mut Stick::full());
            assert!(d.pass);
            m = back;
        }
        let (writes, host) = (m.writes.len(), m.nb.host_changes.len());
        let (d, m) = rig.run(&mut s, st, m, &mut Stick::full());
        // The chain's one check names the stage that is missing: gmc's `nbif`, ih's `gmc`.
        let first = if st == Stage::Gmc { "angehalten: zuerst den Anschluss der Karte einrichten." } else { "angehalten: zuerst den Speicher der Karte einrichten." };
        assert!(!d.pass && d.line.ends_with(first), "{st:?} after {before:?}: {}", d.line);
        assert_eq!(s.before(st).map(|(prev, passed)| (prev.name(), passed)), Some((if st == Stage::Gmc { "nbif" } else { "gmc" }, false)));
        assert_eq!((d.totals.writes, m.writes.len() - writes, d.trail.len()), (0, 0, 0), "{st:?}: nothing reached the card, not even a read");
        assert!(m.refused.is_empty() && m.nb.host_changes.len() == host && m.nb.sys_given.is_empty());
    }
    // Each stage a second time.
    let mut s = Session::default();
    let (_, mut m) = up(&rig, &mut s, model());
    for st in NB {
        let (writes, host) = (m.writes.len(), m.nb.host_changes.len());
        let (d, back) = rig.run(&mut s, st, m, &mut Stick::full());
        m = back;
        assert!(!d.pass && d.line.contains("schon"), "{st:?}: {}", d.line);
        assert_eq!((d.totals.writes, m.writes.len() - writes, m.nb.host_changes.len() - host), (0, 0, 0), "{st:?}");
    }
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    assert_eq!(s.bump.as_ref().unwrap().taken.len(), 2, "a refused second gmc took nothing more");
}

#[test]
fn the_second_card_answers_with_zero_writes() {
    let mut rig = Rig::new();
    rig.display = false;
    for st in NB {
        let mut s = Session::default();
        let (d, m) = rig.run(&mut s, st, model(), &mut Stick::full());
        assert!(!d.pass && d.line.contains("die zweite Karte bleibt unberührt"), "{st:?}: {}", d.line);
        assert_eq!((d.totals.writes, m.writes.len(), d.trail.len()), (0, 0, 0), "{st:?}");
        assert!(m.nb.host_changes.is_empty() && m.nb.sys_given.is_empty() && !m.nb.survey_asked && !m.nb.atomics_req);
        assert!(s.layout.is_none() && s.bump.is_none() && !s.nb.nbif && s.nb.plan.is_none());
    }
}

#[test]
fn after_nbif_every_flush_goes_to_the_remapped_place() {
    let rig = Rig::new();
    let r = regs(&rig);
    let flushes = |d: &Done| -> Vec<u32> {
        d.trail.windows(2).filter_map(|w| match (w[0], w[1]) { (Access::Write { byte, value: 0, reached: true }, Access::Read { byte: m, .. }) if m == r.memsize => Some(byte), _ => None }).collect()
    };
    // Linux's place, for this wave's stages and for the secure processor's alike (two in `gmc`, two in `ih`'s
    // bind, one per bootloader part, one per ring command); the model refuses the register itself.
    let mut s = Session::default();
    let (done, mut m) = up(&rig, &mut s, model());
    let mut seen: Vec<u32> = done.iter().flat_map(flushes).collect();
    assert_eq!(seen.len(), 2 + 2);
    for st in [Stage::Sos, Stage::Ring, Stage::Toc] {
        let (d, back) = rig.run(&mut s, st, m, &mut Stick::full());
        m = back;
        assert!(d.pass, "{st:?}: {:?}", d.lines);
        seen.extend(flushes(&d));
    }
    assert_eq!(seen, [nbreg::REMAPPED_MEM_FLUSH_BYTE; 4 + 9 + 2 + 1]);
    assert_eq!((r.hdp_flush, m.flushes), (nbreg::REMAPPED_MEM_FLUSH_BYTE, 16));
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    assert!(!crate::gpucore::psp::Hw::reg_write(&mut m, r.hdp_direct, 0));
    assert_eq!(m.refused.len(), 1);
    // The ring's buffers and this wave's lie apart: one manager for both.
    let (bump, g) = (s.bump.as_ref().unwrap(), s.nb.gart.as_ref().unwrap());
    assert!(!bump.given.is_empty() && bump.given.iter().all(|r| r.end <= g.table.start));
    // The firmware's region the ring stage learned is kept now: above half of VRAM is free for what follows.
    assert!(bump.kept().iter().any(|(k, r)| *k == crate::rdna4::vram::Kept::Firmware && r.end == 16304 << 20));
}

#[test]
fn a_wait_that_runs_out_is_logged_and_passed_as_linux_does() {
    let rig = Rig::new();
    for (sem, ack) in [(true, false), (false, true)] {
        let mut s = Session::default();
        let (d, m) = rig.run(&mut s, Stage::Nbif, model(), &mut Stick::full());
        assert!(d.pass);
        let mut m = m;
        (m.nb.sem_never, m.nb.ack_never) = (sem, ack);
        let (d, m) = rig.run(&mut s, Stage::Gmc, m, &mut Stick::full());
        assert!(d.pass, "{}: {:?}", d.line, d.lines);
        let what = if sem { "semaphore" } else { "acknowledge" };
        assert!(d.lines.iter().any(|l| l.contains(&std::format!("the wait for the {what} ran out after 100000 reads"))), "{:?}", d.lines);
        let waits: Vec<u32> = d.trail.iter().filter_map(|a| if let Access::Wait { reads, .. } = a { Some(*reads) } else { None }).collect();
        assert_eq!(waits, if sem { [100_000, 1] } else { [1, 100_000] });
        // Bounded by reads, not by a clock: 100000 reads 1 us apart.
        assert!(m.card.now_us >= 100_000 && m.card.now_us < 200_000, "{}", m.card.now_us);
        assert!(m.refused.is_empty(), "{:?}", m.refused);
    }
}

#[test]
fn the_guard_lists_are_the_sections_writes_and_nothing_else() {
    use crate::rdna4::nb;
    use crate::rdna4::nbregs::NbRegs;
    let rig = Rig::new();
    let r = regs(&rig);
    let n = NbRegs::from(&rig.att.discovery, &r).unwrap();
    let mut s = Session::default();
    let (done, _) = up(&rig, &mut s, model());
    for (d, st) in done.iter().zip(NB) {
        // Every register on the stage's list was written by it, and nothing off the list reached the card.
        let before = if st == Stage::Nbif { nb::State::new() } else { nb::State { remapped: true, nbif: true, ..nb::State::new() } };
        let list = nb::allow(st, &before, &n, |r| r.clone());
        let written: Vec<u32> = d.trail.iter().filter_map(|a| if let Access::Write { byte, reached: true, .. } = a { Some(*byte) } else { None }).collect();
        for b in &list.regs {
            assert!(written.contains(b), "{st:?} lists {b:#x} and never writes it");
        }
        assert!(written.iter().all(|b| list.regs.contains(b)), "{st:?}");
        assert!(d.refused.iter().all(|x| !matches!(x, Refusal::Reg { .. })));
    }
    // Sizes as Linux's: both rings 256 KiB, the GART's table one entry per page.
    assert_eq!((ih7::RING_BYTES, crate::rdna4::gmcstage::TABLE_BYTES, layout::GART_BYTES / 4096), (256 << 10, 1 << 20, 131072));
}
