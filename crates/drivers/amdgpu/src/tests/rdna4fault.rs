//! Wave 43, item HB: page faults of both hubs through the IH (`rdna4::ihfault`), the TLB flush of both hubs after
//! a bind once GFXHUB is on (`gart12`, with the model's walker through GFXHUB), and GFXHUB switched off for a
//! Reload as Linux's fini does (`gchuboff`).

use super::rdna4fix::*;
use super::rdna4gc::loaded;
use super::rdna4nb::regs;
use super::rdna4nbmodel::n;
use crate::gpucore::bringup::{Bringup, Gate};
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakenb::SYS_BASE;
use crate::rdna4::gart12::{self, FlushRegs};
use crate::rdna4::gchuboff;
use crate::rdna4::guard::Mc;
use crate::rdna4::ihfault::Faults;
use crate::rdna4::ihoff::{Off, OffIo};
use crate::rdna4::ihring::{IhIo, Pass};
use crate::rdna4::linuxseq::compare::{Compare, Kind as DiffKind};
use crate::rdna4::linuxseq::fold::{Bus, fold};
use crate::rdna4::linuxseq::rules::Rule;
use crate::rdna4::linuxseq::{self, Section, What};
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::fault12::{self, st};
use rdna4_core::hub41::{self, GC, Kind, MM};
use rdna4_core::ih7::{Iv, Ring};
use rdna4_core::nbreg::member;
use std::vec::Vec;

/// The whole chain on the model: both hubs on.
fn chain() -> (Rig, Session, Model) {
    let (rig, mut s, m) = loaded();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcBoot, Stage::GcHub]);
    (rig, s, m)
}

#[test]
fn a_fault_of_either_hub_is_read_cleared_counted_and_said() {
    let (rig, mut s, mut m) = chain();
    let n = n(&rig);
    let ih = s.nb.ih.as_mut().unwrap();
    let sys = ih.place.sys;
    let reader = &mut ih.reader;
    // GFXHUB: SDMA0 writing a page it may not write. The entry comes through ring 1, where `ih` sent client 10
    // source 0, with the IH's own entry on ring 0.
    let status = st::CID.put(0, 13) | st::RW.put(0, 1) | st::PERMISSION_FAULTS.put(0, 2);
    m.raise_fault(Kind::Gc, 0x0000_8000_1234_5000, status, true).unwrap();
    assert_eq!((m.faults_open(), m.nb.wptr), ([true, false], [32, 32]));
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys }), [Pass { entries: 1, more: false }, Pass { entries: 1, more: false }]);
    let f = reader.faults.last.unwrap();
    assert_eq!((reader.faults.count, f.hub, f.named, f.addr, f.client(), f.status_write(), reader.self_irqs), ([1, 0], Kind::Gc, Kind::Gc, 0x0000_8000_1234_5000, "SDMA0", true, 1));
    assert_eq!(std::format!("{f}"), "gfxhub page fault at 0x0000800012345000 (vmid 0, pasid 0): SDMA0 (client id 0xd) tried to write; the page does not allow it (permission faults 0x2)");
    // As Linux: the status read twice (the dummy read for GFXHUB), then the fault control with its clear bit, which
    // does not stay; the fault's status is gone and no entry was left without a taker.
    assert_eq!((m.gc.fault_reads, m.gc.faults_cleared, m.faults_open()), ([2, 0], [1, 0], [false, false]));
    assert_eq!(m.writes.last(), Some(&(n.byte(GC.fault_cntl).unwrap(), 0x3FFF_FFFD)));
    assert_eq!(m.reg_read(n.byte(GC.fault_cntl).unwrap()), Some(0x3FFF_FFFC));
    assert!(reader.unclaimed.is_empty() && reader.faults.unread == 0, "{}", reader.describe());
    // MMHUB: VCNU reading an unmapped page, through ring 0; one status read, no dummy.
    let status = st::CID.put(0, 19) | st::MAPPING_ERROR.put(0, 1) | st::VMID.put(0, 2);
    m.raise_fault(Kind::Mm, 0x4000, status, false).unwrap();
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, 1);
    let f = reader.faults.last.unwrap();
    assert_eq!((reader.faults.count, f.hub, f.named, f.client(), f.vmid), ([1, 1], Kind::Mm, Kind::Mm, "VCNU", 2));
    assert_eq!((m.gc.fault_reads, m.gc.faults_cleared, m.faults_open()), ([2, 1], [1, 1], [false, false]));
    assert_eq!(m.reg_read(n.byte(MM.fault_cntl).unwrap()), Some(0x3FFF_FFFC));
    assert!(reader.describe().contains("page faults: gfxhub 1, mmhub 1 (0 not printed, 0 with an unreadable status); the last: mmhub page fault at 0x0000000000004000 (vmid 2, pasid 0): VCNU (client id 0x13) tried to read; no page is mapped there (mapping error)"), "{}", reader.describe());
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

/// Linux prints ten faults in five seconds; every one is still read, cleared and counted.
#[test]
fn faults_past_linuxs_rate_limit_are_counted_and_cleared_all_the_same() {
    let (_, mut s, mut m) = chain();
    let ih = s.nb.ih.as_mut().unwrap();
    let (sys, reader) = (ih.place.sys, &mut ih.reader);
    let status = st::CID.put(0, 7) | st::MAPPING_ERROR.put(0, 1);
    for i in 0..25u64 {
        m.raise_fault(Kind::Gc, i << 12, status, false).unwrap();
        reader.process(&mut ModelIo { m: &mut m, sys });
        assert_eq!(m.faults_open(), [false, false], "fault {i} was left standing");
    }
    assert_eq!((reader.faults.count, reader.faults.rate.missed_total, m.gc.faults_raised, m.gc.faults_cleared), ([25, 0], 15, [25, 0], [25, 0]));
    assert_eq!(reader.faults.last.map(|f| (f.addr, f.client())), Some((24 << 12, "RLC")));
    // Five seconds on: printed again.
    m.card.now_us += fault12::RATE_INTERVAL_US + 1;
    m.raise_fault(Kind::Mm, 0x1000, status, false).unwrap();
    reader.process(&mut ModelIo { m: &mut m, sys });
    assert_eq!((reader.faults.count, reader.faults.rate.missed, reader.faults.rate.missed_total), ([25, 1], 0, 15));
    // Two faults before the reader looks: the first's status stands with MORE_FAULTS, and one clear takes both.
    m.raise_fault(Kind::Gc, 0x7000, status, false).unwrap();
    m.raise_fault(Kind::Gc, 0x8000, status, false).unwrap();
    reader.process(&mut ModelIo { m: &mut m, sys });
    assert_eq!((reader.faults.count[0], m.faults_open(), m.gc.faults_cleared[0]), (27, [false, false], 26));
    assert!(m.refused.is_empty());
}

/// A hub that is not on raises nothing; an entry whose hub does not answer is still counted, never silent.
#[test]
fn a_fault_entry_is_never_silent() {
    let (_, mut s, mut m) = loaded();
    assert!(m.raise_fault(Kind::Gc, 0x1000, 1, false).is_err(), "GFXHUB is not on before gchub");
    let ih = s.nb.ih.as_mut().unwrap();
    let (sys, reader) = (ih.place.sys, &mut ih.reader);
    // The entry alone, with nothing behind it in the hub (status 0): Linux prints its two lines and no status.
    m.raise(Ring::Ring0, &Iv { client_id: fault12::CLIENT_GFX, src_id: 0, src_data: [0x55, fault12::DATA_WRITE, 0, 0], ..Iv::default() }).unwrap();
    reader.process(&mut ModelIo { m: &mut m, sys });
    let f = reader.faults.last.unwrap();
    assert_eq!((reader.faults.count, f.status, f.addr, f.write), ([1, 0], 0, 0x55000, true));
    assert!(std::format!("{f}").ends_with("a write (the entry's flag), the hub's status read 0: no engine and no kind known"));
    assert!(reader.unclaimed.is_empty());
    // A reader whose hub registers do not read: counted as a fault with an unreadable status.
    struct Deaf;
    impl IhIo for Deaf {
        fn mem32(&mut self, _: u64) -> Option<u32> {
            None
        }
        fn set_mem32(&mut self, _: u64, _: u32) -> bool {
            false
        }
        fn wptr_reg(&mut self, _: Ring) -> Option<u32> {
            None
        }
        fn cntl(&mut self, _: Ring) -> Option<u32> {
            None
        }
        fn set_cntl(&mut self, _: Ring, _: u32) -> bool {
            false
        }
        fn doorbell(&mut self, _: Ring, _: u32) -> bool {
            false
        }
        fn fault_status(&mut self, _: Kind) -> Option<u32> {
            Some(u32::MAX)
        }
        fn fault_clear(&mut self, _: Kind) -> bool {
            false
        }
        fn now_us(&mut self) -> u64 {
            7
        }
    }
    let mut faults = Faults::default();
    let f = faults.take(&mut Deaf, &Iv { client_id: fault12::CLIENT_VMC, vmid_src: true, src_data: [1, 0, 0, 0], ..Iv::default() }, Kind::Mm);
    assert_eq!((faults.count, faults.unread, f.status, f.addr), ([0, 1], 1, 0, 0x1000));
    assert!(faults.describe().contains("mmhub 1 (0 not printed, 1 with an unreadable status)"));
    assert_eq!(Faults::default().describe(), "no page faults");
}

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

#[test]
fn a_reload_switches_gfxhub_off_as_linux_does_and_writes_nothing_while_it_is_off() {
    let rig = Rig::new();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    // No power, power without the hub, the block started but its hub not: not one write.
    let (_, _, mut started) = loaded();
    let mut powered = model();
    powered.pmfw.enabled = true;
    for m in [&mut model(), &mut powered, &mut started] {
        let writes = m.writes.len();
        assert_eq!(gchuboff::gart_disable(&mut ModelOff(m), &n, &GC), Off::NotOn);
        assert_eq!(m.writes.len(), writes);
    }
    // The hub on: gfxhub_v12_0_gart_disable's writes in its order (gfxhub_v12_0.c:388-409).
    let (_, _, mut m) = chain();
    let writes = m.writes.len();
    assert_eq!(gchuboff::gart_disable(&mut ModelOff(&mut m), &n, &GC), Off::Done);
    let mut want: Vec<(u32, u32)> = (0..16).map(|i| (b(member(GC.context0_cntl, i, 1)), 0)).collect();
    want.extend([(b(GC.l1_tlb), 0x0000_1818), (b(GC.l2_cntl), 0x0008_0E00), (b(GC.l2_cntl3), 0)]);
    assert_eq!(m.writes[writes..], want[..]);
    assert!(!m.gc.context0 && m.refused.is_empty(), "{:?}", m.refused);
    // Off now: the hub translates nothing, raises nothing, and a second Reload writes nothing.
    assert!(m.gc_walk(0x1000).is_none() && m.raise_fault(Kind::Gc, 0x1000, 1, false).is_err());
    let writes = m.writes.len();
    assert_eq!(gchuboff::gart_disable(&mut ModelOff(&mut m), &n, &GC), Off::NotOn);
    assert_eq!(m.writes.len(), writes);
    // The same steps over MMHUB's set are Linux's mmhub_v4_1_0_gart_disable: one sequence, two sets.
    assert_eq!(gchuboff::gart_disable(&mut ModelOff(&mut m), &n, &MM), Off::Done);
    assert_eq!(m.writes[writes], (b(MM.context0_cntl), 0));
    // A block that answers all ones or does not read is left alone.
    struct Gone(Option<u32>, u32);
    impl OffIo for Gone {
        fn read(&mut self, _: u32) -> Option<u32> {
            self.0
        }
        fn write(&mut self, _: u32, _: u32) -> bool {
            self.1 += 1;
            true
        }
        fn delay_us(&mut self, _: u64) {}
    }
    for answer in [Some(u32::MAX), None] {
        let mut g = Gone(answer, 0);
        assert_eq!((gchuboff::gart_disable(&mut g, &n, &GC), g.1), (Off::Gone, 0));
    }
}

/// A card after the whole chain behind an owner that keeps the trail, with MC addresses.
fn gart_rig() -> (Rig, Session, Bringup<Mc<Model>>) {
    let (rig, s, m) = chain();
    let l = s.layout.clone().unwrap();
    let owner = Bringup::new(Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes }, Gate::Allow, 64).with_trail(256).without_read_back();
    (rig, s, owner)
}

fn shape(lines: &[linuxseq::Line]) -> std::string::String {
    lines.iter().map(|l| match l.what { What::Hdp { .. } => 'H', What::Wait { .. } => 'T', What::Write { .. } => 'W', What::Read { .. } => 'R', _ => '?' }).collect()
}

#[test]
fn a_bind_after_gchub_flushes_both_hubs_in_linuxs_order() {
    let (rig, mut s, mut hw) = gart_rig();
    let n = n(&rig);
    // Until `gchub` passed a flush is MMHUB's alone; from then on it names GFXHUB too.
    let before = loaded();
    assert!(before.1.flush_regs(&n).unwrap().gc.is_none());
    let f = s.flush_regs(&n).unwrap();
    let eng = |r| n.byte(member(r, hub41::GART_ENGINE, 1)).unwrap();
    assert_eq!(f.gc, Some((eng(GC.eng0_req), eng(GC.eng0_ack))));
    assert_eq!(f.written(), [f.hdp, f.sem, f.req, f.cid2, eng(GC.eng0_req)]);
    // Linux's unit for it: the bind's HDP flush, GFXHUB's flush (bit 0 of vmhubs_mask), MMHUB's.
    let unit = linuxseq::bind_flush_both().unwrap();
    assert_eq!((shape(&unit), shape(&linuxseq::bind_flush().unwrap())), (std::string::String::from("HHWTHTWTWRWR"), std::string::String::from("HHTWTWRWR")));
    assert!(matches!(unit[2].what, What::Write { name: "GCVM_INVALIDATE_ENG17_REQ", value: 0x00F8_0001, .. }) && matches!(unit[6].what, What::Write { name: "MMVM_INVALIDATE_ENG17_REQ", .. }));
    let gart = s.nb.gart.as_mut().unwrap();
    let pages: Vec<u64> = (0..3).map(|i| SYS_BASE + (64 << 20) + i * 4096).collect();
    let mut out = Vec::new();
    let (gc_before, mm_before) = (hw.hw().hw.gc.tlb_flushes, hw.hw().hw.nb.tlb_flushes);
    let mc = gart.map(&mut hw, &f, "three pages", &pages, &mut out).unwrap();
    let trail: Vec<Access> = hw.trail().unwrap().recent().copied().collect();
    let did = fold(&trail, &Bus::of(&regs(&rig))).unwrap();
    let sec = Section { name: "bind-flush", head: "", lines: unit };
    let mut c = Compare::new(&sec, &[]).unwrap();
    c.feed(&did).unwrap_or_else(|d| panic!("{d}"));
    c.finish().unwrap_or_else(|d| panic!("{d}"));
    assert_eq!((hw.hw().hw.gc.tlb_flushes, hw.hw().hw.nb.tlb_flushes), (gc_before + 1, mm_before + 1));
    // The unbind flushes both again.
    gart.unmap(&mut hw, &f, mc, &mut out).unwrap();
    assert_eq!((hw.hw().hw.gc.tlb_flushes, hw.hw().hw.nb.tlb_flushes, out.len()), (gc_before + 2, mm_before + 2, 0));
    assert!(hw.hw().hw.refused.is_empty(), "{:?}", hw.hw().hw.refused);
}

/// The card's walker through GFXHUB sees a bound page only after the HDP flush AND GFXHUB's own invalidation; a
/// flush of MMHUB alone, as before `gchub`, leaves GFXHUB's view old.
#[test]
fn gfxhubs_walker_sees_a_page_only_after_its_own_invalidation() {
    let (rig, mut s, hw) = gart_rig();
    let n = n(&rig);
    let mut card = hw.into_hw();
    let both = s.flush_regs(&n).unwrap();
    let mm_only = FlushRegs::of(&n, s.nb.hdp_flush(n.hdp_flush)).unwrap();
    let gart = s.nb.gart.as_mut().unwrap();
    let page = SYS_BASE + (80 << 20);
    let mut out = Vec::new();
    // What `ih` bound before GFXHUB came on is seen by it: a context that starts has nothing cached.
    let ih_mc = gart.l.gart_start + 0x1000;
    assert!(card.hw.gc_walk(ih_mc).is_some() && card.hw.gc.entries_pending() == 0);
    let mc = gart.bind(&mut card, "one page", &[page]).unwrap();
    assert_eq!((card.hw.gc_walk(mc), card.hw.gc.entries_pending()), (None, 1));
    gart12::flush_hdp(&mut card, &both).unwrap();
    assert_eq!(card.hw.gc_walk(mc), None, "the HDP flush alone");
    gart12::invalidate_tlb(&mut card, &mm_only, &mut out).unwrap();
    assert_eq!((card.hw.gc_walk(mc), card.hw.gc.entries_pending(), card.hw.nb.entries_pending()), (None, 1, 0), "MMHUB's invalidation alone");
    gart12::invalidate_tlb(&mut card, &both, &mut out).unwrap();
    assert_eq!((card.hw.gc_walk(mc + 0x10), card.hw.gc.entries_pending()), (Some(page + 0x10), 0));
    // The other way round: an unbound page is still reached through GFXHUB until both flushes.
    gart.unbind(&mut card, mc).unwrap();
    assert_eq!(card.hw.gc_walk(mc), Some(page));
    gart12::invalidate_tlb(&mut card, &both, &mut out).unwrap();
    assert_eq!(card.hw.gc_walk(mc), None);
    assert!(out.is_empty() && card.hw.refused.is_empty(), "{:?}", card.hw.refused);
}

/// The rule a later stage declares for a bind of its own once GFXHUB is on: the twelve lines, each compared; a
/// stage that flushes MMHUB alone there is named.
#[test]
fn the_bind_rule_for_both_hubs_holds_every_line() {
    let (rig, s, _) = chain();
    let r = regs(&rig);
    let bus = Bus::of(&r);
    let n = n(&rig);
    let eng = |x| n.byte(member(x, hub41::GART_ENGINE, 1)).unwrap();
    // The `ih` stage's own run, whose bind was flushed on MMHUB alone, against its section under each rule.
    let rig2 = Rig::new();
    let mut s2 = Session::default();
    let (done, _) = rig2.pass(&mut s2, model(), &mut Stick::full(), &NB);
    let ih = &done[2];
    let rules = |both: bool| -> Vec<Rule> {
        crate::rdna4::nbseq::rules(Stage::Ih, rig.att.vram_bytes)
            .into_iter()
            .map(|x| match x {
                Rule::BindBefore { reg, why } if both => Rule::BindBothBefore { reg, why },
                other => other,
            })
            .collect()
    };
    let sec = linuxseq::section("ih").unwrap();
    let (one, two) = (rules(false), rules(true));
    let mut c = Compare::new(&sec, &one).unwrap();
    c.feed_trail(&ih.trail, 0, &bus).unwrap();
    c.finish().unwrap();
    let mut c = Compare::new(&sec, &two).unwrap();
    let d = c.feed_trail(&ih.trail, 0, &bus).unwrap_err();
    assert_eq!(d.kind, DiffKind::Missing, "{d}");
    assert!(d.text.contains("gc-hub.txt line") && d.text.contains("GCVM_INVALIDATE_ENG17_REQ"), "{d}");
    // With GFXHUB's three lines where Linux has them, the same trail holds under the rule for both hubs.
    let hdp = ih.trail.iter().position(|a| matches!(a, Access::Write { byte, value: 0, .. } if *byte == s.nb.hdp_flush(n.hdp_flush))).unwrap();
    let mut t = ih.trail.clone();
    let wait = Access::Wait { byte: eng(GC.eng0_ack), cond: crate::gpucore::psp::Cond::Masked { mask: 1, value: 1 }, bound: crate::gpucore::psp::Bound::Reads(100_000), reads: 1, last: Some(1), us: 0 };
    let gc = [t[hdp], t[hdp + 1], Access::Write { byte: eng(GC.eng0_req), value: 0x00F8_0001, reached: true }, wait];
    t.splice(hdp + 2..hdp + 2, gc);
    let mut c = Compare::new(&sec, &two).unwrap();
    c.feed_trail(&t, 0, &bus).unwrap_or_else(|d| panic!("{d}"));
    c.finish().unwrap();
}
