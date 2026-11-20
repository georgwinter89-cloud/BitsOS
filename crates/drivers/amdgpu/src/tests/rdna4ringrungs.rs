//! Wave 44, item RG: the stage `ringtest` on the model. Every rung passes on Linux's frames; the end-of-pipe
//! entries arrive through the IH reader by ring id; the remap goes through MES; what the rungs took is given back;
//! a second call (the enables already on) is held against the forms without them; the rungs need `gcring`; a ring
//! test on the queue MES unmapped fails on the fake before its map again.

use super::rdna4cpeng::{kq, mesq_up, rptr, submit};
use super::rdna4fix::*;
use crate::gpucore::psp::Hw;
use crate::rdna4::host::Host;
use crate::rdna4::meskq;
use crate::rdna4::ringih;
use crate::rdna4::ringrungs::RUNGS;
use crate::rdna4::ringseq;
use crate::rdna4::stage::Stage;
use rdna4_core::gc12ring as rg;
use rdna4_core::mes12::KERNEL_QUEUES;
use rdna4_core::mes12api::Unmap;

#[test]
fn every_rung_passes_on_linuxs_frames_and_gives_back_what_it_took() {
    let (rig, mut s, m) = mesq_up();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcRing]);
    let (slots, bound) = (s.mes.up.as_ref().unwrap().pool.used(), s.nb.gart.as_ref().unwrap().bound.clone());
    let (d, m) = rig.run(&mut s, Stage::RingTest, m, &mut Stick::full());
    assert!(d.pass && d.refused.is_empty() && m.refused.is_empty(), "{}\n{}\n{:?}", d.line, d.lines.join("\n"), m.refused);
    assert_eq!(s.rings.rungs, RUNGS.map(|n| (n, true)));
    // The enables as Linux's fence driver sets them, over what gc-ring left.
    assert!(d.lines.iter().any(|l| l == "rung eop-on: PASS: TIME_STAMP and GENERIC0 on: CP_INT_CNTL_RING0 0x1c00000 to 0x85c00000, CP_ME1_PIPE0_INT_CNTL 0x1800000 to 0x85800000, CP_ME1_PIPE1_INT_CNTL 0x1800000 to 0x85800000"), "{:?}", d.lines);
    // Each IB test's fence came with its entry; each entry arrived by the queue's ring id.
    let ih = s.nb.ih.as_ref().unwrap();
    for k in &KERNEL_QUEUES {
        let want = if k.doorbell == KERNEL_QUEUES[0].doorbell || k.gfx { 2 } else { 1 };
        assert_eq!(ringih::eops(ih, k), want, "{k:?}");
    }
    assert!(d.lines.iter().filter(|l| l.contains("its end-of-pipe entry came through the IH reader: true")).count() == 5, "{:?}", d.lines);
    // Given back: the write-back slots and the IB page's GART entries.
    assert_eq!((s.mes.up.as_ref().unwrap().pool.used(), &s.nb.gart.as_ref().unwrap().bound), (slots, &bound));
    assert!(s.rings.ib_mc.is_none() && s.rings.ib_page.is_some() && s.rings.eop_on);
    // The call held against Linux's forms, line for line.
    let last = d.lines.last().unwrap();
    assert!(last.starts_with("linuxseq gc-ring: equal: the call's register accesses are Linux's forms, line for line: the end-of-pipe enables"), "{last}");
    assert_eq!(ringseq::expected(true).unwrap().len(), 6 + 3 + 3 + 3);
    // A second call: the enables stay on and are not written again; the rest is the same and equal.
    let (d, m) = rig.run(&mut s, Stage::RingTest, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty(), "{}\n{}", d.line, d.lines.join("\n"));
    assert!(d.lines.iter().any(|l| l.starts_with("rung eop-on: PASS: the end-of-pipe interrupts are on since an earlier call")), "{:?}", d.lines);
    assert!(d.lines.last().unwrap().starts_with("linuxseq gc-ring: equal: the call's register accesses are Linux's forms, line for line: the TLB flush"), "{:?}", d.lines.last());
}

#[test]
fn the_rungs_need_gcring_and_refuse_before_any_access() {
    let (rig, mut s, m) = mesq_up();
    let (d, m) = rig.run(&mut s, Stage::RingTest, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && m.refused.is_empty() && d.line.contains("zuerst die Befehlsringe prüfen"), "{} {:?}", d.line, d.lines);
    assert!(d.lines.iter().all(|l| !l.starts_with("linuxseq ")));
}

/// The fake card answers the forms the rungs are held to; a call whose accesses are not those forms is named.
#[test]
fn an_access_out_of_the_rungs_forms_is_named() {
    let (rig, mut s, m) = mesq_up();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcRing]);
    let (d, _) = rig.run(&mut s, Stage::RingTest, m, &mut Stick::full());
    let r = super::rdna4nb::regs(&rig);
    let mut trail = d.trail.clone();
    // The end-of-pipe enable of the gfx pipe written with one bit only.
    if let Some(crate::gpucore::trail::Access::Write { value, .. }) = trail.iter_mut().find(|a| matches!(a, crate::gpucore::trail::Access::Write { .. })) {
        *value &= !rg::f::GENERIC0_INT_ENABLE.mask;
    }
    let v = ringseq::rungs_judge(true, &trail, 0, &r, &[]).unwrap();
    assert!(v.line.starts_with("linuxseq gc-ring: differs:") && v.line.contains("CP_INT_CNTL_RING0"), "{}", v.line);
}

/// The ring test on the queue MES unmapped fails on the fake (the scratch register keeps its poison), and passes
/// again once MES mapped it again.
#[test]
fn a_ring_test_on_the_unmapped_queue_fails_until_mes_maps_it_again() {
    let (rig, mut s, mut m) = mesq_up();
    let n = super::rdna4cp::n_of(&rig);
    let scratch = n.byte(rg::SCRATCH_REG0).unwrap();
    let legacy = kq(&s, 0).legacy(&s.mes.up.as_ref().unwrap().place);
    let (sent, _) = meskq::unmap_legacy(s.mes.up.as_mut().unwrap(), &mut m, &legacy, Unmap::ResetQueues, (0, 0)).unwrap();
    assert!(sent.ok());
    let words = crate::rdna4::ringops::ring_test(scratch / 4).unwrap();
    assert!(m.reg_write(scratch, amdgpu_fmt::gfx12::RING_TEST_POISON));
    let (rung, _) = submit(&s, &mut m, 0, 0, &words);
    assert!(rung && m.reg_read(scratch) == Some(0xCAFE_DEAD) && m.refused.pop().unwrap().contains("no queue was placed"));
    // Restored as Linux's resume restores it (the write pointer 0) and mapped again: the test passes.
    let up = s.mes.up.as_ref().unwrap();
    assert!(m.sys_write(up.place.slot_sys(kq(&s, 0).slots.wptr), &0u64.to_le_bytes()));
    let (sent, _) = meskq::map_legacy(s.mes.up.as_mut().unwrap(), &mut m, &legacy).unwrap();
    assert!(sent.ok() && m.refused.is_empty(), "{:?}", m.refused);
    let (rung, wptr) = submit(&s, &mut m, 0, 0, &words);
    assert!(rung && m.reg_read(scratch) == Some(0xDEAD_BEEF) && rptr(&s, &mut m, 0) == wptr);
}

/// An IB test whose fence does not come keeps its write-back slot for the boot: the IB may still run and write it
/// (MEC is halted here behind the session's back, so a compute queue's doorbell runs nothing).
#[test]
fn an_ib_test_whose_fence_does_not_come_keeps_its_slot() {
    let (rig, mut s, m) = mesq_up();
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcRing]);
    let mec = super::rdna4cp::n_of(&rig).byte(rdna4_core::gc12::CP_MEC_RS64_CNTL).unwrap();
    let v = m.reg_read(mec).unwrap();
    assert!(m.reg_write(mec, rdna4_core::gc12cp::halt_mec(v)), "{:?}", m.refused);
    let slots = s.mes.up.as_ref().unwrap().pool.used();
    let (d, m) = rig.run(&mut s, Stage::RingTest, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("ein Befehlsring hat einen Auftrag nicht beendet"), "{}\n{}", d.line, d.lines.join("\n"));
    assert_eq!(s.rings.rungs, [(RUNGS[0], true), (RUNGS[1], true)], "ib-gfx passed before it");
    assert!(d.lines.iter().any(|l| l.starts_with("kept after the failed IB test (its IB may still write it): write-back slot ")), "{:?}", d.lines);
    assert_eq!(s.mes.up.as_ref().unwrap().pool.used(), slots + 1, "the failed test's slot stays taken, the passed one's came back");
    assert!(!m.refused.is_empty() && m.refused.iter().all(|r| r.contains("a queue on a halted engine")), "{:?}", m.refused);
}
