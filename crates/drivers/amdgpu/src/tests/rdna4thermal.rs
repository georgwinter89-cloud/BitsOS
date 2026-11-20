//! Wave 44 (TH): the thermal taker at the IH reader on the model. The model's thermal block raises each alert as
//! the card would, once the stage `pmfw` armed it, and refuses what Linux's handler would not do; the taker does
//! what Linux's `smu_v14_0_irq_process` and the software CTF check do, in Linux's order, through its gate.

use super::rdna4fix::*;
use crate::gpucore::psp::Hw;
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakethermal::Alert;
use crate::rdna4::ihring::IhIo;
use crate::rdna4::stage::{Session, Stage};
use crate::rdna4::thermal::{self, PowerOff, Thermal};
use rdna4_core::hub41;
use rdna4_core::ih7::{Iv, Ring};
use rdna4_core::thm14;
use std::cell::RefCell;
use std::format;
use std::string::String;
use std::vec::Vec;

/// The chain up to and with `pmfw` on `m`: the alert armed, the taker knows the card.
pub(super) fn armed_on(m: Model) -> (Session, Model, u64) {
    let rig = Rig::new();
    let mut s = Session::default();
    let upto = Stage::CHAIN.iter().position(|x| *x == Stage::Pmfw).unwrap() + 1;
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &Stage::CHAIN[..upto]);
    let sys = s.nb.ih.as_ref().unwrap().place.sys;
    (s, m, sys)
}

pub(super) fn armed() -> (Session, Model, u64) {
    armed_on(model())
}

pub(super) fn th(s: &Session) -> &Thermal {
    &s.nb.ih.as_ref().unwrap().reader.thermal
}

pub(super) fn pass(s: &mut Session, m: &mut Model, sys: u64) {
    s.nb.ih.as_mut().unwrap().reader.process(&mut ModelIo { m, sys });
}

/// The rings as a stage's own reader reaches them: no thermal register (`sdmaih::StageIh`).
pub(super) struct NoReach<'a>(pub(super) ModelIo<'a>);

impl IhIo for NoReach<'_> {
    fn mem32(&mut self, off: u64) -> Option<u32> {
        self.0.mem32(off)
    }
    fn set_mem32(&mut self, off: u64, v: u32) -> bool {
        self.0.set_mem32(off, v)
    }
    fn wptr_reg(&mut self, ring: Ring) -> Option<u32> {
        self.0.wptr_reg(ring)
    }
    fn cntl(&mut self, ring: Ring) -> Option<u32> {
        self.0.cntl(ring)
    }
    fn set_cntl(&mut self, ring: Ring, v: u32) -> bool {
        self.0.set_cntl(ring, v)
    }
    fn doorbell(&mut self, ring: Ring, rptr: u32) -> bool {
        self.0.doorbell(ring, rptr)
    }
    fn fault_status(&mut self, hub: hub41::Kind) -> Option<u32> {
        self.0.fault_status(hub)
    }
    fn fault_clear(&mut self, hub: hub41::Kind) -> bool {
        self.0.fault_clear(hub)
    }
    fn now_us(&mut self) -> u64 {
        self.0.now_us()
    }
}

pub(super) fn metrics_sent(m: &Model) -> usize {
    m.messages.iter().filter(|x| **x == (0x12, 5)).count()
}

#[test]
fn the_alert_is_armed_by_pmfw_and_raises_nothing_before() {
    let rig = Rig::new();
    let mut s = Session::default();
    let upto = Stage::CHAIN.iter().position(|x| *x == Stage::Pmfw).unwrap();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), &Stage::CHAIN[..upto]);
    for a in [Alert::High, Alert::Low, Alert::Smu(8)] {
        assert!(m.raise_thermal(a).unwrap_err().contains("the alert is not armed"), "{a:?}");
    }
    assert_eq!((th(&s).range, th(&s).sensors.is_some(), th(&s).alert_armed), (None, false, false));
    // A table transfer of the metrics before the start ended: the model refuses it.
    assert!(!super::rdna4pmfw::raw(&mut m, 0x12, 5));
    assert!(super::rdna4pmfw::last(&m).contains("before Linux's start of the power controller ended"), "{:?}", m.refused);
    let (s, mut m, _) = armed();
    let t = th(&s);
    assert!(t.alert_armed && t.sensors.is_some());
    assert_eq!(t.range.unwrap().software_shutdown_temp, 118);
    for a in [Alert::High, Alert::Low, Alert::Smu(3)] {
        m.raise_thermal(a).unwrap();
    }
}

#[test]
fn a_rising_alert_schedules_one_check_50_ms_ahead_and_touches_no_register() {
    let (mut s, mut m, sys) = armed();
    let (writes, t0) = (m.writes.len(), m.card.now_us);
    m.raise_thermal(Alert::High).unwrap();
    pass(&mut s, &mut m, sys);
    assert_eq!((th(&s).count, th(&s).due_us), ([1, 0, 0], Some(t0 + 50_000)));
    // A second alert while the check is pending leaves it where it is (`schedule_delayed_work`).
    m.delay_us(10_000);
    m.raise_thermal(Alert::High).unwrap();
    pass(&mut s, &mut m, sys);
    assert_eq!((th(&s).count[0], th(&s).due_us), (2, Some(t0 + 50_000)));
    // Not before its delay: no message, no register of the handler.
    m.delay_us(39_000);
    pass(&mut s, &mut m, sys);
    assert_eq!((metrics_sent(&m), th(&s).checks), (0, 0));
    assert!(m.writes[writes..].iter().all(|(b, _)| *b != m.regs.smu_message && *b != m.regs.thm_int_ctrl && *b != m.regs.mp1_ih_sw_int_ctrl));
    m.delay_us(1_000);
    pass(&mut s, &mut m, sys);
    let t = th(&s);
    assert_eq!((metrics_sent(&m), t.checks, t.due_us, t.power_off.clone()), (1, 1, None, None));
    assert!(t.outcome.as_deref().unwrap().starts_with("hotspot 38 C below the software shutdown temperature 118 C"), "{:?}", t.outcome);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    // The sensors' read is exactly Linux's: the answer register, 0, argument 5, message 0x12; no other message.
    let r = m.regs.clone();
    let mine: Vec<(u32, u32)> = m.writes[writes..].iter().copied().filter(|(b, _)| [r.smu_answer, r.smu_argument, r.smu_message].contains(b)).collect();
    assert_eq!(mine, [(r.smu_answer, 0), (r.smu_argument, 5), (r.smu_message, 0x12)]);
}

/// The check after a rising alert on a model the test set up; the power-off it decided.
fn check_on(prepare: impl FnOnce(&mut Model)) -> (Thermal, Model) {
    let (mut s, mut m, sys) = armed();
    prepare(&mut m);
    m.raise_thermal(Alert::High).unwrap();
    pass(&mut s, &mut m, sys);
    m.delay_us(50_000);
    pass(&mut s, &mut m, sys);
    (th(&s).clone(), m)
}

#[test]
fn the_check_powers_off_in_every_case_linux_does_and_only_then() {
    let hot = |h: u16| check_on(|m| m.pmfw.thm.temps.hotspot = h).0;
    assert_eq!(hot(117).power_off, None);
    assert!(hot(118).power_off.unwrap().contains("hotspot 118 C at or above the software shutdown temperature 118 C"));
    assert!(hot(130).power_off.unwrap().contains("hotspot 130 C"));
    // A table of zeros reads 0 degrees: Linux takes that as cool, and so does the check.
    let (t, _) = check_on(|m| m.pmfw.thm.temps = Default::default());
    assert_eq!(t.power_off, None);
    // The read fails: an answer that is not OK, no answer within Linux's bound, a table that does not read.
    let (t, _) = check_on(|m| m.pmfw.answer_with = Some((0x12, 0xFF)));
    assert!(t.power_off.unwrap().contains("could not be read") && t.read_failed.unwrap().contains("answered 0xff: Linux's -EIO"));
    let (t, _) = check_on(|m| m.pmfw.table_unreadable = true);
    assert!(t.power_off.unwrap().contains("the driver table does not read"));
    let (t, _) = check_on(|m| m.smu_silent_on = Some(0x12));
    assert!(t.power_off.unwrap().contains("no answer within Linux's 2000000 reads"));
}

#[test]
fn a_table_without_a_limit_powers_off_without_a_read() {
    let mut m = model();
    m.pmfw.table.software_shutdown_temp = 0;
    let (mut s, mut m, sys) = armed_on(m);
    m.raise_thermal(Alert::High).unwrap();
    pass(&mut s, &mut m, sys);
    m.delay_us(50_000);
    pass(&mut s, &mut m, sys);
    assert!(th(&s).power_off.as_deref().unwrap_or("").contains("could not be read after 50 ms (the table names no software shutdown temperature)"), "{:?}", th(&s));
    assert_eq!(metrics_sent(&m), 0, "Linux reads no sensor then");
}

#[test]
fn the_cards_own_alert_reaches_the_taker_when_the_hotspot_crosses_the_threshold() {
    let (mut s, mut m, sys) = armed();
    m.set_temps(60, 110, 70).unwrap();
    pass(&mut s, &mut m, sys);
    assert_eq!(th(&s).count, [0, 0, 0], "below the threshold no alert");
    m.set_temps(70, 121, 80).unwrap();
    m.set_temps(70, 122, 80).unwrap();
    pass(&mut s, &mut m, sys);
    assert_eq!(th(&s).count, [1, 0, 0], "one alert per crossing");
    m.delay_us(50_000);
    pass(&mut s, &mut m, sys);
    assert!(th(&s).power_off.as_deref().unwrap().contains("hotspot 122 C"));
    assert_eq!(th(&s).last.unwrap().0.edge, 70);
}

#[test]
fn a_pmfw_reload_cancels_a_pending_check_and_a_waiting_acknowledge() {
    let (mut s, mut m, sys) = armed();
    m.raise_thermal(Alert::High).unwrap();
    pass(&mut s, &mut m, sys);
    // An acknowledge that waits for a reader reaching the card belongs to the old PMFW too.
    let at = m.writes.len();
    m.raise_thermal(Alert::Smu(3)).unwrap();
    s.nb.ih.as_mut().unwrap().reader.process(&mut NoReach(ModelIo { m: &mut m, sys }));
    assert_eq!(th(&s).queue.len(), 1);
    s.nb.ih.as_mut().unwrap().reader.thermal.reloaded();
    let t = th(&s);
    assert_eq!((t.queue.len(), t.cancelled, t.regs.is_some(), t.alert_armed), (0, 2, false, false));
    m.delay_us(60_000);
    pass(&mut s, &mut m, sys);
    let t = th(&s);
    assert_eq!((t.checks, t.cancelled, t.due_us, t.sensors.is_some(), metrics_sent(&m), t.failed), (0, 2, None, false, 0, 0));
    assert!(mp1_writes(&m, at).is_empty(), "nothing of the old PMFW's acknowledge reaches the card");
}

pub(super) fn mp1_writes(m: &Model, from: usize) -> Vec<(u32, u32)> {
    let r = &m.regs;
    m.writes[from..].iter().copied().filter(|(b, _)| *b == r.mp1_ih_sw_int_ctrl || *b == r.thm_int_ctrl).collect()
}

#[test]
fn a_fan_fault_is_acknowledged_first_and_moves_the_threshold_by_the_tables_offset() {
    let mut m = model();
    m.pmfw.table.fan_abnormal_temp_limit_offset = -10;
    let (mut s, mut m, sys) = armed_on(m);
    let (r, at) = (m.regs.clone(), m.writes.len());
    m.raise_thermal(Alert::Smu(thm14::CTX_FAN_ABNORMAL)).unwrap();
    pass(&mut s, &mut m, sys);
    assert_eq!(mp1_writes(&m, at), [(r.mp1_ih_sw_int_ctrl, 0x100), (r.thm_int_ctrl, 0xb8ff_006c)], "{:?}", m.refused);
    assert_eq!((th(&s).high_now, m.pmfw.thm.acks, m.pmfw.thm.rearms), (Some(108), 1, 1));
    let at = m.writes.len();
    m.raise_thermal(Alert::Smu(thm14::CTX_FAN_RECOVERY)).unwrap();
    pass(&mut s, &mut m, sys);
    // The acknowledge keeps what it read: the model holds the bit written before (section 7, `thm-ack`).
    assert_eq!(mp1_writes(&m, at), [(r.mp1_ih_sw_int_ctrl, 0x100), (r.thm_int_ctrl, 0xb8ff_0076)]);
    assert_eq!((th(&s).high_now, th(&s).count[2]), (Some(118), 2));
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

#[test]
fn every_other_context_is_only_acknowledged() {
    let (mut s, mut m, sys) = armed();
    let r = m.regs.clone();
    for ctx in [2, 3, 4, 5, 6, 7, 10, 0x55] {
        let at = m.writes.len();
        m.raise_thermal(Alert::Smu(ctx)).unwrap();
        pass(&mut s, &mut m, sys);
        assert_eq!(mp1_writes(&m, at), [(r.mp1_ih_sw_int_ctrl, 0x100)], "context {ctx}");
    }
    assert_eq!((th(&s).count[2], th(&s).contexts.len(), th(&s).high_now), (8, 8, None));
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

#[test]
fn a_falling_alert_is_counted_and_said_and_nothing_else() {
    let (mut s, mut m, sys) = armed();
    let at = m.writes.len();
    m.raise_thermal(Alert::Low).unwrap();
    pass(&mut s, &mut m, sys);
    assert_eq!((th(&s).count, th(&s).due_us), ([0, 1, 0], None));
    assert!(mp1_writes(&m, at).is_empty());
}

#[test]
fn a_thermal_source_linux_does_not_take_is_loud_and_not_taken() {
    let (mut s, mut m, sys) = armed();
    let at = m.writes.len();
    for (c, src) in [(0x0f, 2), (0x0f, 5), (0x1f, 3), (0x1f, 0)] {
        m.raise(Ring::Ring0, &Iv { client_id: c, src_id: src, ..Iv::default() }).unwrap();
    }
    pass(&mut s, &mut m, sys);
    let reader = &s.nb.ih.as_ref().unwrap().reader;
    assert_eq!(reader.thermal.foreign.values().sum::<u64>(), 4);
    assert_eq!(reader.thermal.count, [0, 0, 0]);
    assert_eq!(reader.unclaimed.get(&(0x1f, 3)), Some(&1));
    assert!(mp1_writes(&m, at).is_empty());
    assert!(reader.describe().contains("client 31 source 3: 1") && reader.thermal.describe().contains("other thermal sources client 15 source 2: 1"));
}

#[test]
fn the_model_refuses_what_linuxs_handler_would_not_do() {
    let (_, mut m, _) = armed();
    let r = m.regs.clone();
    // An acknowledge with no interrupt pending, the threshold without a fan case, the two registers Linux's
    // handler never writes, a start message after the start, and the never list.
    assert!(!m.reg_write(r.mp1_ih_sw_int_ctrl, 0x100));
    assert!(super::rdna4pmfw::last(&m).contains("an acknowledge with no power controller interrupt pending"));
    assert!(!m.reg_write(r.thm_int_ctrl, 0xb8ff_0070));
    assert!(super::rdna4pmfw::last(&m).contains("not right after the acknowledge of a fan fault"));
    for b in [r.thm_int_ena, r.mp1_ih_sw_int] {
        assert!(!m.reg_write(b, 0x38));
        assert!(super::rdna4pmfw::last(&m).contains("Linux's handler never writes it"));
    }
    m.raise_thermal(Alert::Smu(8)).unwrap();
    assert!(!m.reg_write(r.mp1_ih_sw_int_ctrl, 0x101));
    assert!(super::rdna4pmfw::last(&m).contains("Linux's acknowledge of what it holds is 0x100"));
    assert!(m.reg_write(r.mp1_ih_sw_int_ctrl, 0x100));
    assert!(!m.reg_write(r.thm_int_ctrl, 0xb8ff_0070));
    assert!(super::rdna4pmfw::last(&m).contains("Linux's limit 118 in what it holds is 0xb8ff0076"));
    for (msg, arg) in [(0x36, 0), (0x06, 0), (0x12, 1), (0x12, 8)] {
        assert!(!super::rdna4pmfw::raw(&mut m, msg, arg), "{msg:#x} {arg:#x}");
    }
    assert!(!super::rdna4pmfw::raw(&mut m, 0x32, 0x14b));
    assert!(super::rdna4pmfw::last(&m).contains("never list"), "{:?}", m.refused);
}

#[test]
fn the_taker_never_sends_another_message_whatever_it_is_asked() {
    use crate::rdna4::thermalhw::Gated;
    use rdna4_core::thmgate::Action;
    let (s, mut m, _) = armed();
    let sensors = th(&s).sensors.unwrap();
    let r = th(&s).regs.clone().unwrap();
    let at = m.writes.len();
    let _ = m.reg_read(r.smu_answer);
    let mut g = Gated::new(&mut m, &r, Action::Metrics, Some(sensors.driver_at));
    let _ = g.reg_read(r.smu_answer);
    assert!(g.reg_write(r.smu_answer, 0) && g.reg_write(r.smu_argument, 5));
    assert!(!g.reg_write(r.smu_message, 0x32));
    assert_eq!(g.refused, Some("an SMU message on the never list"));
    assert!(!g.reg_write(r.rsmu_index, 0) && !g.reg_write(r.thm_int_ena, 0x38));
    assert_eq!(g.reg_read(r.thm_int_ctrl), None, "no read outside the action's own");
    let mut b = [0u8; 412];
    assert!(!g.mem_read(sensors.driver_at, &mut b), "no table before the answer");
    drop(g);
    assert_eq!(m.writes[at..], [(r.smu_answer, 0), (r.smu_argument, 5)]);
}

#[test]
fn without_reach_the_register_part_waits_in_order_and_runs_at_the_next_pass() {
    let (mut s, mut m, sys) = armed();
    let r = m.regs.clone();
    let at = m.writes.len();
    m.raise_thermal(Alert::Smu(thm14::CTX_FAN_ABNORMAL)).unwrap();
    m.raise_thermal(Alert::Smu(3)).unwrap();
    m.raise_thermal(Alert::High).unwrap();
    s.nb.ih.as_mut().unwrap().reader.process(&mut NoReach(ModelIo { m: &mut m, sys }));
    assert_eq!((th(&s).queue.len(), th(&s).count), (2, [1, 0, 2]));
    assert!(mp1_writes(&m, at).is_empty());
    // The check comes due while only a stage's reader runs: it waits too.
    m.delay_us(60_000);
    s.nb.ih.as_mut().unwrap().reader.process(&mut NoReach(ModelIo { m: &mut m, sys }));
    assert_eq!((th(&s).checks, metrics_sent(&m)), (0, 0));
    pass(&mut s, &mut m, sys);
    assert_eq!(mp1_writes(&m, at), [(r.mp1_ih_sw_int_ctrl, 0x100), (r.thm_int_ctrl, 0xb8ff_0076), (r.mp1_ih_sw_int_ctrl, 0x100)]);
    assert_eq!((th(&s).queue.len(), th(&s).checks, th(&s).late, metrics_sent(&m)), (0, 1, 1, 1));
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

#[test]
fn a_full_queue_counts_what_it_loses() {
    let (mut s, mut m, sys) = armed();
    for _ in 0..65 {
        m.raise_thermal(Alert::Smu(3)).unwrap();
    }
    s.nb.ih.as_mut().unwrap().reader.process(&mut NoReach(ModelIo { m: &mut m, sys }));
    assert_eq!((th(&s).queue.len(), th(&s).lost, th(&s).count[2]), (64, 1, 65));
    assert!(th(&s).describe().contains("waiting acknowledges 64 (1 lost)"));
}

#[test]
fn the_rungs_reader_keeps_the_register_part_out_of_its_stage() {
    let rig = Rig::new();
    let (mut s, mut m, sys) = armed();
    let upto = Stage::CHAIN.iter().position(|x| *x == Stage::Pmfw).unwrap() + 1;
    let (_, back) = rig.pass(&mut s, m, &mut Stick::full(), &Stage::CHAIN[upto..]);
    m = back;
    m.raise_thermal(Alert::Smu(3)).unwrap();
    let at = m.writes.len();
    let (d, mut m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(d.pass && d.refused.is_empty(), "{} {:?}", d.line, d.lines);
    // The stage's line holds its two flushes and nothing of the handler's; the entry waits for the thread.
    assert!(d.lines.iter().any(|l| l.starts_with("linuxseq mes: equal")), "{:?}", d.lines);
    assert!(mp1_writes(&m, at).is_empty());
    assert_eq!((th(&s).count[2], th(&s).queue.len()), (1, 1));
    pass(&mut s, &mut m, sys);
    assert_eq!(mp1_writes(&m, at), [(m.regs.mp1_ih_sw_int_ctrl, 0x100)]);
}

struct Recorder<'a> {
    calls: &'a RefCell<Vec<String>>,
    answer: &'static str,
}

impl PowerOff for Recorder<'_> {
    fn power_off(&mut self, why: &str) -> String {
        self.calls.borrow_mut().push(format!("off: {why}"));
        String::from(self.answer)
    }
}

#[test]
fn the_power_off_comes_after_the_take_and_its_failure_is_said() {
    let calls = RefCell::new(Vec::new());
    let mut off = Recorder { calls: &calls, answer: "S5 refused" };
    let mut t = Thermal { power_off: Some(String::from("hotspot 120 C")), ..Thermal::default() };
    let mut watch = thermal::OffWatch::default();
    let failed = watch.after(
        || {
            calls.borrow_mut().push(String::from("take"));
            t.take_power_off()
        },
        &mut off,
    );
    assert_eq!(*calls.borrow(), ["take", "off: hotspot 120 C"]);
    assert!(failed.unwrap().contains("power-off did not happen (S5 refused; tried again 0 times, every 10 s): hold the power button"));
    // It did not happen: the next turn tries again with the same reason and asks the taker nothing (the
    // quiesce switched the IH off, no alert comes any more), and says how often it tried.
    let again = watch.after(|| panic!("the taker is not asked again"), &mut off);
    assert_eq!(*calls.borrow(), ["take", "off: hotspot 120 C", "off: hotspot 120 C"]);
    assert!(again.unwrap().contains("tried again 1 times") && watch.tries == 1);
    // Nothing decided: nothing is called.
    let mut idle = thermal::OffWatch::default();
    assert_eq!(idle.after(|| t.take_power_off(), &mut off), None);
    assert_eq!((calls.borrow().len(), idle.failed), (3, None));
}

#[test]
fn the_thread_waits_until_the_check_is_due() {
    assert_eq!(thermal::wait_ns(1_000_000_000, None), 1_000_000_000);
    assert_eq!(thermal::wait_ns(1_000_000_000, Some(50_000)), 50_000_000);
    assert_eq!(thermal::wait_ns(1_000_000_000, Some(0)), 0);
    assert_eq!(thermal::wait_ns(1_000_000_000, Some(5_000_000)), 1_000_000_000);
}

#[test]
fn the_look_shows_limits_and_temperatures_from_one_read() {
    let (mut s, mut m, _) = armed();
    m.pmfw.thm.temps.edge = 45;
    let now = m.card.now_us;
    let look = s.nb.ih.as_mut().unwrap().reader.thermal.look(&mut m, now);
    assert_eq!(look.temps.unwrap().edge, 45);
    assert_eq!(look.limits, Some((100, 110, 108, 118)));
    assert!(look.lines[0].starts_with("thermal limits (the card's table): edge 100 C, hotspot 110 C, memory 108 C"));
    assert_eq!(metrics_sent(&m), 1);
    assert!(s.summary().contains("thermal alert armed, upper threshold 118 C"));
}
