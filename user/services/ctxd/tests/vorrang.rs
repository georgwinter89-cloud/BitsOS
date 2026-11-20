//! VORRANG (K2): the habit "which priority Georg gives which program": the DB4 threshold and decay, hoch only
//! proposed until Georg's "Ja", Rückgängig and "Nein", bad values and BitsOS's own services refused, the control
//! socket teaching nothing, the switch, and forgetting that really deletes from disk.

mod common;
use common::*;
use ctxd::config::Config;
use ctxd::engine::{Engine, EngineOpts};
use ctxd::habits::Habits;
use ctxd::habits_rules::{self as rules, Apply};
use serde_json::{Value, json};

fn set(h: &Habits, app: &str, value: &str) {
    h.event(&json!({"kind": "vorrang", "app": app, "value": value, "name": "Blender"})).unwrap();
}

fn item(h: &Habits, app: &str) -> Option<rules::Item> {
    rules::items(&h.records(), h.now()).into_iter().find(|i| i.id == format!("vorrang:{app}"))
}

fn vorrang(value: &str, ask: bool) -> Option<Apply> {
    Some(Apply::Vorrang { value: value.into(), ask })
}

fn on_disk(h: &Habits) -> String {
    std::fs::read_dir(h.dir()).unwrap().flatten().map(|e| std::fs::read_to_string(e.path()).unwrap_or_default()).collect()
}

#[test]
fn three_times_niedrig_is_imitated_with_its_reason_and_rueckgaengig_stops_it() {
    let d = tmp("vor-niedrig");
    let h = Habits::open(&d);
    set(&h, "blender", "niedrig");
    set(&h, "blender", "niedrig");
    assert!(item(&h, "blender").is_none(), "two are not a habit");
    set(&h, "blender", "niedrig");
    let i = item(&h, "blender").expect("three in a row");
    assert_eq!((i.state, i.apply.clone(), i.hinted), ("nachgemacht", vorrang("niedrig", false), false));
    assert_eq!(i.title, "Blender startet mit niedrigem Vorrang");
    assert_eq!(i.reason, "weil du das 3 von 3 Mal so eingestellt hast");
    h.event(&json!({"kind": "hint_shown", "id": "vorrang:blender"})).unwrap();
    assert!(item(&h, "blender").unwrap().hinted, "the hint comes once");
    // Rückgängig: back to normal and never imitated again until Georg deletes it on the page.
    h.event(&json!({"kind": "undo", "id": "vorrang:blender"})).unwrap();
    let i = item(&h, "blender").unwrap();
    assert_eq!((i.state, i.apply.clone()), ("abgelehnt", None));
    assert_eq!(i.title, "Blender: Vorrang wird nicht mehr nachgemacht");
    assert_eq!(i.reason, "weil du beim Nachmachen „Rückgängig“ gewählt hast");
    assert_eq!(item(&Habits::open(&d), "blender").unwrap().state, "abgelehnt", "after a restart the same");
}

#[test]
fn a_mixed_history_is_no_habit_and_old_settings_fade() {
    let h = Habits::open(&tmp("vor-share"));
    for v in ["normal", "normal", "normal", "niedrig", "niedrig", "niedrig"] {
        set(&h, "blender", v);
    }
    assert!(item(&h, "blender").is_none(), "3 of 6 is below the share");
    let h = Habits::open(&tmp("vor-decay"));
    for _ in 0..4 {
        set(&h, "blender", "normal");
    }
    // Four half-lives later the four old ones weigh 1/4; three new ones are 3/3.25 >= 0.75.
    h.advance(120 * 86_400);
    for _ in 0..3 {
        set(&h, "blender", "niedrig");
    }
    let i = item(&h, "blender").expect("the old ones faded");
    assert_eq!(i.reason, "weil du das 3 von 3 Mal so eingestellt hast", "the reason counts the last 60 days");
    // Unused for 70 days: the habit fades out and is no longer imitated.
    h.advance(70 * 86_400);
    assert!(item(&h, "blender").is_none(), "an unused habit stops");
}

#[test]
fn hoch_is_only_proposed_until_georg_says_ja_and_nein_declines() {
    let h = Habits::open(&tmp("vor-hoch"));
    for _ in 0..3 {
        set(&h, "blender", "hoch");
    }
    let i = item(&h, "blender").unwrap();
    assert_eq!((i.state, i.apply.clone()), ("vorgeschlagen", vorrang("hoch", true)), "only a proposal");
    assert_eq!(i.title, "Blender immer mit hohem Vorrang starten");
    h.event(&json!({"kind": "vorrang_ja", "app": "blender"})).unwrap();
    let i = item(&h, "blender").unwrap();
    assert_eq!((i.state, i.apply.clone(), i.hinted), ("nachgemacht", vorrang("hoch", false), true));
    assert_eq!(i.reason, "weil du das 3 von 3 Mal so eingestellt und „Ja“ gesagt hast");
    // Georg chose something else in between: his old yes does not carry over to a new run of hoch.
    h.advance(60);
    set(&h, "blender", "normal");
    for _ in 0..3 {
        set(&h, "blender", "hoch");
    }
    let i = item(&h, "blender").unwrap();
    assert_eq!((i.state, i.apply), ("vorgeschlagen", vorrang("hoch", true)), "asked again");
}

#[test]
fn a_yes_from_before_the_proposal_counts_nothing_and_nein_is_kept_as_such() {
    let h = Habits::open(&tmp("vor-ja-early"));
    h.event(&json!({"kind": "vorrang_ja", "app": "blender"})).unwrap();
    h.advance(60);
    for _ in 0..3 {
        set(&h, "blender", "hoch");
    }
    assert_eq!(item(&h, "blender").unwrap().state, "vorgeschlagen", "a yes before the proposal is none");
    h.event(&json!({"kind": "undo", "id": "vorrang:blender", "nein": true})).unwrap();
    let i = item(&h, "blender").unwrap();
    assert_eq!((i.state, i.reason.as_str(), i.apply.clone()), ("abgelehnt", "weil du „Nein“ gesagt hast", None));
}

#[test]
fn bad_values_own_services_and_the_control_socket_teach_nothing() {
    let h = Habits::open(&tmp("vor-bad"));
    for bad in [json!({"kind": "vorrang", "app": "blender", "value": "high"}), json!({"kind": "vorrang", "app": "blender", "value": "Hoch"}),
                json!({"kind": "vorrang", "app": "blender"}), json!({"kind": "vorrang", "value": "hoch"}), json!({"kind": "vorrang", "app": "blender", "value": -5}),
                json!({"kind": "vorrang", "app": "ctxd", "value": "niedrig"}), json!({"kind": "vorrang", "app": "bitscomp.system", "value": "hoch"}),
                json!({"kind": "vorrang_ja", "app": "settingsd"})] {
        assert!(h.event(&bad).is_err(), "refused: {bad}");
    }
    for _ in 0..3 {
        h.event(&json!({"kind": "vorrang", "app": "blender", "value": "niedrig", "by": "ctl"})).unwrap();
    }
    assert!(h.is_empty(), "the control socket's changes are not Georg's: nothing kept");
    let recent = h.history(60, 3);
    assert!(recent.len() == 3 && recent.iter().all(|r| r.kind == "vorrang" && r.app == "blender"), "but they are in the history (memory only)");
    assert!(on_disk(&h).is_empty());
}

#[test]
fn the_switch_off_stops_learning_and_forget_deletes_from_disk() {
    let h = Habits::open(&tmp("vor-switch"));
    h.set_switch("vorrang", false);
    for _ in 0..3 {
        set(&h, "blender", "niedrig");
    }
    assert!(h.is_empty(), "switched off: nothing learned");
    h.set_switch("vorrang", true);
    for _ in 0..3 {
        set(&h, "blender", "hoch");
        set(&h, "gimp", "niedrig");
    }
    h.event(&json!({"kind": "vorrang_ja", "app": "blender"})).unwrap();
    h.event(&json!({"kind": "hint_shown", "id": "vorrang:gimp"})).unwrap();
    assert!(on_disk(&h).contains("\"part\":\"ja\""));
    assert_eq!(h.forget("vorrang:blender"), Ok(4), "three settings and the yes");
    let disk = on_disk(&h);
    assert!(!disk.contains("blender"), "nothing of it stays on disk: {disk}");
    assert!(disk.contains("vorrang:gimp") && item(&h, "gimp").is_some(), "the other stays");
    let h2 = Habits::open(h.dir().parent().unwrap());
    assert!(item(&h2, "blender").is_none(), "also after a restart");
}

#[test]
fn the_ops_give_rules_list_and_switch_through_the_engine() {
    let d = tmp("vor-ops");
    let mut cfg = Config::default();
    cfg.context.embedder.kind = "none".into();
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: None }).unwrap();
    let call = |op: &str, a: Value| ctxd::habits_ops::call(&e, op, &a, true).unwrap();
    for _ in 0..3 {
        call("habit_event", json!({"kind": "vorrang", "app": "blender", "value": "hoch", "name": "Blender"})).unwrap();
    }
    assert_eq!(call("habit_event", json!({"kind": "vorrang", "app": "blender", "value": "sehr hoch"})).unwrap_err().code, "bad_request");
    let r = call("habit_rules", json!({})).unwrap();
    let rule = &r["rules"][0];
    assert_eq!((rule["id"].as_str(), rule["vorrang"].as_str(), rule["ask"].as_bool()), (Some("vorrang:blender"), Some("hoch"), Some(true)));
    assert!(rule.get("state").is_none() && rule.get("width").is_none(), "no window rule");
    let l = call("learned_list", json!({})).unwrap();
    assert!(l["parts"].as_array().unwrap().iter().any(|p| p["part"] == "vorrang" && p["name"] == "Vorrang je Programm"));
    let entry = l["entries"].as_array().unwrap().iter().find(|x| x["id"] == "vorrang:blender").expect("listed on the page");
    assert_eq!((entry["state"].as_str(), entry["title"].as_str()), (Some("vorgeschlagen"), Some("Blender immer mit hohem Vorrang starten")));
    assert_eq!((entry["kind"].as_str(), entry["app"].as_str(), entry["value"].as_str()), (Some("vorrang"), Some("blender"), Some("hoch")), "K2's fields");
    assert!(entry["reason"].as_str().is_some_and(|r| r.contains("3 von 3")));
    call("habit_event", json!({"kind": "undo", "id": "vorrang:blender", "nein": true})).unwrap();
    let l = call("learned_list", json!({})).unwrap();
    let entry = l["entries"].as_array().unwrap().iter().find(|x| x["id"] == "vorrang:blender").unwrap();
    assert_eq!((entry["state"].as_str(), entry["value"].as_str()), (Some("abgelehnt"), Some("hoch")), "declined: the value Georg set last");
    call("learned_switch", json!({"part": "vorrang", "on": false})).unwrap();
    assert!(call("habit_rules", json!({})).unwrap()["rules"].as_array().unwrap().is_empty(), "off: nothing applied");
    call("learned_switch", json!({"part": "vorrang", "on": true})).unwrap();
    assert_eq!(call("learned_forget", json!({"id": "vorrang:blender"})).unwrap()["forgotten"], 4);
    assert!(call("habit_rules", json!({})).unwrap()["rules"].as_array().unwrap().is_empty());
}

/// A program of the session that is not bitscomp (here: this test, on ctxd's real socket) cannot pose as Georg:
/// its Vorrang events teach nothing whatever `by` says, and its "Ja" to a high Vorrang is refused.
#[test]
fn only_the_desktop_speaks_for_georg_on_the_socket() {
    let d = tmp("vor-peer");
    let mut cfg = Config::default();
    cfg.context.embedder.kind = "none".into();
    let e = std::sync::Arc::new(Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: None }).unwrap());
    let path = d.join("ctxd.sock");
    let l = ctxd::sock::bind(&path).unwrap();
    let served = std::sync::Arc::clone(&e);
    std::thread::spawn(move || ctxd::sock::serve(served, l));
    let call = |op: &str, a: Value| ctxd::sock::request(&path, op, a, std::time::Duration::from_secs(10)).unwrap();
    for by in [None, Some("georg"), Some("georg")] {
        let mut ev = json!({"kind": "vorrang", "app": "blender", "value": "hoch", "name": "Blender"});
        if let Some(b) = by {
            ev["by"] = json!(b);
        }
        assert_eq!(call("habit_event", ev)["ok"], true, "taken into the history");
    }
    let ja = call("habit_event", json!({"kind": "vorrang_ja", "app": "blender"}));
    assert_eq!((ja["ok"].as_bool(), ja["error"]["code"].as_str()), (Some(false), Some("bad_request")), "{ja}");
    assert_eq!(call("habit_event", json!({"kind": " vorrang_ja ", "app": "blender"}))["ok"], false, "also with blanks around the kind");
    assert_eq!(call("habit_event", json!({"kind": "vorrang", "app": "blender", "value": "sehr"}))["ok"], false, "bad values still refused");
    assert!(call("habit_rules", json!({}))["result"]["rules"].as_array().unwrap().is_empty(), "nothing learned from a non-desktop peer");
    assert!(e.habits.records().iter().all(|r| r.part != "vorrang" && r.part != "ja"), "nothing recorded");
    // The same three and the yes from the desktop (the trusted path the socket takes for bitscomp) do teach.
    for _ in 0..3 {
        let r = ctxd::sock::handle_line(&e, r#"{"op":"habit_event","args":{"kind":"vorrang","app":"blender","value":"hoch","name":"Blender"}}"#, true);
        assert_eq!(r["ok"], true);
    }
    assert_eq!(ctxd::sock::handle_line(&e, r#"{"op":"habit_event","args":{"kind":"vorrang_ja","app":"blender"}}"#, true)["ok"], true);
    let rules = call("habit_rules", json!({}));
    let rule = &rules["result"]["rules"][0];
    assert_eq!((rule["vorrang"].as_str(), rule["ask"].as_bool()), (Some("hoch"), Some(false)), "{rules}");
    // Forgetting and declining stay open to every program of the session: they only take away.
    assert_eq!(call("learned_forget", json!({"id": "vorrang:blender"}))["ok"], true);
}
