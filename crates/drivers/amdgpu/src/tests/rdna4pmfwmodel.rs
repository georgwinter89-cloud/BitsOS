//! Wave 43: what keeps anything but Linux's start from the card's power controller, each shown by doing the
//! thing Linux would not do: the guard of the stage `pmfw` with its gate, and the model's power controller on its
//! own account (`rdna4::fakepmfw`). The kernel tests of the stage run here too.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use super::rdna4pmfw::{last, loaded, raw, raw_start};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakepmfw::FEATURES;
use crate::rdna4::guard::{Guard, Refusal};
use crate::rdna4::regs14::Regs14;
use crate::rdna4::stage_pmfw;
use rdna4_core::refuse::{SMU_NEVER, SMU_START};
use rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS;
use rdna4_core::smugate::Gate;
use rdna4_core::smuseq::{Script, Tables};

/// The stage's guard with its gate, over the model as the chain left it: what the gate lets through is tested in
/// `rdna4_core`; here that the guard asks it, feeds it and keeps a refused write from the card.
#[test]
fn the_stages_guard_keeps_what_is_not_linuxs_next_step_from_the_card() {
    let rig = Rig::new();
    let (s, m) = loaded(&rig);
    let r = regs(&rig);
    let l = s.layout.clone().unwrap();
    let tables = Tables { driver_mc: l.mc(0x600_0000), tools_mc: l.mc(0x610_0000) };
    let gate = || Some(std::boxed::Box::new(Gate::new(Script::new(tables, (4, 6)))));
    let guard = |m: Model| Guard::new(crate::rdna4::guard::Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes }, stage_pmfw::allow(&r), r.clone()).with_smu(gate());
    let why = |g: &Guard<crate::rdna4::guard::Mc<Model>>| match g.refused.last() {
        Some(Refusal::Reg { why, .. }) => *why,
        other => panic!("{other:?}"),
    };
    let mut g = guard(m);
    let writes = g.hw.hw.writes.len();
    // The first message is GetDriverIfVersion with argument 0, and nothing else.
    assert!(g.reg_write(r.smu_answer, 0));
    assert!(!g.reg_write(r.smu_argument, 1) && why(&g).starts_with("not the argument"));
    assert!(g.reg_write(r.smu_argument, 0));
    for (m, name) in SMU_NEVER {
        assert!(!g.reg_write(r.smu_message, u32::from(m)) && why(&g) == "an SMU message on the never list", "{name}");
    }
    for (m, name) in SMU_START {
        assert!(!g.reg_write(r.smu_message, u32::from(m)) && why(&g).starts_with("an SMU message out of Linux's order"), "{name}");
    }
    assert!(!g.reg_write(r.smu_message, 0x28) && why(&g).starts_with("not a message of Linux's start"));
    assert!(g.reg_write(r.smu_message, 0x03));
    // The guard showed the gate the answer: the next message opens only after the value and the pre-poll.
    assert!(!g.reg_write(r.smu_answer, 0) && why(&g).contains("has no answer"));
    assert_eq!((g.reg_read(r.smu_answer), g.reg_read(r.smu_argument)), (Some(1), Some(0x33)));
    assert!(!g.reg_write(r.smu_answer, 0) && why(&g).contains("before the answer register was read as answered"));
    assert_eq!(g.reg_read(r.smu_answer), Some(1));
    assert!(g.reg_write(r.smu_answer, 0) && g.reg_write(r.smu_argument, 0) && g.reg_write(r.smu_message, 0x02));
    // The thermal alert's registers are on the stage's list and shut until their step.
    assert!(!g.reg_write(r.thm_int_ena, 0x38) && why(&g).contains("only at their step"));
    // Nothing that was refused reached the model, and the model refused nothing of what passed.
    let m = g.hw.hw;
    assert_eq!(m.writes.len() - writes, 6);
    assert!(m.refused.is_empty() && m.messages == [(0x03, 0), (0x02, 0)], "{:?}", m.refused);
    // Without the gate the same guard takes questions only: the stage's list alone opens no message of the start.
    let mut g = Guard::new(m, stage_pmfw::allow(&r), r.clone());
    assert_eq!(g.reg_read(r.smu_answer), Some(1));
    assert!(g.reg_write(r.smu_answer, 0) && g.reg_write(r.smu_argument, 0));
    for (m, name) in SMU_START {
        assert!(!g.reg_write(r.smu_message, u32::from(m)), "{name}");
    }
    // The stage's list holds no memory and no register of another block.
    let allow = stage_pmfw::allow(&r);
    assert!(allow.mem.is_empty() && allow.regs.len() == 8 && !allow.regs.contains(&r.rsmu_data) && !allow.regs.contains(&r.psp.c2pmsg_67));
}

fn fresh(rig: &Rig) -> (Model, Regs14) {
    (loaded(rig).1, regs(rig))
}

#[test]
fn the_model_takes_linuxs_messages_only_as_linux_sends_them() {
    let rig = Rig::new();
    let (mut m, _) = fresh(&rig);
    raw_start(&mut m, 12);
    assert!(m.gc_powered() && m.pmfw.pcie == [0x406, 0x1_0406, 0x2_0406] && m.pmfw.btc && m.pmfw.transferred);
    // A message of the start before the secure processor loaded the SMU's firmware in this boot.
    for (msg, name) in SMU_START {
        let mut cold = model();
        assert!(!raw(&mut cold, u32::from(msg), 0) && last(&cold).contains("before the secure processor loaded"), "{name}: {:?}", cold.refused);
    }
    // The look's questions pass on a cold card.
    let mut cold = model();
    assert!(raw(&mut cold, 0x02, 0) && raw(&mut cold, 0x0D, 0) && cold.refused.is_empty());
    // Another argument than Linux's, for each message that takes 0.
    for (upto, msg) in [(7, 0x36), (11, 0x06), (12, 0x2A)] {
        let (mut m, _) = fresh(&rig);
        raw_start(&mut m, upto);
        assert!(!raw(&mut m, msg, 1) && last(&m).contains("Linux sends 0"), "{msg:#x}: {:?}", m.refused);
        assert!(raw(&mut m, msg, 0), "{msg:#x}: {:?}", m.refused);
    }
    // The transfer: only table 1, only after both tables' addresses.
    let (mut m, _) = fresh(&rig);
    raw_start(&mut m, 6);
    assert!(!raw(&mut m, 0x12, 0) && !raw(&mut m, 0x12, 0x1_0001) && !raw(&mut m, 0x12, 2) && last(&m).contains("table 1"));
    for upto in [2, 3, 4, 5] {
        let (mut m, _) = fresh(&rig);
        raw_start(&mut m, upto);
        assert!(!raw(&mut m, 0x12, 1) && last(&m).contains("before the driver table's and the tools table's addresses were set"), "after {upto}: {:?}", m.refused);
    }
    // A table's address: the high half first, a page of free VRAM, not over the other table or the GART table.
    let (mut m, _) = fresh(&rig);
    raw_start(&mut m, 2);
    assert!(!raw(&mut m, 0x0F, 0x0600_0000) && last(&m).contains("low half of a table's address before its high half"));
    assert!(raw(&mut m, 0x0E, 0x80));
    let held = m.nb.vram_held()[0].1.start as u32;
    for bad in [0x0600_0800, 0x0000_1000, held] {
        assert!(!raw(&mut m, 0x0F, bad) && last(&m).contains("not on a page of free VRAM of its own"), "{bad:#x}: {:?}", m.refused);
    }
    assert!(raw(&mut m, 0x0F, 0x0600_0000) && raw(&mut m, 0x10, 0x80));
    assert!(!raw(&mut m, 0x11, 0x0600_2000) && last(&m).contains("of its own"), "the tools table over the driver table");
    assert!(!raw(&mut m, 0x11, 0x05FF_0000), "its 100 KiB reach into the driver table");
    assert!(raw(&mut m, 0x11, 0x0610_0000));
    let (mut m, _) = fresh(&rig);
    raw_start(&mut m, 2);
    assert!(raw(&mut m, 0x0E, 0x90) && !raw(&mut m, 0x0F, 0), "outside VRAM");
    // RunDcBtc: after the transfer, once.
    let (mut m, _) = fresh(&rig);
    raw_start(&mut m, 6);
    assert!(!raw(&mut m, 0x36, 0) && last(&m).contains("before the PMFW's table was transferred"));
    assert!(raw(&mut m, 0x12, 1) && raw(&mut m, 0x36, 0));
    assert!(!raw(&mut m, 0x36, 0) && last(&m) == "a second RunDcBtc");
    // The link levels: after RunDcBtc, in order, each with Linux's argument, no fourth.
    assert!(!raw(&mut m, 0x20, 0x1_0406) && !raw(&mut m, 0x20, 0x0405) && !raw(&mut m, 0x20, 0x0306) && last(&m).contains("Linux sends 0x406"));
    assert!(raw(&mut m, 0x20, 0x0406) && !raw(&mut m, 0x20, 0x0406) && raw(&mut m, 0x20, 0x1_0406));
    // EnableAllSmuFeatures: after every level, once.
    assert!(!raw(&mut m, 0x06, 0) && last(&m).contains("before RunDcBtc and every link level"));
    assert!(raw(&mut m, 0x20, 0x2_0406) && !raw(&mut m, 0x20, 0x3_0406));
    let (mut early, _) = fresh(&rig);
    raw_start(&mut early, 7);
    assert!(!raw(&mut early, 0x06, 0) && !raw(&mut early, 0x20, 0x0406), "neither before RunDcBtc");
    assert!(!m.gc_powered());
    assert!(raw(&mut m, 0x06, 0));
    let answer = m.regs.smu_answer;
    while m.reg_read(answer) == Some(0) {}
    assert!(m.gc_powered());
    assert!(!raw(&mut m, 0x06, 0) && last(&m) == "a second EnableAllSmuFeatures");
    assert!(!raw(&mut m, 0x20, 0x0406), "no link level once the features are on");
    // The never-messages at any point.
    for (msg, name) in SMU_NEVER {
        assert!(!raw(&mut m, u32::from(msg), 0) && last(&m).contains("never list"), "{name}");
    }
    assert!(!raw(&mut m, 0x28, 0) && last(&m).contains("nobody reviewed"), "AllowGfxOff");
}

#[test]
fn the_model_holds_the_video_block_the_clock_questions_and_the_thermal_alert_to_linuxs_order() {
    let rig = Rig::new();
    let (mut m, r) = fresh(&rig);
    // Before the features are on: no power message, no thermal register.
    raw_start(&mut m, 11);
    assert!(!raw(&mut m, 0x2A, 0) && last(&m).contains("not in Linux's order"));
    assert!(!m.reg_write(r.thm_int_ctrl, 0xB8FF_0076) && last(&m).contains("before the features are on"));
    assert!(!raw(&mut m, 0x4D, 0));
    assert!(raw(&mut m, 0x06, 0));
    while m.reg_read(r.smu_answer) == Some(0) {}
    // VCN up, JPEG up, JPEG down, VCN down: no other order, none twice.
    assert!(!raw(&mut m, 0x2C, 0) && !raw(&mut m, 0x2D, 0) && !raw(&mut m, 0x2B, 0));
    // The video clocks' tables need the video block powered; the others do not.
    assert!(raw(&mut m, 0x1F, 0x1_00FF) && m.c82 == 0x8000_0002);
    assert!(!raw(&mut m, 0x1F, 0x5_00FF) && last(&m).contains("while the video block is not powered up"));
    assert!(raw(&mut m, 0x2A, 0) && !raw(&mut m, 0x2A, 0) && !raw(&mut m, 0x2B, 0x1_0000));
    assert!(!raw(&mut m, 0x1F, 0x4_00FF), "JPEG is not up yet");
    assert!(raw(&mut m, 0x2C, 0) && !raw(&mut m, 0x2C, 0) && !raw(&mut m, 0x2B, 0), "VCN stays up while JPEG is");
    // The clock questions: the trace's answers; a level past the count, a clock Linux does not ask about.
    for (arg, want) in [(0x1_0000, 0x8000_01A1), (0xFF, 0x8000_0002), (1, 0x8000_0D7A), (0x2_00FF, 6), (0x2_0005, 0x4EA), (0x5_0001, 0x8000_0B76), (0x4_0001, 0x8000_0898), (0x9_0000, 0x8000_0093)] {
        assert!(raw(&mut m, 0x1F, arg) && m.c82 == want, "{arg:#x}: {:#x} {:?}", m.c82, m.refused);
    }
    for (arg, why) in [(0x1_0002, "past its 2 levels"), (0x2_0006, "past its 6 levels"), (0x6_00FF, "which Linux's start does not ask about"), (0xA_0000, "does not ask about")] {
        assert!(!raw(&mut m, 0x1F, arg) && last(&m).contains(why), "{arg:#x}: {:?}", m.refused.last());
    }
    // A clock whose feature does not run.
    m.pmfw.features &= !(1 << 3);
    assert!(!raw(&mut m, 0x1F, 0x2_00FF) && last(&m).contains("UCLK, whose feature does not run"));
    m.pmfw.features = FEATURES;
    assert!(raw(&mut m, 0x2D, 0) && !raw(&mut m, 0x2D, 0) && raw(&mut m, 0x2B, 0) && !raw(&mut m, 0x2B, 0));
    // The thermal alert: Linux's order, Linux's change, then AllowIHHostInterrupt, once.
    m.refused.clear();
    assert!(!raw(&mut m, 0x4D, 0) && last(&m).contains("after the features are on and the thermal alert's four registers"));
    assert!(!m.reg_write(r.thm_int_ena, 0x38) && !m.reg_write(r.mp1_ih_sw_int, 0xFE) && !m.reg_write(r.mp1_ih_sw_int_ctrl, 0) && last(&m).contains("out of Linux's order"));
    for wrong in [0x2FFF_0071, 0xB8FF_0077, 0xBCFF_0076, 0xA8FF_0076] {
        assert!(!m.reg_write(r.thm_int_ctrl, wrong) && last(&m).contains("Linux's change of what it holds is 0xb8ff0076"), "{wrong:#x}");
    }
    assert!(m.reg_write(r.thm_int_ctrl, 0xB8FF_0076) && !m.reg_write(r.thm_int_ctrl, 0xB8FF_0076));
    assert!(!m.reg_write(r.thm_int_ena, 0x3F) && m.reg_write(r.thm_int_ena, 0x38));
    assert!(!m.reg_write(r.mp1_ih_sw_int, 0x1FE) && m.reg_write(r.mp1_ih_sw_int, 0xFE));
    assert!(!raw(&mut m, 0x4D, 0), "three of the four");
    assert!(!m.reg_write(r.mp1_ih_sw_int_ctrl, 1) && m.reg_write(r.mp1_ih_sw_int_ctrl, 0));
    assert!(!raw(&mut m, 0x4D, 1) && raw(&mut m, 0x4D, 0) && !raw(&mut m, 0x4D, 0));
}

#[test]
fn the_model_takes_a_message_only_after_the_answer_register_was_read_as_answered() {
    let rig = Rig::new();
    let (mut m, r) = fresh(&rig);
    // The first message since the firmware started needs no look (and the register reads 0 then).
    assert_eq!(m.reg_read(r.smu_answer), Some(0));
    assert!(m.reg_write(r.smu_answer, 0) && m.reg_write(r.smu_argument, 0) && m.reg_write(r.smu_message, 0x03));
    // The next one without a read of the answer register since.
    assert!(!m.reg_write(r.smu_answer, 0) && last(&m).contains("was not read as answered since the one before"));
    // Its value alone does not count: the answer register must have been read.
    assert_eq!(m.reg_read(r.smu_argument), Some(0x33));
    assert!(!m.reg_write(r.smu_answer, 0));
    // Read as answered: taken (that Linux reads it once more right before the message is the trace's to hold).
    assert_eq!(m.reg_read(r.smu_answer), Some(1));
    assert_eq!(m.reg_read(r.smu_argument), Some(0x33));
    assert!(m.reg_write(r.smu_answer, 0) && m.reg_write(r.smu_argument, 0) && m.reg_write(r.smu_message, 0x02));
    // One read answers for one message only.
    assert!(!m.reg_write(r.smu_answer, 0));
    assert_eq!(m.reg_read(r.smu_answer), Some(1));
    // While a message has no answer the register reads 0, and reading 0 opens nothing.
    m.smu_silent_on = Some(0x0D);
    assert!(raw(&mut m, 0x0D, 0));
    assert_eq!(m.reg_read(r.smu_answer), Some(0));
    assert!(!m.reg_write(r.smu_answer, 0));
    // The mailbox's own order: the argument after the 0, the message after both; the answer register takes 0 only.
    let (mut m, r) = fresh(&rig);
    assert!(!m.reg_write(r.smu_argument, 0) && !m.reg_write(r.smu_message, 0x03) && !m.reg_write(r.smu_answer, 1));
    assert!(m.reg_write(r.smu_answer, 0) && !m.reg_write(r.smu_message, 0x03) && last(&m).contains("before answer 0 and argument"));
    // The index pair reaches the firmware flags and nothing else, and nothing is put back into it.
    assert!(!m.reg_write(r.rsmu_index, 0x1234_0000) && !m.reg_write(r.rsmu_index, 0x0301_0028) && m.reg_write(r.rsmu_index, SMN_MP1_FIRMWARE_FLAGS));
}

/// The kernel tests of the stage (`gx.rdna4.pmfw`, `gx.rdna4.pmfw_refusals`, `gx.rdna4.pmfw_stops`), also here:
/// their bodies need nothing of the kernel but its log.
#[test]
fn the_kernel_tests_of_the_stage_hold_on_the_host_too() {
    crate::ktests_rdna4pmfw::start().unwrap();
    crate::ktests_rdna4pmfw::refusals().unwrap();
    crate::ktests_rdna4pmfw::stops().unwrap();
}
