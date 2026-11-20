//! DB4: the habit rules (thresholds, decay, together, times, sound), forgetting that really deletes from disk,
//! the switches, and the C4 ops through the engine.

mod common;
use common::*;
use ctxd::config::Config;
use ctxd::engine::{Engine, EngineOpts};
use ctxd::habits::Habits;
use ctxd::habits_rules::{self as rules, Apply};
use serde_json::{Value, json};

fn ev(h: &Habits, v: Value) {
    h.event(&v).unwrap();
}

fn close(h: &Habits, app: &str, state: &str) {
    ev(h, json!({"kind": "window_close", "app": app, "state": state, "name": "Brave"}));
}

fn on_disk(h: &Habits) -> String {
    let mut all = String::new();
    for e in std::fs::read_dir(h.dir()).unwrap().flatten() {
        all.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
    }
    all
}

fn rule(h: &Habits, id: &str) -> Option<rules::Item> {
    rules::items(&h.records(), h.now()).into_iter().find(|i| i.id == id)
}

#[test]
fn three_maximised_closes_in_a_row_are_imitated_with_one_hint_and_undo_stops_it() {
    let d = tmp("hab-max");
    let h = Habits::open(&d);
    close(&h, "brave", "maximised");
    close(&h, "brave", "maximised");
    assert!(rule(&h, "fenster:brave").is_none(), "two are not a habit");
    close(&h, "brave", "maximised");
    let r = rule(&h, "fenster:brave").expect("three in a row");
    assert_eq!((r.state, r.apply.clone(), r.hinted), ("nachgemacht", Some(Apply::State("maximised".into())), false));
    assert_eq!(r.title, "Brave startet maximiert");
    assert_eq!(r.reason, "weil du Brave 3 von 3 Mal maximiert hast");
    ev(&h, json!({"kind": "hint_shown", "id": "fenster:brave"}));
    assert!(rule(&h, "fenster:brave").unwrap().hinted, "the hint comes once");
    ev(&h, json!({"kind": "undo", "id": "fenster:brave"}));
    let r = rule(&h, "fenster:brave").unwrap();
    assert_eq!((r.state, r.apply), ("abgelehnt", None), "Rückgängig: never imitated again until deleted");
    close(&h, "brave", "tiled");
    let r = rule(&h, "fenster:brave").expect("a declined habit stays on the page, deletable");
    assert_eq!(r.title, "Brave: Fenstergröße wird nicht mehr nachgemacht");
    // After a restart the same.
    assert_eq!(rule(&Habits::open(&d), "fenster:brave").unwrap().state, "abgelehnt");
}

#[test]
fn the_share_counts_and_old_closes_fade() {
    let h = Habits::open(&tmp("hab-share"));
    for _ in 0..5 {
        close(&h, "brave", "tiled");
    }
    for _ in 0..3 {
        close(&h, "brave", "maximised");
    }
    assert!(rule(&h, "fenster:brave").is_none(), "3 of 8 is below the share");
    let h = Habits::open(&tmp("hab-decay"));
    for _ in 0..5 {
        close(&h, "brave", "tiled");
    }
    // Four half-lives later the five old closes weigh 5/16; three new ones are 3/(3 + 0.31) >= 0.75.
    h.advance(120 * 86_400);
    for _ in 0..3 {
        close(&h, "brave", "maximised");
    }
    let r = rule(&h, "fenster:brave").expect("the old tiled closes faded");
    assert_eq!(r.reason, "weil du Brave 3 von 3 Mal maximiert hast", "the reason counts what the decision weighed");
    assert!((rules::weight(0, 30 * 86_400) - 0.5).abs() < 1e-9, "half-life 30 days");
}

#[test]
fn forgetting_one_habit_deletes_its_records_from_disk_and_keeps_the_others() {
    let d = tmp("hab-forget");
    let h = Habits::open(&d);
    for _ in 0..3 {
        ev(&h, json!({"kind": "program_start", "app": "brave", "hour": 20}));
        close(&h, "brave", "maximised");
    }
    ev(&h, json!({"kind": "hint_shown", "id": "fenster:brave"}));
    assert!(on_disk(&h).contains("\"part\":\"fenster\""));
    assert_eq!(h.forget("fenster:brave"), Ok(4), "three closes and the hint");
    let disk = on_disk(&h);
    assert!(!disk.contains("\"fenster\"") && !disk.contains("fenster:brave"), "nothing of it on disk: {disk}");
    assert_eq!(disk.matches("\"part\":\"zeiten\"").count(), 3, "the starts stay");
    let again = Habits::open(&d);
    assert!(rule(&again, "fenster:brave").is_none());
    assert_eq!(again.records().len(), 3);
    assert_eq!(again.forget("fenster:brave"), Ok(0));
    assert_eq!(again.forget_all(), Ok(3));
    assert!(on_disk(&again).is_empty(), "Alles vergessen leaves empty files only");
    assert!(Habits::open(&d).is_empty());
}

#[test]
fn programs_started_together_are_proposed_and_suggested() {
    let h = Habits::open(&tmp("hab-together"));
    for i in 0..4 {
        ev(&h, json!({"kind": "program_start", "app": "steam", "name": "Steam", "hour": 20}));
        if i < 3 {
            ev(&h, json!({"kind": "program_start", "app": "discord", "name": "Discord", "hour": 20}));
        }
        h.advance(3600);
    }
    let r = rule(&h, "zusammen:discord+steam").expect("3 of 4");
    assert_eq!(r.state, "vorgeschlagen", "a button is only proposed, never made");
    assert_eq!(r.reason, "weil du Discord und Steam 3 von 3 Mal zusammen startest");
    let s = rules::suggest(&h.records(), h.now(), 20, &["steam".into()]);
    let d = s.iter().find(|x| x.app.as_deref() == Some("discord")).expect("Discord fits to the running Steam");
    assert!(d.reason.starts_with("passt zu Steam, das gerade läuft"), "{}", d.reason);
    assert!(s.iter().all(|x| x.app.as_deref() != Some("steam")), "an open program is not offered");
    let s = rules::suggest(&h.records(), h.now(), 20, &[]);
    let t = s.iter().find(|x| x.kind == "together").expect("both closed: the pair as one proposal");
    assert_eq!(t.action.as_deref(), Some("start:discord+steam"));
    // Two starts far apart are not together.
    let h = Habits::open(&tmp("hab-apart"));
    for _ in 0..3 {
        ev(&h, json!({"kind": "program_start", "app": "steam"}));
        h.advance(rules_gap());
        ev(&h, json!({"kind": "program_start", "app": "discord"}));
        h.advance(3600);
    }
    assert!(rule(&h, "zusammen:discord+steam").is_none());
}

fn rules_gap() -> i64 {
    ctxd::habits::TOGETHER_S + 1
}

#[test]
fn times_widths_sound_and_picks() {
    let h = Habits::open(&tmp("hab-misc"));
    for hour in [19, 20, 21, 20, 9] {
        ev(&h, json!({"kind": "program_start", "app": "steam", "name": "Steam", "hour": hour}));
        h.advance(86_400);
    }
    let r = rule(&h, "zeiten:steam").expect("4 of 5 in the evening");
    assert_eq!(r.title, "Steam meist zwischen 18 und 22 Uhr");
    let s = rules::suggest(&h.records(), h.now(), 20, &[]);
    assert_eq!(s[0].app.as_deref(), Some("steam"));
    assert!(rules::suggest(&h.records(), h.now(), 3, &[]).is_empty(), "nothing fits at three in the night");
    for w in [1.5, 1.45, 1.55] {
        ev(&h, json!({"kind": "window_close", "app": "foot", "state": "tiled", "tile": w, "name": "Terminal"}));
    }
    assert_eq!(rule(&h, "breite:foot").unwrap().apply, Some(Apply::Width(1.5)));
    for w in [1.0, 1.05, 0.95] {
        ev(&h, json!({"kind": "window_close", "app": "calc", "state": "tiled", "tile": w}));
    }
    assert!(rule(&h, "breite:calc").is_none(), "the plain width is no habit");
    // A dragged slider is one change: the last value counts.
    for v in [30.0, 40.0, 60.0] {
        ev(&h, json!({"kind": "sound", "app": "mpv", "output": "HDMI", "volume": v}));
    }
    assert!(rule(&h, "ton:mpv").is_none(), "one drag");
    for _ in 0..2 {
        h.advance(600);
        ev(&h, json!({"kind": "sound", "app": "mpv", "output": "HDMI", "volume": 62}));
    }
    assert_eq!(rule(&h, "ton:mpv").unwrap().title, "mpv: Ton über HDMI mit 62 %");
    for _ in 0..3 {
        ev(&h, json!({"kind": "pick", "via": "library", "target": "programm/steam", "name": "Steam"}));
    }
    assert_eq!(rule(&h, "auswahl:programm/steam").unwrap().reason, "weil du es 3 Mal in der Programmübersicht gewählt hast");
    assert!(rules::ranks(&h.records(), h.now())["programm/steam"] > 2.9);
    assert!(h.event(&json!({"kind": "window_close", "app": "x", "state": "huge"})).is_err());
    assert!(h.event(&json!({"kind": "program_start"})).is_err(), "no app");
}

#[test]
fn a_switched_off_part_learns_nothing_and_history_stays_in_memory() {
    let d = tmp("hab-switch");
    let h = Habits::open(&d);
    h.set_switch("fenster", false);
    close(&h, "brave", "maximised");
    assert!(h.is_empty());
    h.set_switch("fenster", true);
    h.set_switch("enabled", false);
    close(&h, "brave", "maximised");
    assert!(h.is_empty(), "the master switch");
    ev(&h, json!({"kind": "ui", "app": "foot", "window": "w1", "element": "w1/3", "text": "geheimes Wort"}));
    let hist = h.history(60, 10);
    assert_eq!(hist.len(), 3);
    assert_eq!(hist[2].text.as_deref(), Some("geheimes Wort"));
    assert!(!on_disk(&h).contains("geheimes"), "history never on disk");
}

#[test]
fn the_ops_list_forget_and_switch_through_the_engine() {
    let d = tmp("hab-ops");
    let mut cfg = Config::default();
    cfg.context.embedder.kind = "none".into();
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: None }).unwrap();
    let call = |op: &str, a: Value| ctxd::habits_ops::call(&e, op, &a, true).unwrap();
    for _ in 0..3 {
        call("habit_event", json!({"kind": "window_close", "app": "habtest", "state": "maximised", "name": "Prüfprogramm"})).unwrap();
    }
    let rules = call("habit_rules", json!({})).unwrap();
    assert_eq!(rules["rules"][0]["state"], "maximised");
    assert_eq!(rules["rules"][0]["hinted"], false);
    let l = call("learned_list", json!({})).unwrap();
    assert_eq!(l["parts"].as_array().unwrap().len(), 10);
    let entry = &l["entries"][0];
    assert_eq!((entry["id"].as_str(), entry["title"].as_str()), (Some("fenster:habtest"), Some("Prüfprogramm startet maximiert")));
    let dir = e.habits.dir().to_path_buf();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"").unwrap();
    assert_eq!(call("learned_forget", json!({"id": "fenster:habtest"})).unwrap_err().code, "internal", "the page says it could not delete");
    std::fs::remove_file(&dir).unwrap();
    std::fs::create_dir(&dir).unwrap();
    assert_eq!(call("learned_forget", json!({"id": "fenster:habtest"})).unwrap()["forgotten"], 3);
    assert_eq!(call("learned_forget", json!({"id": "fenster:habtest"})).unwrap_err().code, "not_found");
    assert!(call("habit_rules", json!({})).unwrap()["rules"].as_array().unwrap().is_empty());
    assert_eq!(call("learned_switch", json!({"part": "zeiten", "on": false})).unwrap()["kept"], false, "no settingsd here");
    call("habit_event", json!({"kind": "program_start", "app": "habtest"})).unwrap();
    assert!(e.habits.is_empty(), "zeiten off");
    assert!(call("learned_switch", json!({"part": "nichts", "on": false})).is_err());
    assert!(call("suggest", json!({"hour": 20, "open_apps": []})).unwrap().as_array().unwrap().is_empty());
    assert_eq!(call("learned_forget", json!({"id": "all"})).unwrap()["forgotten"], 0);
    assert!(ctxd::habits_ops::call(&e, "context_search", &json!({}), true).is_none(), "other ops are not the memory's");
}

#[test]
fn an_unused_habit_fades_and_after_half_a_year_its_records_leave_the_disk() {
    let d = tmp("hab-prune");
    let h = Habits::open(&d);
    for _ in 0..3 {
        close(&h, "brave", "maximised");
    }
    ev(&h, json!({"kind": "program_start", "app": "steam", "hour": 20}));
    for _ in 0..3 {
        ev(&h, json!({"kind": "pick", "target": "programm/steam"}));
    }
    assert!(rule(&h, "fenster:brave").is_some());
    h.advance(70 * 86_400);
    assert!(rule(&h, "fenster:brave").is_none(), "three closes of 70 days ago are no habit any more");
    assert!(rule(&h, "auswahl:programm/steam").is_none(), "old picks are no longer listed");
    assert!(rules::ranks(&h.records(), h.now()).is_empty(), "and rank nothing");
    assert!(!h.prune_due(), "nothing is past half a year yet");
    assert!(on_disk(&h).contains("\"fenster\""));
    h.advance(130 * 86_400);
    assert!(h.prune_due(), "the daily prune rewrites");
    assert!(h.is_empty());
    let disk = on_disk(&h);
    assert!(!disk.contains("brave") && !disk.contains("steam"), "nothing of it on disk: {disk}");
    assert!(!h.prune_due(), "at most once a day");
    assert!(Habits::open(&d).is_empty());
}

#[test]
fn every_suggestion_and_rank_has_an_entry_on_the_page() {
    let h = Habits::open(&tmp("hab-listed"));
    for _ in 0..2 {
        ev(&h, json!({"kind": "program_start", "app": "steam", "name": "Steam", "hour": 20}));
        h.advance(86_400);
    }
    ev(&h, json!({"kind": "pick", "target": "programm/steam"}));
    assert!(rules::suggest(&h.records(), h.now(), 20, &[]).is_empty(), "two starts are not listed, so not suggested");
    assert!(rules::ranks(&h.records(), h.now()).is_empty(), "one pick is not listed, so it ranks nothing");
    for _ in 0..3 {
        ev(&h, json!({"kind": "program_start", "app": "steam", "name": "Steam", "hour": 20}));
        ev(&h, json!({"kind": "program_start", "app": "discord", "name": "Discord", "hour": 20}));
        ev(&h, json!({"kind": "pick", "target": "programm/steam"}));
        h.advance(86_400);
    }
    let ids: Vec<String> = rules::items(&h.records(), h.now()).into_iter().map(|i| i.id).collect();
    let s = rules::suggest(&h.records(), h.now(), 20, &[]);
    assert!(!s.is_empty());
    for x in &s {
        let ok = match x.kind {
            "together" => ids.iter().any(|i| i.starts_with("zusammen:")),
            "sofa" => ids.contains(&"sofa:sofa".to_string()),
            _ => ids.contains(&format!("zeiten:{}", x.app.as_deref().unwrap_or(""))) || ids.iter().any(|i| i.starts_with("zusammen:")),
        };
        assert!(ok, "{x:?} has no entry among {ids:?}");
    }
    for k in rules::ranks(&h.records(), h.now()).keys() {
        assert!(ids.contains(&format!("auswahl:{k}")), "rank {k} has no entry");
    }
}

#[test]
fn a_close_without_state_teaches_nothing_and_a_bad_kind_is_refused() {
    let h = Habits::open(&tmp("hab-nostate"));
    ev(&h, json!({"kind": "window_close", "app": "brave"}));
    assert!(h.is_empty(), "a dialog's or the sofa mode's close is history only");
    assert_eq!(h.history(60, 10).len(), 1);
    assert!(h.event(&json!({"kind": "x".repeat(41), "app": "a"})).is_err(), "too long");
    assert!(h.event(&json!({"kind": "Programm Start", "app": "a"})).is_err(), "not a kind");
    assert!(h.event(&json!({"kind": "ui", "app": "a"})).is_ok());
    assert_eq!(h.history(60, 10).len(), 2, "a refused kind is not kept");
}

#[test]
fn an_overflow_trims_with_room_so_not_every_record_rewrites() {
    let h = Habits::open(&tmp("hab-overflow"));
    let rec = |i: usize| ctxd::habits::Rec { part: "auswahl".into(), key: format!("t{}", i % 7), ..Default::default() };
    for i in 0..=ctxd::habits::MAX_RECORDS {
        h.record(rec(i));
    }
    assert_eq!(h.len(), ctxd::habits::TRIM_TO, "trimmed with room");
    h.record(rec(0));
    assert_eq!(h.len(), ctxd::habits::TRIM_TO + 1, "the next record is only appended");
}

#[test]
fn a_forget_the_disk_refuses_forgets_nothing() {
    let d = tmp("hab-refuse");
    let h = Habits::open(&d);
    for _ in 0..3 {
        close(&h, "brave", "maximised");
    }
    // The memory's folder replaced by a file: no new file can be written there (also not as root).
    std::fs::remove_dir_all(h.dir()).unwrap();
    std::fs::write(h.dir(), b"").unwrap();
    assert!(h.forget("fenster:brave").is_err());
    assert_eq!(h.len(), 3, "still in memory, as on disk");
    assert!(rule(&h, "fenster:brave").is_some());
    std::fs::remove_file(h.dir()).unwrap();
    std::fs::create_dir(h.dir()).unwrap();
    assert_eq!(h.forget("fenster:brave"), Ok(3));
}
