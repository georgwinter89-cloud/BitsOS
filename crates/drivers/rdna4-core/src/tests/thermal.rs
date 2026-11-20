//! Wave 44: the thermal handler's rules and its gate (`thm14`, `thmgate`, `metrics14`), each refusal shown by
//! doing what Linux's handler would not do.

use crate::metrics14::{self, Temps};
use crate::regmap::{SMU_ANSWER, SMU_ARGUMENT, SMU_MESSAGE};
use crate::thm14::{self, Source};
use crate::thmgate::{Action, Gate};

#[test]
fn the_three_sources_are_linuxs_and_no_other() {
    assert_eq!(Source::of(0x0f, 0), Some(Source::High));
    assert_eq!(Source::of(0x0f, 1), Some(Source::Low));
    assert_eq!(Source::of(0x1f, 0xfe), Some(Source::Smu));
    for (c, s) in [(0x0f, 2), (0x0f, 5), (0x0f, 0xfe), (0x1f, 0), (0x1f, 3), (0x1e, 0xfe), (0x0a, 0)] {
        assert_eq!(Source::of(c, s), None, "client {c:#x} source {s:#x}");
    }
    for s in Source::ALL {
        assert_eq!(Source::of(s.ids().0, s.ids().1), Some(s));
    }
}

#[test]
fn the_handlers_arithmetic_is_linuxs() {
    // INT_ACK set, every other bit kept.
    assert_eq!(thm14::ack(0), 0x100);
    assert_eq!(thm14::ack(0x8000_0001), 0x8000_0101);
    // INTH replaced, the trigger's mask cleared, the rest kept: the trace's value with 118 -> 123.
    assert_eq!(thm14::rearm(0xb8ff_0076, 123), 0xb8ff_007b);
    assert_eq!(thm14::rearm(0xbcff_0076, 118), 0xb8ff_0076);
    assert_eq!(thm14::rearm(0, 0x1_23), 0x23);
    assert_eq!(thm14::high_fan_abnormal(118, 5), 123);
    assert_eq!(thm14::high_fan_abnormal(118, -10), 108);
    assert_eq!(thm14::high_fan_abnormal(250, 10), 255);
    // A negative sum wraps in Linux's u32 and so ends at 255.
    assert_eq!(thm14::high_fan_abnormal(3, -10), 255);
    assert_eq!(thm14::high_fan_recovery(118), 118);
    assert_eq!(thm14::high_fan_recovery(300), 255);
}

#[test]
fn the_software_ctf_check_powers_off_in_every_case_but_one() {
    // Linux's only "nothing": a limit, a read, and below it (amdgpu_smu.c:1318-1323).
    assert!(!thm14::swctf_powers_off(118, Some(117_999)));
    assert!(!thm14::swctf_powers_off(118, Some(0)));
    assert!(thm14::swctf_powers_off(118, Some(118_000)));
    assert!(thm14::swctf_powers_off(118, Some(130_000)));
    assert!(thm14::swctf_powers_off(118, None));
    assert!(thm14::swctf_powers_off(0, Some(20_000)));
    assert!(thm14::swctf_powers_off(0, None));
}

fn ack(g: &mut Gate, read: u32) -> Result<(), &'static str> {
    g.saw_read(thm14::MP1_IH_SW_INT_CTRL, read);
    g.judge(thm14::MP1_IH_SW_INT_CTRL, thm14::ack(read))?;
    g.wrote(thm14::MP1_IH_SW_INT_CTRL, thm14::ack(read));
    Ok(())
}

#[test]
fn an_acknowledge_is_a_read_and_linuxs_change_of_it() {
    let mut g = Gate::new(Action::Ack { ctx: 3, high: None });
    // Not before the read, not another value.
    assert!(g.judge(thm14::MP1_IH_SW_INT_CTRL, 0x100).is_err());
    g.saw_read(thm14::MP1_IH_SW_INT_CTRL, 0x2);
    assert!(g.judge(thm14::MP1_IH_SW_INT_CTRL, 0x100).is_err());
    assert!(g.judge(thm14::MP1_IH_SW_INT_CTRL, 0x103).is_err());
    assert!(g.judge(thm14::MP1_IH_SW_INT_CTRL, 0x102).is_ok());
    g.wrote(thm14::MP1_IH_SW_INT_CTRL, 0x102);
    assert!(g.done());
    // Another context: no threshold moves, nothing more passes.
    assert!(g.judge(thm14::THERMAL_INT_CTRL, 0).is_err());
    assert!(g.judge(thm14::MP1_IH_SW_INT_CTRL, 0x102).is_err());
}

#[test]
fn a_fan_case_moves_the_upper_threshold_after_the_acknowledge_only() {
    let mut g = Gate::new(Action::Ack { ctx: thm14::CTX_FAN_ABNORMAL, high: Some(123) });
    g.saw_read(thm14::THERMAL_INT_CTRL, 0xb8ff_0076);
    assert!(g.judge(thm14::THERMAL_INT_CTRL, 0xb8ff_007b).is_err(), "before the acknowledge");
    ack(&mut g, 0).unwrap();
    assert!(g.judge(thm14::THERMAL_INT_CTRL, 0xb8ff_007b).is_err(), "before its own read");
    g.saw_read(thm14::THERMAL_INT_CTRL, 0xbcff_0076);
    assert!(g.judge(thm14::THERMAL_INT_CTRL, 0xbcff_007b).is_err(), "the trigger's mask kept");
    assert!(g.judge(thm14::THERMAL_INT_CTRL, 0xb8ff_0080).is_err(), "another limit");
    assert!(g.judge(thm14::THERMAL_INT_CTRL, 0xb8ff_007b).is_ok());
    g.wrote(thm14::THERMAL_INT_CTRL, 0xb8ff_007b);
    assert!(g.done());
}

/// The sensors' read on the bus as `smu14::send` puts it: `Err` names the refused write.
fn metrics(g: &mut Gate, arg: u32, m: u32) -> Result<(), &'static str> {
    g.saw_read(SMU_ANSWER, 1);
    for (reg, v) in [(SMU_ANSWER, 0), (SMU_ARGUMENT, arg), (SMU_MESSAGE, m)] {
        g.judge(reg, v)?;
        g.wrote(reg, v);
    }
    Ok(())
}

#[test]
fn the_sensors_read_is_one_message_with_one_argument() {
    let mut g = Gate::new(Action::Metrics);
    assert!(g.judge(SMU_ANSWER, 0).is_err(), "before the answer register read as answered");
    metrics(&mut g, 5, 0x12).unwrap();
    assert!(!g.may_read_table(), "before the answer");
    g.saw_read(SMU_ANSWER, 0);
    g.saw_read(SMU_ANSWER, 1);
    assert!(g.may_read_table() && g.done());
    // Nothing more: no second message.
    assert!(g.judge(SMU_ANSWER, 0).is_err());
    for (arg, m) in [(8, 0x12), (1, 0x12), (0x300, 0x12), (0x10005, 0x12), (5, 0x13), (5, 0x0D), (5, 0x32), (0x14b, 0x32), (5, 0x06)] {
        let mut g = Gate::new(Action::Metrics);
        assert!(metrics(&mut g, arg, m).is_err(), "argument {arg:#x} message {m:#x}");
    }
    // A failed answer gives no table.
    let mut g = Gate::new(Action::Metrics);
    metrics(&mut g, 5, 0x12).unwrap();
    g.saw_read(SMU_ANSWER, 0xFF);
    assert!(g.done() && !g.may_read_table());
}

#[test]
fn no_action_opens_a_never_message_or_anything_else() {
    for a in [Action::Metrics, Action::Ack { ctx: 8, high: Some(118) }, Action::Ack { ctx: 2, high: None }] {
        let mut g = Gate::new(a);
        g.saw_read(SMU_ANSWER, 1);
        g.saw_read(thm14::MP1_IH_SW_INT_CTRL, 0);
        for m in crate::refuse::SMU_NEVER.iter().map(|(m, _)| u32::from(*m)) {
            assert_eq!(g.judge(SMU_MESSAGE, m), Err("an SMU message on the never list"), "{a:?} {m:#x}");
        }
        for r in [thm14::THERMAL_INT_ENA, thm14::MP1_IH_SW_INT, crate::regmap::RSMU_INDEX, crate::regmap::PSP_C2PMSG_64] {
            assert_eq!(g.judge(r, 0), Err("the thermal handler writes nothing else"), "{a:?} {}", r.name);
        }
    }
    // The acknowledge reads nothing but its registers; the read nothing but the answer.
    assert!(!Gate::new(Action::Ack { ctx: 2, high: None }).may_read(thm14::THERMAL_INT_CTRL));
    assert!(Gate::new(Action::Ack { ctx: 8, high: Some(1) }).may_read(thm14::THERMAL_INT_CTRL));
    assert!(!Gate::new(Action::Metrics).may_read(thm14::MP1_IH_SW_INT_CTRL));
}

#[test]
fn a_metrics_table_reads_by_its_documented_fields() {
    let t = Temps { edge: 41, hotspot: 55, mem: 48, counter: 7, ..Temps::default() };
    let mut b = std::vec![0u8; metrics14::METRICS_BYTES];
    metrics14::synthetic(&t, &mut b);
    let r = metrics14::parse(&b).unwrap();
    assert_eq!((r.edge, r.hotspot, r.mem, r.counter, r.hotspot_milli()), (41, 55, 48, 7, 55_000));
    assert_eq!((b[140], b[142], b[148]), (41, 55, 48));
    assert!(!r.all_zero());
    assert!(metrics14::parse(&b[..metrics14::METRICS_BYTES - 1]).is_err());
    assert!(metrics14::parse(&std::vec![0u8; metrics14::METRICS_BYTES]).unwrap().all_zero());
}
