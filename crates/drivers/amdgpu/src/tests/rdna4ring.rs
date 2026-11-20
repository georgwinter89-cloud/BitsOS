//! Wave 44, item RG: the stage `gcring` on the model. It equals the trace's section gc-ring with no declared
//! difference; it refuses a run out of order, a second run and a run after a stop before any access; a ring test
//! on a queue MES did not map times out as on the card and the stage names what a hung engine shows; the model
//! refuses the section's registers in another order, with other values or before the queues are mapped.

use super::rdna4cpeng::{kq, mesq_up};
use super::rdna4fix::*;
use super::rdna4gc::chain_before;
use super::rdna4nb::regs;
use crate::gpucore::psp::Hw;
use crate::rdna4::host::Host;
use crate::rdna4::chainseq;
use crate::rdna4::linuxseq::compare::Compare;
use crate::rdna4::linuxseq::fold::Bus;
use crate::rdna4::linuxseq::section;
use crate::rdna4::meskq;
use crate::rdna4::ringstage::{TEST_ORDER, ring_name};
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::gc12ring as rg;
use rdna4_core::mes12::KERNEL_QUEUES;
use rdna4_core::mes12api::Unmap;
use std::vec::Vec;

#[test]
fn the_stage_equals_gc_ring_with_no_declared_difference() {
    let (rig, mut s, m) = mesq_up();
    let (d, m) = rig.run(&mut s, Stage::GcRing, m, &mut Stick::full());
    assert!(d.pass && d.refused.is_empty() && m.refused.is_empty(), "{}\n{}\n{:?}", d.line, d.lines.join("\n"), m.refused);
    // The stage alone against its section: no rule, and every line used up.
    let sec = section("gc-ring").unwrap();
    assert!(chainseq::rules("gc-ring", rig.att.vram_bytes, chainseq::Chosen::default()).unwrap().is_empty());
    assert_eq!((sec.lines.len(), sec.lines[0].init, sec.lines.last().unwrap().init), (26, 9568, 9603));
    let mut c = Compare::new(&sec, &[]).unwrap();
    c.feed_trail(&d.trail, d.trail_lost, &Bus::of(&regs(&rig))).unwrap_or_else(|e| panic!("{e}"));
    c.finish().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(d.lines.last().unwrap(), "linuxseq gc-ring: equal");
    // Linux's order of the five tests, gfx first, and each queue's write pointer one frame on.
    let tested: Vec<&str> = d.lines.iter().filter(|l| l.contains(" test passed ")).map(|l| l.split(' ').nth(2).unwrap()).collect();
    assert_eq!(tested, ["gfx_0.0.0", "comp_1.0.0", "comp_1.1.0", "comp_1.0.1", "comp_1.1.1"]);
    assert_eq!(TEST_ORDER.map(|i| ring_name(&KERNEL_QUEUES[i])), ["gfx_0.0.0", "comp_1.0.0", "comp_1.1.0", "comp_1.0.1", "comp_1.1.1"]);
    assert_eq!(s.rings.wptr, [256; 5]);
    // The card holds Linux's values: the context count, the device id, the three enables of each pipe.
    let n = super::rdna4cp::n_of(&rig);
    let mut m = m;
    let rd = |m: &mut crate::rdna4::fakemodel::Model, r| m.reg_read(n.byte(r).unwrap()).unwrap();
    assert_eq!((rd(&mut m, rg::CP_MAX_CONTEXT), rd(&mut m, rg::CP_DEVICE_ID)), (7, 1));
    assert_eq!((rd(&mut m, rg::CP_INT_CNTL_RING0), rd(&mut m, rg::CP_ME1_PIPE0_INT_CNTL), rd(&mut m, rg::CP_ME1_PIPE1_INT_CNTL)), (0x01C0_0000, 0x0180_0000, 0x0180_0000));
    assert!(m.cp.queues.iter().all(|q| q.halted.is_none()) && m.cp.raised.is_empty(), "no interrupt while the end-of-pipe enables are off");
    assert!(s.passed(Stage::GcRing) && s.next() == Some(Stage::Sdma) && s.summary().contains("rings tested"), "{}", s.summary());
}

#[test]
fn the_stage_refuses_before_any_access_what_is_its_own_to_refuse() {
    let rig = Rig::new();
    let untouched = |d: &crate::rdna4::stage::Done, what: &str| {
        assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && d.lines.iter().all(|l| !l.starts_with("linuxseq ")), "{what}: {} {:?}", d.line, d.lines);
    };
    // Out of the chain's order: each stage before it missing.
    for missing in [Stage::GcCp, Stage::Mes, Stage::MesQ] {
        let mut s = Session::default();
        let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(missing));
        m.writes.clear();
        let (d, m) = rig.run(&mut s, Stage::GcRing, m, &mut Stick::full());
        untouched(&d, "out of order");
        assert!(m.writes.is_empty() && d.lines.last().unwrap().contains("gcring needs the mesq stage of this boot"), "{missing:?}: {:?}", d.lines);
    }
    // A second run, and a run after a stop that wrote registers.
    let (rig, mut s, m) = mesq_up();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcRing]);
    let (d, _) = rig.run(&mut s, Stage::GcRing, m, &mut Stick::full());
    untouched(&d, "a second run");
    assert!(d.line.contains("schon geprüft"), "{}", d.line);
    let (rig, mut s, m) = mesq_up();
    s.rings.tried = true;
    let (d, _) = rig.run(&mut s, Stage::GcRing, m, &mut Stick::full());
    untouched(&d, "after a stop");
    assert!(d.line.contains("erst neu starten"), "{}", d.line);
}

/// A ring test on a queue MES did not map: the card takes the doorbell and nothing runs, so SCRATCH_REG0 keeps
/// the poison through Linux's 100000 reads; the stage stops with -ETIMEDOUT and looks at what a hung engine shows.
#[test]
fn a_ring_test_on_a_queue_mes_did_not_map_times_out_as_on_the_card() {
    let (rig, mut s, mut m) = mesq_up();
    // comp_1.0.0 taken off its pipe through MES (as a suspend does), the session not told.
    let legacy = kq(&s, 0).legacy(&s.mes.up.as_ref().unwrap().place);
    let (sent, _) = meskq::unmap_legacy(s.mes.up.as_mut().unwrap(), &mut m, &legacy, Unmap::ResetQueues, (0, 0)).unwrap();
    assert!(sent.ok());
    let (d, m) = rig.run(&mut s, Stage::GcRing, m, &mut Stick::full());
    assert!(!d.pass && d.line.ends_with("angehalten: eine Rechen-Warteschlange führt keinen Befehl aus."), "{}", d.line);
    let stopped = d.lines.iter().find(|l| l.starts_with("stopped: ")).unwrap();
    assert!(stopped.contains("ring comp_1.0.0 test failed (-ETIMEDOUT): SCRATCH_REG0 still 0xcafedead after Linux's 100000 reads; the write pointer 256, the read pointer's slot 0x0"), "{stopped}");
    assert!(d.lines.iter().any(|l| l.starts_with("gcring: ring gfx_0.0.0 test passed")), "the gfx ring before it passed: {:?}", d.lines);
    // The card's side: the doorbell was a posted write nobody answered, named, and nothing ran.
    assert!(m.refused.len() == 1 && m.refused[0].contains("no queue was placed"), "{:?}", m.refused);
    // The look after the stop, outside the comparison: an idle block (nothing hung, nothing ran).
    assert!(d.lines.iter().any(|l| l.starts_with("gcring: after the stop GRBM_STATUS 0x0 (idle)")), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.starts_with("linuxseq gc-ring: the same as Linux's as far as the stage came")), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l == "linuxseq: 2 accesses after the stop (the look at a hung engine after the stop) are not compared"), "{:?}", d.lines);
    assert!(s.rings.tried && !s.rings.passed);
}

/// A queue that halted (a packet Linux never emits) shows in GRBM_STATUS and CP_STAT, and its ring test after it
/// fails as Linux's would.
#[test]
fn a_hung_queue_shows_in_grbm_status_and_its_ring_test_fails() {
    let (rig, mut s, mut m) = mesq_up();
    let (rung, _) = super::rdna4cpeng::submit(&s, &mut m, 0, 0, &[amdgpu_fmt::pm4::packet3(0x84, 0), 0]);
    assert!(rung && m.cp.queues.iter().any(|q| q.halted.is_some()));
    m.refused.clear();
    let (d, m) = rig.run(&mut s, Stage::GcRing, m, &mut Stick::full());
    assert!(!d.pass && d.line.ends_with("angehalten: eine Rechen-Warteschlange führt keinen Befehl aus."), "{}", d.line);
    assert!(d.lines.iter().any(|l| l.starts_with("gcring: after the stop GRBM_STATUS 0xa0000000 (the graphics block is busy: an engine did not finish), CP_STAT 0x80000000")), "{:?}", d.lines);
    assert!(m.refused.is_empty() || m.refused.iter().all(|r| r.contains("CP queue at doorbell")), "{:?}", m.refused);
}

#[test]
fn the_model_takes_the_sections_registers_only_as_linux_writes_them() {
    let (rig, s, mut m) = mesq_up();
    let n = super::rdna4cp::n_of(&rig);
    let b = |r| n.byte(r).unwrap();
    let refused = |m: &mut crate::rdna4::fakemodel::Model, byte: u32, v: u32, why: &str| {
        let before = m.refused.len();
        assert!(!m.reg_write(byte, v) && m.refused.len() == before + 1 && m.refused[before].contains(why), "{v:#x}: {:?}", &m.refused[before..]);
    };
    // CP_MAX_CONTEXT only 7, CP_DEVICE_ID only 1 and only after it.
    refused(&mut m, b(rg::CP_DEVICE_ID), 1, "right after CP_MAX_CONTEXT");
    refused(&mut m, b(rg::CP_MAX_CONTEXT), 8, "max_hw_contexts - 1 = 7");
    assert!(m.reg_write(b(rg::CP_MAX_CONTEXT), 7) && m.reg_write(b(rg::CP_DEVICE_ID), 1));
    refused(&mut m, b(rg::CP_DEVICE_ID), 2, "Linux writes 1");
    // The enables in Linux's order only: the privileged instruction before the privileged register is refused, so
    // is a write that sets two bits or a pipe out of turn, and GRBM_STATUS is never written.
    refused(&mut m, b(rg::CP_INT_CNTL_RING0), rg::f::PRIV_INSTR_INT_ENABLE.mask, "CP_INT_CNTL_RING0's next enable");
    refused(&mut m, b(rg::CP_ME1_PIPE0_INT_CNTL), rg::f::PRIV_REG_INT_ENABLE.mask, "CP_INT_CNTL_RING0's next enable");
    refused(&mut m, b(rg::CP_INT_CNTL_RING0), rg::f::PRIV_REG_INT_ENABLE.mask | rg::f::OPCODE_ERROR_INT_ENABLE.mask, "next");
    refused(&mut m, b(rg::GRBM_STATUS), 0, "Linux only reads it");
    for op in rg::FAULT_ENABLES {
        let rdna4_core::hubseq::Op::Modify(r, f) = op else { continue };
        let v = m.reg_read(b(r)).unwrap();
        assert!(m.reg_write(b(r), f(v)), "{}: {:?}", r.name, m.refused);
    }
    // Then only the end-of-pipe enables, on or off, both bits together.
    refused(&mut m, b(rg::CP_ME1_PIPE0_INT_CNTL), 0x0180_0000 | rg::f::TIME_STAMP_INT_ENABLE.mask, "end-of-pipe enables on or off");
    assert!(m.reg_write(b(rg::CP_ME1_PIPE0_INT_CNTL), rg::eop_on(0x0180_0000)) && m.reg_write(b(rg::CP_ME1_PIPE0_INT_CNTL), 0x0180_0000));
    assert!(m.refused.len() == 8, "{:?}", m.refused);
    assert!(s.mes.kq);
}

/// No register of the rings before the queues are mapped: on the card after `gccp` (MES not started) and after
/// `mes` (the kernel queues not mapped yet), CP_MAX_CONTEXT and the interrupt enables are refused.
#[test]
fn no_ring_register_before_the_kernel_queues_are_mapped() {
    let rig = Rig::new();
    let n = super::rdna4cp::n_of(&rig);
    for upto in [Stage::Mes, Stage::MesQ] {
        let mut s = Session::default();
        let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(upto));
        assert!(!m.reg_write(n.byte(rg::CP_MAX_CONTEXT).unwrap(), 7) && m.refused.pop().unwrap().contains("once the gfx ring is mapped"), "{upto:?}");
        assert!(!m.reg_write(n.byte(rg::CP_ME1_PIPE0_INT_CNTL).unwrap(), rg::f::PRIV_REG_INT_ENABLE.mask) && m.refused.pop().unwrap().contains("before the five kernel queues are mapped"), "{upto:?}");
    }
}

/// The whole chain on the fake card from a cold card through SDMA's rungs and the rings' rungs, every covered
/// section compared with chainseq in one go: waves 42 and 43's nine and gc-cp, mes, gc-ring.
#[test]
fn one_run_from_a_cold_card_through_both_rungs_equals_every_covered_section() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (done, m) = rig.pass(&mut s, model(), &mut Stick::full(), &Stage::CHAIN);
    let (taken, bound) = (s.bump.as_ref().unwrap().taken.clone(), s.nb.gart.as_ref().unwrap().bound.clone());
    let (rungs, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::SdmaTest, Stage::RingTest]);
    let l = s.layout.clone().unwrap();
    let compared = chainseq::equal(&done, &regs(&rig), &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = compared.iter().map(|(n, _, _)| *n).collect();
    assert_eq!(names, ["nbif", "mmhub", "ih", "psp-boot", "psp-ring", "smu", "gc-boot", "gc-hub", "gc-cp", "mes", "gc-ring", "sdma"]);
    for d in done.iter().filter(|d| [Stage::GcCp, Stage::MesQ, Stage::GcRing, Stage::Sdma].contains(&d.stage)) {
        assert!(d.lines.last().unwrap().ends_with(": equal"), "{:?}: {:?}", d.stage, d.lines.last());
    }
    assert!(rungs[0].lines.last().unwrap().starts_with("linuxseq mes: equal: the call's only register accesses are 2 of the TLB flush"), "{:?}", rungs[0].lines.last());
    assert!(rungs[1].lines.last().unwrap().starts_with("linuxseq gc-ring: equal: the call's register accesses are Linux's forms, line for line: the end-of-pipe enables"), "{:?}", rungs[1].lines.last());
    assert_eq!(s.rings.rungs.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), crate::rdna4::ringrungs::RUNGS.map(|n| (n, true)));
    // What both rungs took is back: the manager and the GART hold what they held when the chain had passed.
    assert_eq!((&s.bump.as_ref().unwrap().taken, &s.nb.gart.as_ref().unwrap().bound), (&taken, &bound));
    let ih = s.nb.ih.as_ref().unwrap();
    assert!(m.faults_open() == [false, false] && ih.reader.faults.count == [0, 0] && ih.reader.unclaimed.is_empty(), "{}", ih.reader.describe());
    assert!(m.cp.queues.len() == 5 && m.cp.queues.iter().all(|q| q.halted.is_none()));
    assert!(s.next().is_none(), "{}", s.summary());
}

/// The cross-item rule between `gccp` and `mes` on the card's side: MES's pipes start only on a command processor
/// Linux's cp_resume made (a session that says `gccp` passed while it never ran meets the model's refusal).
#[test]
fn mes_does_not_start_before_the_command_processors_resume() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcCp));
    s.cp.passed = true;
    let (d, m) = rig.run(&mut s, Stage::Mes, m, &mut Stick::full());
    assert!(!d.pass && m.refused.iter().any(|r| r.contains("started before the command processor's resume")), "{} {:?}", d.line, m.refused);
    assert!(!m.mes.running.iter().any(|r| *r));
}

/// The kernel tests of the rings (`gx.rdna4.ring.*`), also here: their bodies need nothing of the kernel but its
/// log.
#[test]
fn the_kernel_tests_of_the_rings_hold_on_the_host_too() {
    crate::ktests_rdna4ring::stages().unwrap();
    crate::ktests_rdna4ring::rungs().unwrap();
    crate::ktests_rdna4ring::refusals().unwrap();
    crate::ktests_rdna4ring::reload().unwrap();
}

/// A queue whose last run waits in a poll (a WAIT_REG_MEM on a slot that does not hold its value) shows busy in
/// GRBM_STATUS and CP_STAT without being halted, and idle again once a later doorbell's run gets past the poll.
#[test]
fn a_queue_waiting_in_a_poll_shows_busy_until_the_poll_is_satisfied() {
    let (rig, s, mut m) = mesq_up();
    let n = super::rdna4cp::n_of(&rig);
    let rd = |m: &mut crate::rdna4::fakemodel::Model, r| m.reg_read(n.byte(r).unwrap()).unwrap();
    assert_eq!(rd(&mut m, rg::GRBM_STATUS), 0);
    let up = s.mes.up.as_ref().unwrap();
    let q = kq(&s, 4);
    let mut v = Vec::new();
    amdgpu_fmt::gfx12::pipeline_sync(&mut v, true, up.place.slot_mc(q.slots.fence), 0x1234).unwrap();
    let (rung, wptr) = super::rdna4cpeng::submit(&s, &mut m, 4, 0, &v);
    assert!(rung && m.cp.queues.iter().all(|q| q.halted.is_none()), "{:?}", m.refused);
    assert!(m.cp.queues.iter().any(|q| q.waits.as_deref().is_some_and(|w| w.starts_with("waits in a poll"))));
    assert_eq!((rd(&mut m, rg::GRBM_STATUS), rd(&mut m, rg::CP_STAT)), (rg::f::GUI_ACTIVE.mask | rg::f::CP_BUSY.mask, rg::f::CP_STAT_BUSY.mask));
    assert!(m.sys_write(up.place.slot_sys(q.slots.fence), &0x1234u32.to_le_bytes()));
    let (rung, _) = super::rdna4cpeng::submit(&s, &mut m, 4, wptr, &[amdgpu_fmt::pm4::PAD]);
    assert!(rung && m.cp.queues.iter().all(|q| q.waits.is_none() && q.halted.is_none()), "{:?}", m.refused);
    assert_eq!(rd(&mut m, rg::GRBM_STATUS), 0);
}

/// The section's registers are Linux's only once GRBM_GFX_CNTL selects pipe 0 again (`soc24_grbm_select(0, 0, 0,
/// 0)` after each queue's setup, gfx_v12_0.c:3113, :3483).
#[test]
fn the_sections_registers_are_refused_while_a_pipe_is_still_selected() {
    let (rig, _s, mut m) = mesq_up();
    let n = super::rdna4cp::n_of(&rig);
    let grbm = n.byte(rdna4_core::gc12::GRBM_GFX_CNTL).unwrap();
    assert!(m.reg_write(grbm, 0x4), "{:?}", m.refused);
    for (r, v) in [(rg::CP_MAX_CONTEXT, 7), (rg::CP_ME1_PIPE0_INT_CNTL, rg::f::PRIV_REG_INT_ENABLE.mask)] {
        assert!(!m.reg_write(n.byte(r).unwrap(), v) && m.refused.pop().unwrap().contains("Linux selects pipe 0 again first"), "{}", r.name);
    }
    assert!(m.reg_write(grbm, 0) && m.reg_write(n.byte(rg::CP_MAX_CONTEXT).unwrap(), 7), "{:?}", m.refused);
}

/// The CP's fault sources have their taker from `gcring` on, as Linux registers them in sw_init before hw_init
/// enables them: a bad opcode on a kernel ring in a boot that stops after `gcring` (no `ringtest`) counts as that
/// ring's fault at the IH reader, not as a source no block handles.
#[test]
fn a_fault_after_gcring_alone_counts_as_its_rings() {
    let (rig, mut s, m) = mesq_up();
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::GcRing]);
    assert!(s.rings.rungs.is_empty() && !s.rings.eop_on, "no ringtest in this boot");
    let (rung, _) = super::rdna4cpeng::submit(&s, &mut m, 0, s.rings.wptr[0], &[amdgpu_fmt::pm4::packet3(0x84, 0), 0]);
    assert!(rung && m.cp.raised == [(rg::SRC_BAD_OPCODE, crate::rdna4::ringih::ring_id(&KERNEL_QUEUES[0]))], "{:?} {:?}", m.cp.raised, m.refused);
    let n = super::rdna4cp::n_of(&rig);
    let ih = s.nb.ih.as_mut().unwrap();
    assert_eq!(crate::rdna4::sdmaih::read(ih, &mut m, &n)[0].entries, 1);
    assert_eq!(crate::rdna4::ringih::faults(ih, &KERNEL_QUEUES[0]), 1);
    assert!(KERNEL_QUEUES[1..].iter().all(|k| crate::rdna4::ringih::faults(ih, k) == 0));
    assert!(!ih.reader.unclaimed.contains_key(&(rg::CLIENT_GRBM_CP, rg::SRC_BAD_OPCODE)), "{:?}", ih.reader.unclaimed);
}
