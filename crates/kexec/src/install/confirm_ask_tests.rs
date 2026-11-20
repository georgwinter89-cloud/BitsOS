//! VB-1 host tests: when the question "Version N läuft. Bestätigen?" stands and which answer counts.

use super::confirm::Status;
use super::confirm_ask::{REAL_WITHIN_NS, answer_lost, answered, judge, real_recent, set_answered, stands, state_word, taken};
use alloc::string::String;

fn pending(generation: u32) -> Status {
    Status::Pending { generation, covered_s: 0, need_s: 120, counter: "mcp.answered" }
}

#[test]
fn the_question_stands_for_an_unconfirmed_numbered_version_in_its_own_slot() {
    assert_eq!(stands(&pending(168), 168, true, 0), Some(168));
    assert_eq!(state_word(&pending(168)), "pending");
}

#[test]
fn a_proof_that_confirmed_meanwhile_ends_the_question() {
    for s in [Status::Writing { generation: 168 }, Status::Confirmed { generation: 168 }, Status::Replaced { generation: 168 }, Status::Failed { generation: 168, why: String::from("x") }] {
        assert_eq!(stands(&s, 168, true, 0), None, "{s:?}");
    }
    assert_eq!(state_word(&Status::Confirmed { generation: 168 }), "confirmed");
    assert_eq!(stands(&pending(168), 168, true, 168), None, "Georg's own Ja given: no second question");
}

#[test]
fn a_development_build_never_asks() {
    assert_eq!(stands(&pending(169), 0, true, 0), None, "version 0: MCP stays its only proof");
    assert_eq!(stands(&pending(169), 168, true, 0), None, "a version outside its own slot");
    assert_eq!(stands(&Status::NotNeeded(String::from("reload")), 168, true, 0), None);
    assert_eq!(state_word(&Status::NotNeeded(String::new())), "none");
}

#[test]
fn a_medium_that_cannot_take_ok_never_asks() {
    assert_eq!(stands(&pending(168), 168, false, 0), None);
}

#[test]
fn only_an_answer_behind_real_input_counts() {
    let s = pending(168);
    assert_eq!(judge(&s, 168, true, 0, 168, true), Ok(()));
    assert!(judge(&s, 168, true, 0, 168, false).unwrap_err().contains("real key"), "MCP's injected input never reaches the counter");
    assert!(judge(&s, 168, true, 0, 167, true).unwrap_err().contains("not 167"));
    assert!(judge(&Status::Confirmed { generation: 168 }, 168, true, 0, 168, true).unwrap_err().contains("no question"));
    assert!(judge(&s, 0, true, 0, 168, true).is_err(), "a development build");
    assert!(judge(&s, 168, true, 168, 168, true).unwrap_err().contains("answered already"));
}

#[test]
fn only_a_real_press_after_the_watch_began_and_within_three_seconds_counts() {
    let s = 1_000_000_000u64;
    let watch = 30 * s;
    assert!(real_recent(100 * s, watch, 100 * s), "a press just now");
    assert!(real_recent(100 * s, watch, 100 * s + REAL_WITHIN_NS));
    assert!(!real_recent(100 * s, watch, 100 * s + REAL_WITHIN_NS + 1), "a press more than 3 s ago is no answer");
    assert_eq!(REAL_WITHIN_NS, 3 * s);
    assert!(!real_recent(0, watch, 40 * s), "no real press this boot");
    assert!(!real_recent(20 * s, watch, 31 * s), "a press before the watch began (before the question) never counts");
    assert!(!real_recent(100 * s, 0, 100 * s), "the watch has not begun: no question, no answer");
    assert!(!real_recent(101 * s, watch, 100 * s), "a stamp from the future (a clock step) is no answer");
}

#[test]
fn the_accepted_gap_a_process_right_after_a_real_press() {
    // Stated on purpose (VB-1 report): a root process that MCP did not start, directly or through bitspkgd's
    // update_confirm, answering within 3 s after any real press a physical-only reader read, counts. The node
    // and bitspkgd are closed to MCP-started processes.
    let s = 1_000_000_000u64;
    let st = pending(168);
    let real = real_recent(50 * s, 10 * s, 52 * s);
    assert!(real);
    assert_eq!(judge(&st, 168, true, 0, 168, real), Ok(()));
    assert!(judge(&st, 168, true, 0, 168, real_recent(50 * s, 10 * s, 54 * s)).is_err(), "4 s later the window is shut");
}

#[test]
fn a_ja_whose_write_failed_asks_again() {
    let st = pending(168);
    set_answered(168);
    assert!(taken(168));
    assert_eq!(stands(&st, 168, true, answered()), None, "answered: the question waits for the write");
    answer_lost(167);
    assert_eq!(answered(), 168, "another generation's loss voids nothing");
    answer_lost(168);
    assert_eq!(stands(&st, 168, true, answered()), Some(168), "the write failed: the question stands again");
    assert!(!taken(168));
}
