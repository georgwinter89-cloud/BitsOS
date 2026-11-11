//! DS-4 host tests of the desktop start's decisions (`desktop.rs`): waiting for init, one start's outcome and
//! what follows it (the 3-failure fallback without QEMU).

use crate::desktop::{ATTEMPTS, Hold, InitWait, Next, Outcome, TAKE_SCREEN_NS, WAIT_INIT_NS, hold_wait, init_wait, next, poll};
use console::bootsteps::Init;

/// Runs the start loop's decisions over one outcome per attempt; returns what ended it and the attempts used.
fn run(outcomes: &[Outcome]) -> (Next, u32) {
    for (i, o) in outcomes.iter().enumerate() {
        let attempt = i as u32 + 1;
        match next(attempt, *o) {
            Next::Retry(ns) => assert_eq!(ns, attempt as u64 * 1_000_000_000),
            n => return (n, attempt),
        }
    }
    panic!("the loop wanted more starts than given");
}

#[test]
fn three_immediate_exits_give_up_with_the_last_reason() {
    let o = [Outcome::Failed("ended"), Outcome::Failed("exit_code"), Outcome::Failed("crashed"), Outcome::Up(9)];
    assert_eq!(run(&o), (Next::GiveUp("crashed"), ATTEMPTS));
}

#[test]
fn a_spawn_error_counts_as_a_failed_start() {
    // /nonexistent: proc::spawn_with gives NotFound before any process exists.
    let o = [Outcome::Failed("not_found"); 3];
    assert_eq!(run(&o), (Next::GiveUp("not_found"), 3));
    // A second start that works ends the loop as up.
    assert_eq!(run(&[Outcome::Failed("not_found"), Outcome::Up(42)]), (Next::Up(42), 2));
}

#[test]
fn a_program_that_ended_after_taking_the_screen_is_not_restarted() {
    // Ended within the same poll as it took the screen (`took`, owner already reset by reclaim).
    assert_eq!(poll(7, Some("crashed"), false, true, 50_000_000), Some(Outcome::TookThenEnded("crashed")));
    // Ended while still the owner.
    assert_eq!(poll(7, Some("ended"), true, true, 50_000_000), Some(Outcome::TookThenEnded("ended")));
    assert_eq!(run(&[Outcome::TookThenEnded("crashed")]), (Next::Ended("crashed"), 1));
}

#[test]
fn up_needs_the_program_itself_to_own_the_screen() {
    // Another program has KD_GRAPHICS (has_screen false for this pid): keep waiting, not "up".
    assert_eq!(poll(7, None, false, false, 1_000_000), None);
    assert_eq!(poll(7, None, true, true, 1_000_000), Some(Outcome::Up(7)));
    // Ended before it ever took the screen: a failed start.
    assert_eq!(poll(7, Some("ended"), false, false, 1_000_000), Some(Outcome::Failed("ended")));
    // Runs without the screen for too long: slow (the text console, the program keeps running).
    assert_eq!(poll(7, None, false, false, TAKE_SCREEN_NS + 1), Some(Outcome::Slow(7)));
    assert_eq!(run(&[Outcome::Slow(7)]), (Next::Slow(7), 1));
}

#[test]
fn init_off_given_up_or_late_means_no_desktop() {
    assert_eq!(init_wait(Init::Off, false, 0), InitWait::NoInit);
    assert_eq!(init_wait(Init::GivenUp, true, 0), InitWait::NoInit);
    assert_eq!(init_wait(Init::Running, true, 0), InitWait::Ready);
    // Running but /run/user/1000 not made yet: wait, then time out.
    assert_eq!(init_wait(Init::Running, false, 1), InitWait::Wait);
    assert_eq!(init_wait(Init::Running, false, WAIT_INIT_NS + 1), InitWait::Timeout);
    assert_eq!(init_wait(Init::Unknown, false, WAIT_INIT_NS + 1), InitWait::Timeout);
}

/// GX-8c: a driver's hold keeps the start back to its bound, no longer.
#[test]
fn hold() {
    assert_eq!(hold_wait(None, 0), Hold::Free);
    assert_eq!(hold_wait(None, u64::MAX), Hold::Free);
    assert_eq!(hold_wait(Some(100), 0), Hold::Wait);
    assert_eq!(hold_wait(Some(100), 100), Hold::Wait);
    assert_eq!(hold_wait(Some(100), 101), Hold::Over);
}
