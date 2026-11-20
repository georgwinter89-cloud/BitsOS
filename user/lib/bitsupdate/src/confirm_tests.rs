//! VB-1: what the screens make of the kernel's confirmation status.

use crate::confirm::{Ask, Done, Question, decide, outcome};
use crate::flow::{Installed, parse_status};
use mcp_wire::json::Json;

fn node(version: u32, generation: u32, state: &str, asks: bool, pin: &str, back: &str) -> Installed {
    let t = format!(
        r#"{{"ok":true,"running":{{"version":{version},"date":"2026-11-17","generation":{generation},"reason":"NEWEST_TRY","reload":0}},"medium":{{"ok":true,"writable":true,"pin":{pin},"generations":[{{"n":167,"markers":"O"}},{{"n":168,"markers":"NT"}}],"next":"167 newest confirmed","back":{back}}},"confirm":{{"pending":true,"state":"{state}","asks":{asks},"answered":false,"by":"","text":"generation {generation} waits"}}}}"#
    );
    parse_status(&Json::parse(&t).unwrap())
}

#[test]
fn an_unconfirmed_version_asks_with_its_way_back() {
    let i = node(168, 168, "pending", true, "null", "167");
    assert_eq!((i.state.as_str(), i.asks), ("pending", true));
    assert_eq!(decide(&i), Ask::Now(Question { version: 168, generation: 168, back: Some(167) }));
}

#[test]
fn confirmed_by_mcp_meanwhile_the_question_goes() {
    let mut i = node(168, 168, "confirmed", false, "null", "167");
    i.by = String::from("mcp");
    assert!(matches!(decide(&i), Ask::Never(w) if w.contains("schon bestätigt")));
    assert_eq!(outcome(&i), Some(Ok(Done::Confirmed { by: String::from("mcp") })));
    assert!(matches!(decide(&node(168, 168, "writing", false, "null", "167")), Ask::Never(_)));
    assert_eq!(outcome(&node(168, 168, "pending", true, "null", "167")), None, "still waiting for the write");
    assert!(outcome(&node(168, 168, "failed", false, "null", "167")).unwrap().is_err());
}

#[test]
fn a_development_build_without_a_number_never_asks() {
    assert!(matches!(decide(&node(0, 169, "pending", false, "null", "168")), Ask::Never(w) if w.contains("Entwicklungsstand")));
    assert!(matches!(decide(&node(168, 169, "pending", false, "null", "167")), Ask::Never(_)), "outside its own slot");
}

#[test]
fn no_confirmed_version_below_offers_only_ja() {
    assert_eq!(decide(&node(168, 168, "pending", true, "null", "null")), Ask::Now(Question { version: 168, generation: 168, back: None }));
}

#[test]
fn after_zurueck_the_question_waits() {
    assert!(matches!(decide(&node(168, 168, "pending", true, "167", "167")), Ask::Later(_)));
}

#[test]
fn while_the_kernel_writes_georgs_ja_the_screens_keep_looking() {
    let mut i = node(168, 168, "writing", false, "null", "167");
    i.answered = true;
    assert!(matches!(decide(&i), Ask::Later(_)), "a failed write brings the question back");
    i.state = String::from("pending");
    assert!(matches!(decide(&i), Ask::Later(_)), "taken, not written yet");
    i.state = String::from("confirmed");
    assert!(matches!(decide(&i), Ask::Never(_)));
    assert!(!node(168, 168, "pending", true, "null", "167").answered, "the node's field is read");
}
