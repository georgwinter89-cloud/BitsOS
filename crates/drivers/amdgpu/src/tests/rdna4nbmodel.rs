//! Wave 42: what the model refuses (`rdna4::fakenb`), each thing Linux would not do, and how a stage stops when
//! the machine's side says no: before any register write.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakenb::SYS_BASE;
use crate::rdna4::host::Host;
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::nbseq;
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::ih7::{self, Ring};
use rdna4_core::nbreg::{self, member};
use rdna4_core::{hub41, pte12};

pub fn n(rig: &Rig) -> NbRegs {
    NbRegs::from(&rig.att.discovery, &regs(rig)).unwrap()
}

/// The model after `stages` passed.
pub fn after(rig: &Rig, stages: &[Stage]) -> (Session, Model) {
    let mut s = Session::default();
    let mut m = model();
    for st in stages {
        let (d, back) = rig.run(&mut s, *st, m, &mut Stick::full());
        assert!(d.pass, "{st:?}: {:?}", d.lines);
        m = back;
    }
    (s, m)
}

/// One write the model must refuse, with its reason.
pub fn refused(m: &mut Model, byte: u32, v: u32, why: &str) {
    let before = m.refused.len();
    assert!(!m.reg_write(byte, v), "{byte:#x} = {v:#x} was taken");
    assert!(m.refused.len() == before + 1 && m.refused[before].contains(why), "{:?}", &m.refused[before..]);
}

#[test]
fn context_0_needs_its_page_table_base_and_range_first() {
    let rig = Rig::new();
    let n = n(&rig);
    let cntl = n.byte(hub41::CONTEXT0_CNTL).unwrap();
    let mut m = model();
    refused(&mut m, cntl, 0x03FF_FC01, "context 0 enabled before its page table base and range");
    // Five of the six halves are not enough.
    for i in 0..5 {
        let r = [hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32, hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32, hub41::CONTEXT0_PAGE_TABLE_START_ADDR_LO32, member(hub41::CONTEXT0_PAGE_TABLE_START_ADDR_LO32, 1, 1), hub41::CONTEXT0_PAGE_TABLE_END_ADDR_LO32][i];
        assert!(m.reg_write(n.byte(r).unwrap(), 0));
    }
    refused(&mut m, cntl, 0x03FF_FC01, "context 0 enabled before");
    assert!(m.reg_write(n.byte(member(hub41::CONTEXT0_PAGE_TABLE_END_ADDR_LO32, 1, 1)).unwrap(), 0));
    assert!(m.reg_write(cntl, 0x03FF_FC01) && m.nb.context0);
    // Writing the control without the enable bit is no enable.
    let mut m = model();
    assert!(m.reg_write(cntl, 0x03FF_FC00) && !m.nb.context0 && m.refused.is_empty());
}

#[test]
fn an_invalidation_needs_the_semaphore_and_a_running_context() {
    let rig = Rig::new();
    let n = n(&rig);
    let eng = |r| n.byte(member(r, hub41::GART_ENGINE, 1)).unwrap();
    let (sem, req, ack) = (eng(hub41::INVALIDATE_ENG0_SEM), eng(hub41::INVALIDATE_ENG0_REQ), eng(hub41::INVALIDATE_ENG0_ACK));
    let (_, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    refused(&mut m, req, hub41::invalidate_req(0), "without the engine's semaphore");
    assert_eq!(m.reg_read(ack), Some(0), "the first flush's acknowledge went with its semaphore");
    assert_eq!(m.reg_read(sem), Some(1));
    refused(&mut m, req, 0x00F8_0002, "Linux flushes VMID 0");
    assert_eq!(m.reg_read(ack), Some(0), "a refused request is not acknowledged");
    assert!(m.reg_write(req, hub41::invalidate_req(0)));
    assert_eq!(m.reg_read(ack), Some(0x0001_0001));
    assert!(m.reg_write(sem, 0));
    // Released: the next request needs it again, and the acknowledge is gone until that request.
    assert_eq!(m.reg_read(ack), Some(0));
    refused(&mut m, req, hub41::invalidate_req(0), "without the engine's semaphore");
    refused(&mut m, sem, 1, "semaphore written");
    // Before the GART is on there is nothing to flush.
    let (_, mut m) = after(&rig, &[Stage::Nbif]);
    assert_eq!(m.reg_read(sem), Some(1));
    refused(&mut m, req, hub41::invalidate_req(0), "before context 0 is on");
}

#[test]
fn an_ih_ring_needs_base_size_bound_pages_and_a_card_that_may_send() {
    let rig = Rig::new();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    let on = ih7::toggled(Ring::Ring0, ih7::rb_cntl(Ring::Ring0, 0x4061_0000, true), true);
    // Enabled with nothing written.
    let (_, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    refused(&mut m, b(ih7::RB_CNTL), 0x4061_0001, "IH ring 0 enabled before its base and size were written");
    // Base and size written, but the base lies outside the GART (512 MiB at MC 0).
    assert!(m.reg_write(b(ih7::RB_BASE), ((512u64 << 20) >> 8) as u32) && m.reg_write(b(ih7::RB_BASE_HI), 0) && m.reg_write(b(ih7::RB_CNTL), on & !1));
    refused(&mut m, b(ih7::RB_CNTL), on, "not inside the GART");
    // In the GART, on pages nobody bound.
    assert!(m.reg_write(b(ih7::RB_BASE), 0x1000 >> 8));
    refused(&mut m, b(ih7::RB_CNTL), on, "its pages are not bound in the GART");
    // The stage's own ring is taken; with bus master off it is not.
    let (s, mut m) = after(&rig, &NB);
    assert!(m.refused.is_empty());
    let ih = s.nb.ih.as_ref().unwrap();
    assert!(m.reg_write(b(ih7::RB_CNTL), 0x4031_0120) && !m.nb.ih_on[0], "switched off again");
    m.nb.bus_master = false;
    refused(&mut m, b(ih7::RB_CNTL), 0x4033_0121, "while the card may not send");
    m.nb.bus_master = true;
    assert!(m.reg_write(b(ih7::RB_CNTL), 0x4033_0121) && m.nb.ih_on[0]);
    assert_eq!(m.nb.ring_mc(Ring::Ring0), ih.place.ring_mc[0]);
    // What reads back is the card's: the clear bit gone, ring 1 without bit 8 (the trace's 0x40310120, 0x40100220).
    assert!(m.reg_write(b(ih7::RB_CNTL), 0xC031_0120) && m.reg_write(b(ih7::RB_CNTL_RING1), 0xC010_0320));
    assert_eq!((m.reg_read(b(ih7::RB_CNTL)), m.reg_read(b(ih7::RB_CNTL_RING1))), (Some(0x4031_0120), Some(0x4010_0220)));
}

#[test]
fn nothing_of_the_system_before_the_survey_and_nothing_above_44_bits() {
    let rig = Rig::new();
    let n = n(&rig);
    // Bus master, the fault's page, the interrupt control's page: all before the survey was asked.
    let mut m = model();
    assert!(m.enable_dma().is_err() && m.refused.last().unwrap().contains("bus master before the DMAR survey"));
    refused(&mut m, n.byte(hub41::FAULT_DEFAULT_ADDR_LO32).unwrap(), 0x63FFF, "before the DMAR survey allowed it");
    refused(&mut m, n.byte(nbreg::INTERRUPT_CNTL2).unwrap(), 0x63_FFF0, "before the DMAR survey allowed it");
    // Asked, and the answer is no: still refused.
    m.nb.survey = Err("translation is on");
    assert!(m.dma_survey().is_err() && m.enable_dma().is_err());
    assert!(m.nb.host_changes.is_empty());
    // A GART entry: a system page before the survey, and one above the ceiling after it.
    let (s, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    let table = s.nb.gart.as_ref().unwrap().table.start;
    let high = (pte12::entry(1 << 44, pte12::BOUND_FLAGS)).to_le_bytes();
    assert!(!m.mem_write(table + 8 * 77, &high) && m.refused.last().unwrap().contains("GART entry 77 points at 0x100000000000, above the card's 44 bits"));
    let fine = pte12::bound(SYS_BASE).unwrap().to_le_bytes();
    assert!(m.mem_write(table + 8 * 77, &fine));
    m.nb.survey_asked = false;
    assert!(!m.mem_write(table + 8 * 78, &fine) && m.refused.last().unwrap().contains("before the DMAR survey allowed it"));
    // An entry that is not a valid system page is no hand-over: Linux's fill and an unbound entry pass.
    assert!(m.mem_write(table + 8 * 78, &[pte12::FILL_BYTE; 8]) && m.mem_write(table + 8 * 78, &pte12::unbound(1 << 44).to_le_bytes()));
}

#[test]
fn the_index_pair_the_flush_and_the_doorbells_only_as_linux_uses_them() {
    let rig = Rig::new();
    let n = n(&rig);
    let r = regs(&rig);
    let mut m = model();
    // The index pair: the two registers of the common hw_init, the SMU's flags, nothing else.
    refused(&mut m, n.index, n.strap2 + 4, "index pair aimed at");
    assert!(m.reg_write(n.index, n.strap2) && m.reg_read(n.data) == Some(0x0313_2000));
    assert!(m.reg_write(n.index, n.df) && m.reg_read(n.data) == Some(0x0A40_0001));
    // The flush: through the register itself before the remap, through 0x7f000 after it; the remap only to
    // Linux's place.
    refused(&mut m, nbreg::REMAPPED_MEM_FLUSH_BYTE, 0, "before the remap was written");
    assert!(m.reg_write(r.hdp_direct, 0));
    refused(&mut m, n.byte(nbreg::REMAP_HDP_MEM_FLUSH).unwrap(), 0x7F008, "Linux's remap is 0x7f000");
    assert!(m.reg_write(n.byte(nbreg::REMAP_HDP_MEM_FLUSH).unwrap(), 0x7F000) && m.reg_write(nbreg::REMAPPED_MEM_FLUSH_BYTE, 0));
    refused(&mut m, r.hdp_direct, 0, "after the remap");
    refused(&mut m, nbreg::REMAPPED_MEM_FLUSH_BYTE, 1, "HDP flush written 0x1");
    assert_eq!(m.flushes, 2);
    // Doorbells: only the two of the IH, and only once the aperture, the entry and the ring's doorbell stand.
    assert!(!m.doorbell_write32(ih7::DOORBELL_RING0, 0x20) && m.refused.last().unwrap().contains("before the aperture"));
    assert!(!m.doorbell_write32(0x10, 0) && m.refused.last().unwrap().contains("not an IH doorbell"));
    let (_, mut m) = after(&rig, &NB);
    assert!(m.doorbell_write32(ih7::DOORBELL_RING0, 0x20) && m.doorbell_write32(ih7::DOORBELL_RING1, 0x40));
    assert_eq!(m.nb.rptr, [0x20, 0x40]);
    assert!(!m.doorbell_write32(ih7::DOORBELL_RING0 + 1, 0) && !m.doorbell_write64(ih7::DOORBELL_RING0, 0));
}

#[test]
fn a_no_from_the_machines_side_stops_a_stage_before_any_register_write() {
    let rig = Rig::new();
    // gmc: the survey says the firmware left an IOMMU translating; or the page for the card lies too high.
    for case in 0..2 {
        let (mut s, mut m) = after(&rig, &[Stage::Nbif]);
        if case == 0 { m.nb.survey = Err("TranslationOn") } else { m.nb.next_page = 1 << 40 }
        let writes = m.writes.len();
        let (d, m) = rig.run(&mut s, Stage::Gmc, m, &mut Stick::full());
        assert!(!d.pass && d.line.contains(if case == 0 { "lässt die Karte nicht an den Arbeitsspeicher" } else { "liegt zu hoch" }), "{}", d.line);
        assert_eq!((d.totals.writes, m.writes.len() - writes), (0, 0));
        assert!(m.refused.is_empty() && s.nb.gart.is_none() && !m.nb.context0);
        // The plan's two ranges are kept for the next try, and a next try takes no more.
        assert_eq!(s.bump.as_ref().unwrap().taken.len(), 2);
        let mut m = m;
        (m.nb.survey, m.nb.next_page) = (Ok(()), SYS_BASE);
        let (d, m) = rig.run(&mut s, Stage::Gmc, m, &mut Stick::full());
        assert!(d.pass && m.refused.is_empty(), "{:?}", d.lines);
        assert_eq!(s.bump.as_ref().unwrap().taken.len(), 2);
        nbseq::equal(&d, &regs(&rig), rig.att.vram_bytes).unwrap_or_else(|e| panic!("{e}"));
    }
    // ih: a bridge that does not forward, a PCI core without a vector, a survey that turned to no.
    for case in 0..3 {
        let (mut s, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
        match case {
            0 => m.nb.bridges = Err("the bridge 0000:03:00.0 has bus master off"),
            1 => m.nb.msi_refused = true,
            _ => m.nb.survey = Err("TranslationOn"),
        }
        let writes = m.writes.len();
        let (d, m) = rig.run(&mut s, Stage::Ih, m, &mut Stick::full());
        let want = ["ein Bauteil zwischen Karte und Prozessor leitet nichts weiter", "die Karte bekommt keine Meldeleitung", "lässt die Karte nicht an den Arbeitsspeicher"][case];
        assert!(!d.pass && d.line.contains(want), "{}", d.line);
        assert_eq!((d.totals.writes, m.writes.len() - writes), (0, 0), "case {case}");
        assert!(s.nb.ih.is_none() && m.nb.ih_on == [false, false] && !m.nb.msi_on);
    }
    // ih: rings on pages the card cannot reach are never bound and never named to the card.
    let (mut s, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    m.nb.next_page = (1 << 44) - 4096;
    let (d, m) = rig.run(&mut s, Stage::Ih, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("lässt sich nicht einbinden"), "{}", d.line);
    assert!(d.lines.last().unwrap().contains("PastCeiling"), "{:?}", d.lines.last());
    assert!(m.refused.is_empty() && m.nb.ih_on == [false, false] && s.nb.gart.as_ref().unwrap().bound.is_empty());
    let n = n(&rig);
    assert!(!m.writes.iter().any(|(b, _)| *b == n.byte(ih7::RB_BASE).unwrap()));
}

#[test]
fn only_the_stage_that_lists_it_changes_the_host_side() {
    use crate::gpucore::bringup::{Bringup, Gate};
    use crate::rdna4::guard::{Allow, Guard, HostAsk, Refusal};
    let rig = Rig::new();
    let r = regs(&rig);
    let mut m = model();
    assert!(m.dma_survey().is_ok());
    // A guard that lists nothing: no pages, no enable, no vector.
    let mut g = Guard::new(m, Allow::default(), r.clone());
    assert!(g.pages(4096).is_none() && g.enable_dma().is_err() && g.msi().is_err() && g.atomics().is_err());
    assert_eq!(g.refused, [Refusal::Host { ask: HostAsk::Pages }, Refusal::Host { ask: HostAsk::BusMaster }, Refusal::Host { ask: HostAsk::Msi }, Refusal::Host { ask: HostAsk::Atomics }]);
    assert!(g.hw.nb.host_changes.is_empty() && g.hw.nb.sys_given.is_empty() && !g.hw.nb.atomics_req);
    // Listed, but behind an owner whose gate is shut: still nothing.
    let g = Guard::new(g.hw, Allow::default(), r.clone()).with_host(std::vec![HostAsk::Pages, HostAsk::BusMaster, HostAsk::Msi, HostAsk::Atomics]);
    let mut shut = Bringup::new(g, Gate::Refuse("read-only"), 4);
    assert!(shut.pages(4096).is_none() && shut.enable_dma().is_err() && shut.msi().is_err() && shut.atomics().is_err());
    let g = shut.into_hw();
    assert!(g.hw.nb.host_changes.is_empty() && g.hw.nb.sys_given.is_empty() && !g.hw.nb.atomics_req);
    // Listed and open.
    let mut open = Bringup::new(g, Gate::Allow, 4);
    let at = open.pages(8192).unwrap();
    assert!(open.enable_dma().is_ok() && open.msi().is_ok() && open.atomics().is_ok() && open.sys_write(at + 4096, &[7; 8]));
    // Each stage lists its own and no other's.
    use crate::rdna4::nb;
    assert_eq!(nb::host(Stage::Nbif), [HostAsk::Atomics]);
    assert_eq!(nb::host(Stage::Gmc), [HostAsk::Pages]);
    assert_eq!(nb::host(Stage::Ih), [HostAsk::Pages, HostAsk::BusMaster, HostAsk::Msi]);
    assert!(Stage::ALL.iter().filter(|s| !nb::is_nb(**s)).all(|s| nb::host(*s).is_empty()));
    let mut back = [0u8; 8];
    assert!(open.sys_read(at + 4096, &mut back) && back == [7; 8] && !open.sys_write(at + 8192, &[1]));
    assert_eq!(n(&rig).index_targets().map(Some), nbreg::index_targets(&rdna4_core::ipfacts::TABLE), "the card's own table places them where the measured one does");
}
