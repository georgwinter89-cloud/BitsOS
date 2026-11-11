//! GX-8c: the order of the boot's bring-up on a chip that only says what it is told (`gx.hw.boot_*` run it on the fake chain).

use crate::hw::boot::{self, Chip, Fail, Outcome, StopEnd};
use crate::hw::live::ToolGate;
use crate::gpucore::psp::Until;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

struct Script {
    gate: ToolGate,
    clock: Cell<u64>,
    stick_after: Cell<u32>,
    back: Result<u32, Fail>,
    stop_answer: Result<bool, ()>,
    order: RefCell<Vec<&'static str>>,
}

fn chip(back: Result<u32, Fail>) -> Script {
    Script { gate: ToolGate::new(), clock: Cell::new(0), stick_after: Cell::new(0), back, stop_answer: Ok(true), order: RefCell::new(Vec::new()) }
}

impl Chip for Script {
    fn gate(&self) -> &ToolGate {
        &self.gate
    }
    fn now_ns(&self) -> u64 {
        self.clock.get()
    }
    fn sleep_ns(&self, ns: u64) {
        self.clock.set(self.clock.get() + ns);
    }
    fn stick_ready(&self) -> bool {
        let n = self.stick_after.get();
        self.stick_after.set(n.saturating_sub(1));
        n == 0
    }
    fn backend(&self, node_by: u64, lines: &mut Vec<String>) -> Result<u32, Fail> {
        assert_eq!(node_by, 1000 * boot::STICK_POLL_NS);
        assert!(self.gate.enter().is_none(), "the gate is held over the backend");
        self.order.borrow_mut().push("backend");
        lines.push("the stage said FAIL: 7".into());
        self.back
    }
    fn stop(&self, lines: &mut Vec<String>) -> Result<bool, ()> {
        assert!(self.gate.enter().is_none(), "the gate is held over the stop");
        self.order.borrow_mut().push("stop");
        lines.push("stopped".into());
        self.stop_answer
    }
}

const DEADLINE: u64 = 1000 * boot::STICK_POLL_NS;

#[test]
fn up_needs_no_stop() {
    let c = chip(Ok(128));
    let r = boot::run(&c, DEADLINE);
    assert_eq!(r.outcome, Outcome::Up(128));
    assert_eq!(*c.order.borrow(), ["backend"]);
    assert!(c.gate.enter().is_some(), "the gate stays free after the end");
    assert!(boot::render(&r).starts_with("gx_bringup: PASS\nboot: up on renderD128 after 0 ms\n"));
}

/// A refusal wrote nothing: no stop, whose bus-master line is a write, and whose chain would be the call's that kept it.
#[test]
fn a_refusal_stops_nothing() {
    let c = chip(Err(Fail::Refused));
    let r = boot::run(&c, DEADLINE);
    assert_eq!(r.outcome, Outcome::Failed { fail: Fail::Refused, why: "the stage said FAIL: 7".into(), stop: StopEnd::NotNeeded });
    assert_eq!(*c.order.borrow(), ["backend"]);
    assert!(c.gate.enter().is_some(), "the gate stayed shut");
}

/// Whatever the chain did, the stop comes; the reason is what the chain said, not what the stop said.
#[test]
fn every_failure_with_a_chain_stops() {
    for f in [Fail::Stage(Until::Sdma), Fail::Stage(Until::GfxMes), Fail::Device, Fail::Proof, Fail::Picture, Fail::Late, Fail::Node] {
        let c = chip(Err(f));
        let r = boot::run(&c, DEADLINE);
        assert_eq!(r.outcome, Outcome::Failed { fail: f, why: "the stage said FAIL: 7".into(), stop: StopEnd::Stopped }, "{f:?}");
        assert_eq!(*c.order.borrow(), ["backend", "stop"], "{f:?}");
        assert!(c.gate.enter().is_some(), "{f:?}: the gate stayed shut");
        assert!(boot::render(&r).starts_with(&format!("gx_bringup: FAIL\nboot: failed at {}, stop done: the stage said FAIL: 7 after 0 ms\n", f.word())), "{f:?}: {}", boot::render(&r));
    }
}

/// A stop that fails or is refused is no stop, and neither stops the boot.
#[test]
fn a_stop_that_did_not_prove_quiet_says_so() {
    for answer in [Ok(false), Err(())] {
        let mut c = chip(Err(Fail::Proof));
        c.stop_answer = answer;
        let r = boot::run(&c, DEADLINE);
        assert!(matches!(r.outcome, Outcome::Failed { stop: StopEnd::Left, .. }), "{answer:?}: {:?}", r.outcome);
    }
}

#[test]
fn the_stick_comes_or_the_chip_stays_untouched() {
    let c = chip(Ok(128));
    c.stick_after.set(5);
    let r = boot::run(&c, DEADLINE);
    assert_eq!(r.outcome, Outcome::Up(128));
    assert_eq!(r.took_ns, 5 * boot::STICK_POLL_NS);
    let c = chip(Ok(128));
    c.stick_after.set(u32::MAX);
    let r = boot::run(&c, DEADLINE);
    assert!(matches!(&r.outcome, Outcome::Failed { fail: Fail::NoStick, stop: StopEnd::NotNeeded, why } if why.contains("did not come")), "{:?}", r.outcome);
    assert!(c.order.borrow().is_empty());
}

#[test]
fn a_call_over_mcp_keeps_the_chip() {
    let c = chip(Ok(128));
    let held = c.gate.enter().unwrap();
    let r = boot::run(&c, DEADLINE);
    assert!(matches!(r.outcome, Outcome::Failed { fail: Fail::Busy, stop: StopEnd::NotNeeded, .. }), "{:?}", r.outcome);
    assert!(c.order.borrow().is_empty());
    drop(held);
}

#[test]
fn the_bound_is_inclusive_and_the_words_stay() {
    assert!(boot::in_time(10, 10) && boot::in_time(0, 10) && !boot::in_time(11, 10));
    assert_eq!(boot::BOUND_NS, 30_000_000_000);
    assert_eq!(Fail::Stage(Until::GfxMes).word(), "gfx_mes");
    assert!(!Fail::NoStick.stops() && !Fail::Busy.stops() && !Fail::Refused.stops());
}

#[test]
fn the_reason_is_short_and_found() {
    assert_eq!(boot::why_of(&[]), "");
    assert_eq!(boot::why_of(&["a".into(), "b".into()]), "b");
    assert_eq!(boot::why_of(&["a".into(), "x FAIL y".into(), "b".into()]), "x FAIL y");
    let long = "\u{e4}".repeat(1000);
    assert_eq!(boot::why_of(&[long]).chars().count(), 200, "cut on a character");
}
