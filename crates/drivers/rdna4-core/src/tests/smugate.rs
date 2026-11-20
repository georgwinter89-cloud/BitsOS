//! Wave 43: the gate that opens the SMU's mailbox for Linux's start of the power controller and for nothing else
//! (`smugate::Gate`), driven as the bus would drive it: each test does what Linux would not do and is refused.

use super::smuseq::{CAPS, FEATURES, TABLES, TRACE, play, table, table_bytes, value};
use crate::pptable14::{PcieLevels, Pptable};
use crate::refuse::{self, SMU_NEVER, SMU_QUESTIONS, SMU_START, Verdict, write_refused, write_refused_with};
use crate::regmap::{SMU_ANSWER, SMU_ARGUMENT, SMU_MESSAGE};
use crate::smugate::{Gate, Judged};
use crate::smuseq::{Script, Step, Stopped};
use crate::thm14;
use std::vec::Vec;

/// One message through the gate as Linux's `smu_msg_v1_send_msg` puts it on the bus. `Err`: the write that was
/// refused, with the reason.
fn send(g: &mut Gate, first: bool, m: u32, arg: u32, answer: u32, val: Option<u32>) -> Result<(), (&'static str, &'static str)> {
    if !first {
        g.saw_read(SMU_ANSWER, 1);
    }
    for (reg, v, what) in [(SMU_ANSWER, 0, "answer"), (SMU_ARGUMENT, arg, "argument"), (SMU_MESSAGE, m, "message")] {
        match g.judge(reg, v) {
            Judged::Pass => g.wrote(reg, v),
            Judged::Refused(why) => return Err((what, why)),
            Judged::NotMine => panic!("the gate does not know {}", reg.name),
        }
    }
    g.saw_read(SMU_ANSWER, 0);
    g.saw_read(SMU_ANSWER, answer);
    if let Some(v) = val {
        g.saw_read(SMU_ARGUMENT, v);
    }
    Ok(())
}

/// The gate's next step put on the bus with the card's answers; `false` at the script's end.
fn step(g: &mut Gate, first: &mut bool) -> bool {
    match g.script().next() {
        Step::End => return false,
        Step::Table => g.saw_memory(TABLES.driver_mc, &table_bytes(&table())),
        Step::Thermal => thermal(g).unwrap(),
        Step::Send { msg: m, arg, value: wants } => {
            send(g, *first, u32::from(m), arg, 1, wants.then(|| value(m, arg, FEATURES))).unwrap_or_else(|e| panic!("{m:#x} {arg:#x}: {e:?}"));
            *first = false;
        }
    }
    true
}

/// The thermal alert's registers as Linux touches them (the trace's values).
fn thermal(g: &mut Gate) -> Result<(), &'static str> {
    let put = |g: &mut Gate, reg, v| match g.judge(reg, v) {
        Judged::Pass => {
            g.wrote(reg, v);
            Ok(())
        }
        Judged::Refused(why) => Err(why),
        Judged::NotMine => panic!("the gate does not know {}", reg.name),
    };
    g.saw_read(thm14::THERMAL_INT_CTRL, 0x2FFF_0071);
    put(g, thm14::THERMAL_INT_CTRL, 0xB8FF_0076)?;
    put(g, thm14::THERMAL_INT_ENA, 0x38)?;
    g.saw_read(thm14::MP1_IH_SW_INT, 0);
    put(g, thm14::MP1_IH_SW_INT, 0xFE)?;
    g.saw_read(thm14::MP1_IH_SW_INT_CTRL, 0);
    put(g, thm14::MP1_IH_SW_INT_CTRL, 0)
}

fn gate() -> Gate {
    Gate::new(Script::new(TABLES, CAPS))
}

/// The gate moved by the bus up to the step that sends `stop_at`.
fn gate_at(stop_at: (u16, u32)) -> (Gate, bool) {
    let (mut g, mut first) = (gate(), true);
    loop {
        if let Step::Send { msg: m, arg, .. } = g.script().next()
            && (m, arg) == stop_at
        {
            return (g, first);
        }
        assert!(step(&mut g, &mut first), "the script ended before {stop_at:x?}");
    }
}

#[test]
fn the_gate_lets_linuxs_whole_start_through_and_ends_where_the_script_does() {
    let (mut g, mut first) = (gate(), true);
    let mut steps = 0;
    while step(&mut g, &mut first) {
        steps += 1;
    }
    // 125 messages, the table, the thermal alert.
    assert_eq!(steps, 127);
    assert!(g.script().passed());
    // What the gate learned from the bus alone is what the stage's own script learns.
    assert_eq!(g.script().facts, play(FEATURES, &table()).0.facts);
    // Nothing after the end.
    g.saw_read(SMU_ANSWER, 1);
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Refused("no SMU message is the start's next step"));
}

/// Every message of the start that is no question, at the step where it is Linux's next: (message, argument).
const STARTS: [(u16, u32); 13] = [
    (0x0E, 0x80),
    (0x0F, 0x0531_A000),
    (0x10, 0x80),
    (0x11, 0x0530_1000),
    (0x12, 1),
    (0x36, 0),
    (0x20, 0x406),
    (0x06, 0),
    (0x2A, 0),
    (0x2C, 0),
    (0x2D, 0),
    (0x2B, 0),
    (0x4D, 0),
];

#[test]
fn a_message_passes_only_with_the_argument_linuxs_source_computes() {
    let mut all = STARTS.to_vec();
    all.extend([(0x20, 0x1_0406), (0x20, 0x2_0406), (0x1F, 0x1_00FF), (0x1F, 0x2_0003), (0x1F, 0x9_0001), (0x03, 0), (0x0D, 0)]);
    for (m, arg) in all {
        for wrong in [arg ^ 1, arg ^ 0x100, arg ^ 0x1_0000, arg.wrapping_add(0x1000), u32::MAX] {
            let (mut g, first) = gate_at((m, arg));
            let e = send(&mut g, first, u32::from(m), wrong, 1, None).unwrap_err();
            assert_eq!(e, ("argument", "not the argument Linux's source computes for the start's next message"), "{m:#x} {wrong:#x}");
        }
        // With Linux's argument it passes.
        let (mut g, first) = gate_at((m, arg));
        assert_eq!(send(&mut g, first, u32::from(m), arg, 1, None), Ok(()), "{m:#x}");
    }
}

#[test]
fn a_message_before_the_one_linux_sends_before_it_is_refused() {
    // At every step of the start every other message of the start is refused, and so is every question.
    let (mut g, mut first) = (gate(), true);
    let mut checked = 0;
    loop {
        if let Step::Send { msg: next, arg, .. } = g.script().next() {
            for (m, _) in SMU_START.iter().chain(SMU_QUESTIONS.iter()).filter(|(m, _)| *m != next) {
                let mut probe = g.clone();
                let e = send(&mut probe, first, u32::from(*m), arg, 1, None).unwrap_err();
                assert_eq!(e.0, "message", "{m:#x} where {next:#x} is next");
                assert_eq!(e.1, "an SMU message out of Linux's order: another one is the start's next", "{m:#x} where {next:#x} is next");
                checked += 1;
            }
        }
        if !step(&mut g, &mut first) {
            break;
        }
    }
    assert_eq!(checked, 125 * (13 + 11 - 1));
}

#[test]
fn enable_all_features_goes_out_once() {
    let (mut g, first) = gate_at((0x06, 0));
    assert_eq!(send(&mut g, first, 0x06, 0, 1, None), Ok(()));
    assert!(g.script().facts.enabled);
    // A second one: the features pair is next.
    assert_eq!(send(&mut g.clone(), false, 0x06, 0, 1, None).unwrap_err().0, "message");
    // And at every later step of the start.
    let mut first = false;
    while step(&mut g, &mut first) {
        if matches!(g.script().next(), Step::Send { arg: 0, .. }) {
            assert!(send(&mut g.clone(), false, 0x06, 0, 1, None).is_err());
        }
    }
}

#[test]
fn no_gate_opens_a_message_of_the_never_list_or_one_nobody_reviewed() {
    let (mut g, mut first) = (gate(), true);
    loop {
        if let Step::Send { arg, .. } = g.script().next() {
            for (m, name) in SMU_NEVER {
                let e = send(&mut g.clone(), first, u32::from(m), arg, 1, None).unwrap_err();
                assert_eq!(e, ("message", "an SMU message on the never list"), "{name}");
            }
            for m in [0x00, 0x04, 0x05, 0x08, 0x09, 0x16, 0x21, 0x24, 0x28, 0x29, 0x35, 0x50, 0x52, 0x58 + 1, 0x1_0006] {
                let e = send(&mut g.clone(), first, m, arg, 1, None).unwrap_err();
                assert_eq!(e.0, "message", "{m:#x}");
            }
        }
        if !step(&mut g, &mut first) {
            break;
        }
    }
    // SetPptLimit is on the never list, and stays there whatever a later wave's section shows.
    assert_eq!(refuse::smu_verdict(0x32), Verdict::Never("SetPptLimit"));
}

#[test]
fn a_clock_question_is_held_to_the_answered_count_and_the_running_features() {
    // SOCCLK answered two levels: level 2 is past them.
    let (mut g, _) = gate_at((0x1F, 0x1_0001));
    assert!(send(&mut g.clone(), false, 0x1F, 0x1_0002, 1, None).is_err());
    assert!(send(&mut g.clone(), false, 0x1F, 0x1_00FF, 1, None).is_err(), "the count was asked already");
    assert_eq!(send(&mut g, false, 0x1F, 0x1_0001, 1, Some(0x8000_05C4)), Ok(()));
    // After SOCCLK's last level GFXCLK's feature question is next, not a third level and not another clock.
    assert!(send(&mut g.clone(), false, 0x1F, 0x1_0002, 1, None).is_err());
    assert!(send(&mut g.clone(), false, 0x1F, 0xFF, 1, None).is_err());
    // A clock whose feature the PMFW said does not run: its questions never become the next step.
    let mut g = gate();
    let mut first = true;
    let off = FEATURES & !(1 << 3);
    loop {
        match g.script().next() {
            Step::End => break,
            Step::Table => g.saw_memory(TABLES.driver_mc, &table_bytes(&table())),
            Step::Thermal => thermal(&mut g).unwrap(),
            Step::Send { msg: m, arg, value: wants } => {
                for probe in [0x2_00FF, 0x2_0000, 0x2_0005] {
                    assert!(arg == probe || send(&mut g.clone(), first, 0x1F, probe, 1, None).is_err(), "UCLK {probe:#x} at {m:#x}");
                }
                send(&mut g, first, u32::from(m), arg, 1, wants.then(|| value(m, arg, off))).unwrap();
                first = false;
            }
        }
    }
    assert!(g.script().passed());
}

#[test]
fn the_mailbox_is_written_only_in_linuxs_order() {
    // The first message needs no look at the answer register (SMU_FW_INIT, smu_cmn.c:508-511).
    let mut g = gate();
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Pass);
    // The answer register takes 0 only; the argument only after it; the message only after both.
    assert_eq!(g.judge(SMU_ANSWER, 1), Judged::Refused("the SMU's answer register is written with 0 only"));
    assert_eq!(g.judge(SMU_ARGUMENT, 0), Judged::Refused("the SMU's argument before 0 went into the answer register"));
    assert_eq!(g.judge(SMU_MESSAGE, 0x03), Judged::Refused("an SMU message before answer 0 and its argument went"));
    g.wrote(SMU_ANSWER, 0);
    assert_eq!(g.judge(SMU_MESSAGE, 0x03), Judged::Refused("an SMU message before answer 0 and its argument went"));
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Refused("a new SMU message before the answer register was read as answered"));
    g.wrote(SMU_ARGUMENT, 0);
    assert_eq!(g.judge(SMU_MESSAGE, 0x03), Judged::Pass);
    g.wrote(SMU_MESSAGE, 0x03);
    // No new message while this one has no answer, nor before the answer register was read as answered again.
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Refused("a new SMU message while the one before has no answer"));
    g.saw_read(SMU_ANSWER, 0);
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Refused("a new SMU message while the one before has no answer"));
    g.saw_read(SMU_ANSWER, 1);
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Refused("a new SMU message while the one before has no answer"), "Linux reads the value first");
    g.saw_read(SMU_ARGUMENT, 0x33);
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Refused("a new SMU message before the answer register was read as answered"));
    // A pre-poll that sees 0, or a code Linux takes for a hang, opens nothing; OK and the PMFW's error codes do.
    for (seen, opens) in [(0, false), (0xFB, false), (u32::MAX, false), (1, true), (0xFF, true), (0xFE, true), (0xFD, true), (0xFC, true)] {
        let mut probe = g.clone();
        probe.saw_read(SMU_ANSWER, seen);
        assert_eq!(probe.judge(SMU_ANSWER, 0) == Judged::Pass, opens, "{seen:#x}");
    }
    // A message that was not answered OK stops the gate's script: nothing of the start follows.
    let (mut g, first) = gate_at((0x36, 0));
    send(&mut g, first, 0x36, 0, 0xFF, None).unwrap();
    assert_eq!(g.script().stopped(), Some(Stopped::Answer { msg: 0x36, arg: 0, answer: 0xFF }));
    assert!(send(&mut g, false, 0x20, 0x406, 1, None).is_err());
    // One that never answers: the gate saw no answer, so no further write passes.
    let (mut g, first) = gate_at((0x06, 0));
    assert!(!first);
    g.saw_read(SMU_ANSWER, 1);
    for (reg, v) in [(SMU_ANSWER, 0), (SMU_ARGUMENT, 0), (SMU_MESSAGE, 0x06)] {
        assert_eq!(g.judge(reg, v), Judged::Pass);
        g.wrote(reg, v);
    }
    for _ in 0..10 {
        g.saw_read(SMU_ANSWER, 0);
    }
    assert!(g.script().facts.enable_sent && !g.script().facts.enabled);
    assert_eq!(g.judge(SMU_ANSWER, 0), Judged::Refused("a new SMU message while the one before has no answer"));
}

#[test]
fn the_table_counts_only_as_the_driver_table_read_whole() {
    let (mut g, first) = gate_at((0x12, 1));
    send(&mut g, first, 0x12, 1, 1, None).unwrap();
    assert_eq!(g.script().next(), Step::Table);
    // No message before the table was read out of VRAM.
    assert_eq!(send(&mut g.clone(), false, 0x36, 0, 1, None).unwrap_err().1, "no SMU message is the start's next step");
    let bytes = table_bytes(&table());
    // Another place, or a part of it, is not the driver table.
    g.saw_memory(TABLES.tools_mc, &bytes);
    g.saw_memory(TABLES.driver_mc, &bytes[..0x2000]);
    g.saw_memory(TABLES.driver_mc + 0x1000, &bytes);
    assert_eq!(g.script().next(), Step::Table);
    g.saw_memory(TABLES.driver_mc, &bytes);
    assert_eq!(g.script().next(), Step::Send { msg: 0x36, arg: 0, value: false });
    // The link levels come from the table the gate itself saw: another table, other arguments.
    let (mut g, first) = gate_at((0x12, 1));
    send(&mut g, first, 0x12, 1, 1, None).unwrap();
    let narrow = Pptable { pcie: PcieLevels { gen_speed: [0, 2, 3], lane_count: [6, 6, 4], lclk_freq: [310, 619, 619] }, ..table() };
    g.saw_memory(TABLES.driver_mc, &table_bytes(&narrow));
    send(&mut g, false, 0x36, 0, 1, None).unwrap();
    assert!(send(&mut g.clone(), false, 0x20, 0x406, 1, None).is_err());
    assert_eq!(send(&mut g, false, 0x20, 0x304, 1, None), Ok(()));
}

#[test]
fn the_thermal_alerts_registers_pass_only_at_their_step_in_linuxs_order_with_linuxs_change() {
    // Before the step: refused, read or not.
    let (mut g, _) = gate_at((0x2B, 0));
    g.saw_read(thm14::THERMAL_INT_CTRL, 0x2FFF_0071);
    assert_eq!(g.judge(thm14::THERMAL_INT_CTRL, 0xB8FF_0076), Judged::Refused("the thermal alert's registers are written only at their step of the start"));
    assert_eq!(g.judge(thm14::THERMAL_INT_ENA, 0x38), Judged::Refused("the thermal alert's registers are written only at their step of the start"));
    send(&mut g, false, 0x2B, 0, 1, None).unwrap();
    assert_eq!(g.script().next(), Step::Thermal);
    // No message before the four registers.
    assert_eq!(send(&mut g.clone(), false, 0x4D, 0, 1, None).unwrap_err().1, "no SMU message is the start's next step");
    // The control register before it was read, the enable before the control register.
    let order = "a register of the thermal alert out of Linux's order, or before it was read";
    assert_eq!(g.judge(thm14::THERMAL_INT_CTRL, 0xB8FF_0076), Judged::Refused(order));
    assert_eq!(g.judge(thm14::THERMAL_INT_ENA, 0x38), Judged::Refused(order));
    g.saw_read(thm14::THERMAL_INT_CTRL, 0x2FFF_0071);
    // Linux's change of what was read, with the table's shutdown temperature, and nothing else.
    let change = "not Linux's change of the thermal alert's register as it read";
    for wrong in [0x2FFF_0071, 0xB8FF_0077, 0xB8FF_0176, 0xBCFF_0076, 0xB9FF_0076, 0xA8FF_0076, 0x38FF_0076] {
        assert_eq!(g.judge(thm14::THERMAL_INT_CTRL, wrong), Judged::Refused(change), "{wrong:#x}");
    }
    assert_eq!(g.judge(thm14::THERMAL_INT_CTRL, 0xB8FF_0076), Judged::Pass);
    g.wrote(thm14::THERMAL_INT_CTRL, 0xB8FF_0076);
    assert_eq!(g.judge(thm14::THERMAL_INT_CTRL, 0xB8FF_0076), Judged::Refused(order), "a second time");
    assert_eq!(g.judge(thm14::THERMAL_INT_ENA, 0x3F), Judged::Refused(change));
    assert_eq!(g.judge(thm14::MP1_IH_SW_INT, 0xFE), Judged::Refused(order));
    g.wrote(thm14::THERMAL_INT_ENA, 0x38);
    assert_eq!(g.judge(thm14::MP1_IH_SW_INT, 0xFE), Judged::Refused(order), "not read yet");
    g.saw_read(thm14::MP1_IH_SW_INT, 0x1AB);
    assert_eq!(g.judge(thm14::MP1_IH_SW_INT, 0x1FE), Judged::Refused(change), "VALID is cleared");
    assert_eq!(g.judge(thm14::MP1_IH_SW_INT, 0xFE), Judged::Pass);
    g.wrote(thm14::MP1_IH_SW_INT, 0xFE);
    g.saw_read(thm14::MP1_IH_SW_INT_CTRL, 0x101);
    assert_eq!(g.judge(thm14::MP1_IH_SW_INT_CTRL, 0x101), Judged::Refused(change), "INT_MASK is cleared");
    assert_eq!(g.judge(thm14::MP1_IH_SW_INT_CTRL, 0x100), Judged::Pass);
    assert_eq!(g.script().next(), Step::Thermal, "until the last write reached the card");
    g.wrote(thm14::MP1_IH_SW_INT_CTRL, 0x100);
    assert_eq!(g.script().next(), Step::Send { msg: 0x4D, arg: 0, value: false });
    // And after the step they are shut again.
    assert_eq!(g.judge(thm14::THERMAL_INT_ENA, 0x38), Judged::Refused("the thermal alert's registers are written only at their step of the start"));
}

#[test]
fn after_a_stop_only_what_the_stop_leaves_to_do_passes() {
    let (mut g, _) = gate_at((0x1F, 0x2_0003));
    send(&mut g, false, 0x1F, 0x2_0003, 0xFF, None).unwrap();
    // Linux's err_out, then the read-back; nothing of the start, and not in another order.
    for (m, arg) in [(0x1F, 0x2_0003), (0x1F, 0x2_0004), (0x2B, 0), (0x0D, 0), (0x4D, 0), (0x06, 0)] {
        assert!(send(&mut g.clone(), false, m, arg, 1, None).is_err(), "{m:#x}");
    }
    send(&mut g, false, 0x2D, 0, 1, None).unwrap();
    assert!(send(&mut g.clone(), false, 0x2D, 0, 1, None).is_err());
    send(&mut g, false, 0x2B, 0, 1, None).unwrap();
    assert!(send(&mut g.clone(), false, 0x0C, 0, 1, None).is_err(), "the high half first");
    send(&mut g, false, 0x0D, 0, 1, Some(0x048C_F19E)).unwrap();
    send(&mut g, false, 0x0C, 0, 1, Some(0x38FF_FCFB)).unwrap();
    assert_eq!((g.script().facts.read_back, g.script().next()), (Some(FEATURES), Step::End));
    for (m, _) in SMU_START.iter().chain(SMU_QUESTIONS.iter()) {
        assert!(send(&mut g.clone(), false, u32::from(*m), 0, 1, None).is_err(), "{m:#x} after the end");
    }
}

#[test]
fn without_a_gate_the_starts_messages_stay_shut_and_with_one_the_rest_is_as_before() {
    for (m, name) in SMU_START {
        assert_eq!(refuse::smu_verdict(m), Verdict::Start(name));
        assert!(write_refused(SMU_MESSAGE, u32::from(m)).is_some(), "{name}");
        assert!(write_refused_with(SMU_MESSAGE, u32::from(m), None, None).is_some(), "{name}");
    }
    // The three lists are apart, and the trace's section holds no message outside them.
    for (m, name) in SMU_START {
        assert!(SMU_NEVER.iter().all(|(n, _)| *n != m) && SMU_QUESTIONS.iter().all(|(q, _)| *q != m) && m < refuse::SMU_MESSAGE_COUNT, "{name}");
    }
    assert!(TRACE.iter().all(|(_, m, _)| !matches!(refuse::smu_verdict(*m), Verdict::Never(_) | Verdict::NotReviewed | Verdict::Unknown)));
    let mut starts: Vec<u16> = TRACE.iter().map(|t| t.1).filter(|m| matches!(refuse::smu_verdict(*m), Verdict::Start(_))).collect();
    starts.sort();
    starts.dedup();
    assert_eq!(starts, SMU_START.map(|s| s.0), "exactly the messages Linux sends in this section are opened");
    // With a gate: questions too go only at their step; registers that are not the gate's are judged as before.
    let g = gate();
    assert!(write_refused_with(SMU_MESSAGE, 0x02, None, Some(&g)).is_some(), "GetSmuVersion before GetDriverIfVersion");
    assert_eq!(write_refused_with(SMU_ANSWER, 0, None, Some(&g)), None);
    assert!(write_refused_with(SMU_ANSWER, 0, None, None).is_none(), "the look's protocol");
    assert!(write_refused_with(crate::regmap::PSP_C2PMSG_64, refuse::GFX_CTRL_MODE1_RESET, None, Some(&g)).is_some());
    assert!(write_refused_with(crate::regmap::SMU_DEBUG_MESSAGE, 2, None, Some(&g)).is_some(), "Mode1Reset's mailbox");
    assert_eq!(write_refused_with(crate::regmap::PSP_C2PMSG_64, refuse::GFX_CTRL_INIT_GPCOM_RING, None, Some(&g)), None);
}
