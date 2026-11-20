//! The assistant's tree (PLAN K5): asked steps wait for Georg's answer and run once after his yes; a no, or no
//! answer, is exit 5 with nothing done; settings follow the level and never carry `confirmed`.

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::common::World;

/// bitscomp's side of a question: `ui-act` is asked, then the scripted answers to `bits-antwort 1`.
fn asking(answers: &[&'static str]) -> World {
    let w = World::new(&format!("ask-{}", answers.join("-")));
    let left = Arc::new(Mutex::new(answers.to_vec()));
    w.comp(move |l| {
        if l.starts_with("ui-act ") || l.starts_with("bits-frage ") {
            return "frage 1 120".into();
        }
        if l == "bits-antwort 1" {
            let mut a = left.lock().unwrap();
            return if a.len() > 1 { a.remove(0).into() } else { a.first().copied().unwrap_or("wartet").into() };
        }
        if l.starts_with("bits-antwort 1 nein") {
            return "nein".into();
        }
        if l.starts_with("bits-mit 1 ") {
            return "ok".into();
        }
        if l == "program-start Spiel" {
            return "refused: Auf der Stufe „Nur antworten“ handelt der Assistent nicht. Es wurde nichts getan.".into();
        }
        "error: ?".into()
    });
    w
}

#[test]
fn a_yes_runs_the_step_once() {
    let w = asking(&["wartet", "wartet", "ja"]);
    let (code, v) = w.tree(&["ui", "tun", "w5/ab", "type", "Hallo"]);
    assert_eq!((code, v["answer"].as_str()), (0, Some("ok")));
    let sent = World::sent(&w.ctl);
    assert_eq!(sent.first().map(String::as_str), Some("ui-act w5/ab type Hallo"));
    assert_eq!(sent.iter().filter(|l| *l == "bits-antwort 1").count(), 3);
    assert_eq!(sent.last().map(String::as_str), Some("bits-mit 1 ui-act w5/ab type Hallo"));
}

#[test]
fn a_no_or_silence_does_nothing() {
    let w = asking(&["wartet", "nein"]);
    let (code, v) = w.tree(&["ui", "tun", "w5/ab", "press"]);
    assert_eq!((code, v["error"].as_str()), (5, Some("refused")));
    assert!(v["message"].as_str().unwrap().contains("Nein"));
    assert!(!World::sent(&w.ctl).iter().any(|l| l.starts_with("bits-mit")));
    let w = asking(&["wartet"]);
    let (code, v) = w.json_env(&["ui", "tun", "w5/ab", "press"], &[("BITSOS_FAKE_TREE", "1"), ("BITS_FRAGE_S", "0.6")]);
    assert_eq!(code, 5);
    assert!(v["message"].as_str().unwrap().contains("nicht geantwortet"));
    let sent = World::sent(&w.ctl);
    assert_eq!(sent.last().map(String::as_str), Some("bits-antwort 1 nein"), "the question is withdrawn");
    assert!(!sent.iter().any(|l| l.starts_with("bits-mit")));
    let (code, v) = w.tree(&["programm", "start", "Spiel"]);
    assert_eq!((code, v["error"].as_str()), (5, Some("refused")), "bitscomp's refusal is exit 5");
}

#[test]
fn settings_from_the_tree_follow_the_level() {
    let level = Arc::new(Mutex::new("answer"));
    let w = asking(&["ja"]);
    let l2 = level.clone();
    w.settingsd(move |r| match r["op"].as_str() {
        Some("get") if r["key"] == "ai.level" => json!({"ok": true, "key": "ai.level", "value": *l2.lock().unwrap(), "changed": true}),
        Some("set") if r["key"] == "boot.x" => json!({"ok": false, "error": "needs_confirm", "message": "boot.x: ask Georg first"}),
        Some("set") => json!({"ok": true, "key": r["key"], "old": "a", "new": r["value"]}),
        _ => json!({"ok": false, "error": "bad_request", "message": "?"}),
    });
    let (code, v) = w.tree(&["einstellungen", "setzen", "desktop.theme", "dunkel"]);
    assert_eq!((code, v["message"].as_str().unwrap().contains("Nur antworten")), (5, true));
    *level.lock().unwrap() = "ask";
    assert_eq!(w.tree(&["einstellungen", "setzen", "desktop.theme", "dunkel"]).0, 0);
    assert!(World::sent(&w.ctl).contains(&"bits-frage einstellung desktop.theme dunkel".to_string()), "asked on the desktop first");
    *level.lock().unwrap() = "auto";
    assert_eq!(w.tree(&["einstellungen", "setzen", "desktop.theme", "hell"]).0, 0);
    assert_eq!(w.tree(&["einstellungen", "setzen", "boot.x", "1"]).0, 5, "a key that asks first stays Georg's");
    assert_eq!(w.tree(&["einstellungen", "setzen", "desktop.theme", "hell", "--bestaetigt"]).0, 5);
    let sets: Vec<serde_json::Value> = World::sent(&w.settings).iter().map(|l| serde_json::from_str(l).unwrap()).filter(|r: &serde_json::Value| r["op"] == "set").collect();
    assert_eq!(sets.len(), 3);
    assert!(sets.iter().all(|r| r["confirmed"] == false && r["source"] == "assistent"), "{sets:?}");
}
