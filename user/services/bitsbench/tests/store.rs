//! The night's JSON, the 30-night history, the folders kept and the morning line.

use bitsbench::report::{Tonight, line};
use bitsbench::store::{self, Entry, Night, Outcome};
use std::collections::{BTreeMap, BTreeSet};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bitsbench-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn night(date: &str, boot: f64) -> Night {
    let mut b = BTreeMap::new();
    b.insert("boot".to_string(), Outcome::Runs(vec![boot, boot + 40.0, boot - 20.0, boot + 10.0, boot]));
    b.insert("file_read".to_string(), Outcome::Runs(vec![1000.0; 5]));
    b.insert("steam_start".to_string(), Outcome::Missing("Steam ist auf diesem Rechner nicht da".into()));
    Night { night: date.into(), started_ms: 1, ended_ms: 2, generation: Some(150), origin: "von Hand gebaut".into(), system: "bitsos".into(), benches: b, profile: Err("Der Profiler ist auf diesem Rechner nicht da.".into()) }
}

fn entry(date: &str, boot: f64, read: f64) -> Entry {
    entry_of(date, 150, boot, read)
}

fn entry_of(date: &str, generation: u64, boot: f64, read: f64) -> Entry {
    Entry {
        night: date.into(),
        generation: Some(generation),
        origin: "von Hand gebaut".into(),
        medians: [("boot".to_string(), boot), ("file_read".to_string(), read)].into(),
        runs: [("boot".to_string(), 5), ("file_read".to_string(), 5)].into(),
    }
}

/// The line for tonight's medians, generation 150, every benchmark with its five runs.
fn said(m: &BTreeMap<String, f64>, before: &[Entry]) -> String {
    line(&Tonight { medians: m, few: &BTreeSet::new(), generation: Some(150), origin: "von Hand gebaut" }, before)
}

#[test]
fn a_night_goes_to_json_and_back() {
    let n = night("2026-11-14", 12_000.0);
    let j = n.to_json();
    let t = j.to_string();
    for want in [
        r#""night":"2026-11-14""#,
        r#""generation":150"#,
        r#""origin":"von Hand gebaut""#,
        r#""boot":{"runs":[12000,12040,11980,12010,12000],"median":12000,"spread":60,"unit":"ms"}"#,
        r#""steam_start":{"missing":"Steam ist auf diesem Rechner nicht da","unit":"ms"}"#,
        r#""profile":{"missing":"#,
    ] {
        assert!(t.contains(want), "{want} in {t}");
    }
    assert_eq!(Night::from_json(&mcpreg::Json::parse(&t).unwrap()), Some(n.clone()));
    assert_eq!(n.medians().get("boot"), Some(&12_000.0));
    assert!(!n.medians().contains_key("steam_start"));
}

#[test]
fn the_history_keeps_thirty_nights_and_a_night_once() {
    let mut h = Vec::new();
    for d in 1..=40u32 {
        h = store::history_with(h, entry(&format!("2026-10-{:02}", d.min(31)), d as f64, 1.0));
    }
    // Days 31..40 are one night (the 31st): measured again it replaces itself.
    assert_eq!(h.len(), store::KEEP);
    assert_eq!(h.last().unwrap().medians["boot"], 40.0);
    assert_eq!(h.first().unwrap().night, "2026-10-02");
    let back = store::history_of_json(&mcpreg::Json::parse(&store::history_to_json(&h).to_string()).unwrap());
    assert_eq!(back, h);
}

#[test]
fn a_broken_history_is_kept_aside_and_old_folders_go() {
    let root = tmp("prune");
    let b = store::bench_dir(&root);
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join("history.json"), "{not json").unwrap();
    assert!(store::load_history(&root).is_empty());
    assert!(b.join("history.json.bad").is_file(), "never overwritten unread");
    for d in 1..=33 {
        std::fs::create_dir_all(b.join(format!("2026-10-{d:02}"))).unwrap();
    }
    std::fs::create_dir_all(b.join("other")).unwrap();
    assert_eq!(store::prune(&root), vec!["2026-10-01", "2026-10-02", "2026-10-03"]);
    assert!(b.join("other").is_dir(), "only night folders are pruned");
    assert!(b.join("2026-10-04").is_dir());
}

#[test]
fn the_morning_line_says_what_moved() {
    let before: Vec<Entry> = (1..=9).map(|d| entry(&format!("2026-11-0{d}"), 12_000.0, 1000.0)).collect();
    let now = |boot: f64, read: f64| -> BTreeMap<String, f64> { [("boot".to_string(), boot), ("file_read".to_string(), read)].into() };
    assert_eq!(said(&now(12_300.0, 980.0), &before), "Nacht-Messung: alles im Rahmen");
    assert_eq!(said(&now(13_440.0, 1000.0), &before), "Nacht-Messung: Hochfahren 12 % langsamer");
    // Fewer MB/s is slower; more ms is slower; the largest change first.
    assert_eq!(said(&now(10_800.0, 700.0), &before), "Nacht-Messung: Dateien lesen 30 % langsamer, Hochfahren 10 % schneller");
    assert_eq!(said(&now(1.0, 1.0), &[]), "Nacht-Messung: erste Messung, noch nichts zum Vergleichen");
    assert_eq!(said(&BTreeMap::new(), &before), "Nacht-Messung: nichts gemessen");
    // Only the last 7 nights make the usual range.
    let mut drift = before.clone();
    drift.extend((10..=16).map(|d| entry(&format!("2026-11-{d}"), 14_000.0, 1000.0)));
    assert_eq!(said(&now(14_000.0, 1000.0), &drift), "Nacht-Messung: alles im Rahmen");
}

#[test]
fn a_new_generation_is_held_against_the_one_before_and_named() {
    let now = |boot: f64, read: f64| -> BTreeMap<String, f64> { [("boot".to_string(), boot), ("file_read".to_string(), read)].into() };
    let none = BTreeSet::new();
    let at = |g: u64, m: &BTreeMap<String, f64>, before: &[Entry]| line(&Tonight { medians: m, few: &none, generation: Some(g), origin: "von Hand gebaut" }, before);
    // Generation 150's nights, then two nights of 151 that boot 20 % faster.
    let mut h: Vec<Entry> = (1..=5).map(|d| entry_of(&format!("2026-11-0{d}"), 150, 10_000.0, 1000.0)).collect();
    let fast = now(8000.0, 1000.0);
    assert_eq!(at(151, &fast, &h), "Nacht-Messung nach dem Wechsel auf Generation 151: Hochfahren 20 % schneller");
    h.push(entry_of("2026-11-06", 151, 8000.0, 1000.0));
    // The second night of 151 is held against 151 only: no wobble from the old generation's nights.
    assert_eq!(at(151, &now(8100.0, 1000.0), &h), "Nacht-Messung: alles im Rahmen");
    // Back to 150 (a generation rolled back): held against 150's own nights.
    assert_eq!(at(150, &now(10_100.0, 1000.0), &h), "Nacht-Messung: alles im Rahmen");
    // Another origin of the same number counts as a change too.
    let selfmade = line(&Tonight { medians: &fast, few: &none, generation: Some(151), origin: "selbst optimiert" }, &h);
    assert_eq!(selfmade, "Nacht-Messung nach dem Wechsel auf „selbst optimiert“: alles im Rahmen");
}

#[test]
fn a_value_from_few_runs_is_kept_but_not_compared() {
    let before: Vec<Entry> = (1..=5).map(|d| entry(&format!("2026-11-0{d}"), 12_000.0, 1000.0)).collect();
    let m: BTreeMap<String, f64> = [("boot".to_string(), 15_000.0), ("file_read".to_string(), 1000.0)].into();
    let few: BTreeSet<String> = ["boot".to_string()].into();
    let t = |few: &BTreeSet<String>| line(&Tonight { medians: &m, few, generation: Some(150), origin: "von Hand gebaut" }, &before);
    assert_eq!(t(&BTreeSet::new()), "Nacht-Messung: Hochfahren 25 % langsamer");
    assert_eq!(t(&few), "Nacht-Messung: alles im Rahmen", "one boot moving says nothing");
    let only: BTreeMap<String, f64> = [("boot".to_string(), 15_000.0)].into();
    assert_eq!(line(&Tonight { medians: &only, few: &few, generation: Some(150), origin: "von Hand gebaut" }, &before), "Nacht-Messung: erste Messung, noch nichts zum Vergleichen");
    // Which ones are few: fewer than 3 runs, or than the set asked for when that is less.
    let mut n = night("2026-11-14", 12_000.0);
    n.benches.insert("boot".into(), Outcome::Runs(vec![9000.0]));
    assert_eq!(n.few(5), ["boot".to_string()].into());
    assert_eq!(n.few(1), BTreeSet::new(), "a set of one run each compares single runs");
    // The counts go into the history.
    let e = Entry::of(&n);
    assert_eq!((e.runs["boot"], e.runs["file_read"], e.runs.get("steam_start")), (1, 5, None));
    let back = store::history_of_json(&mcpreg::Json::parse(&store::history_to_json(std::slice::from_ref(&e)).to_string()).unwrap());
    assert_eq!(back, vec![e]);
}

#[test]
fn the_boot_takes_what_this_generation_has() {
    use bitsbench::bench::boot::{Boot, last_of};
    let b = |at_s, ms, g| Boot { at_s, ms, generation: Some(g) };
    let v = vec![b(10, 9000, 150), b(20, 9100, 150), b(30, 8000, 151)];
    assert_eq!(last_of(&v, Some(151), 5), vec![8000.0], "right after a new generation: one start, not five");
    assert_eq!(last_of(&v, Some(150), 5), vec![9000.0, 9100.0]);
    assert_eq!(last_of(&v, Some(152), 5), Vec::<f64>::new());
}

#[test]
fn the_settings_register_as_settingsd_reads_them() {
    let j = settingsd::json::parse(&bitsbench::settings::register_request()).unwrap();
    let Some(settingsd::json::Json::Arr(keys)) = j.get("keys") else { panic!("no keys") };
    let defs: Vec<_> = keys.iter().map(|k| settingsd::key::KeyDef::from_json(k, "bitsbench").unwrap_or_else(|e| panic!("{e}: {}", k.to_text()))).collect();
    assert_eq!(defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["leistung.nachts", "leistung.von", "leistung.bis"]);
    assert_eq!(defs[0].safety, settingsd::key::Safety::Confirm, "only Georg's own yes turns it off and on again");
    assert_eq!(defs[0].default, settingsd::value::Value::Bool(true), "on by default (DECISIONS 18.11.)");
    let mut s = bitsbench::rules::Settings::default();
    bitsbench::settings::set(&mut s, "leistung.von", &mcpreg::Json::Num(99.0));
    bitsbench::settings::set(&mut s, "leistung.nachts", &mcpreg::Json::str("ja"));
    assert_eq!(s, bitsbench::rules::Settings { on: true, from_h: 23, to_h: 5 }, "a wrong type keeps the value before");
    let (d, why) = bitsbench::settings::load("/nonexistent/settings.sock");
    assert_eq!(d, bitsbench::rules::Settings::default());
    assert!(why.is_some());
}

#[test]
fn boots_and_frames_and_games_are_read_right() {
    use bitsbench::bench::{boot, desktop, game};
    let text = "bitsos-boot 1\nseq 9\ndrawn 9\nscreen gone\ngeneration 150\nstep kernel done 120 x\nstep sound skipped 300 -\nstep desktop done 9870 bitscomp läuft\n";
    assert_eq!(bitsbench::probe::boot_ms(text), Some(9870));
    assert_eq!(bitsbench::probe::boot_ms(&text.replace("desktop done", "desktop running")), None, "still starting");
    assert_eq!(bitsbench::probe::parse_generation("generation=150 reason=NEWEST_OK reload=0 version=0.1.0"), Some(150));
    assert_eq!(bitsbench::probe::origin_of("generation=150 reason=x"), "von Hand gebaut");
    let b = |at_s, ms, g| boot::Boot { at_s, ms, generation: Some(g) };
    let mut v = Vec::new();
    for (i, g) in [149, 150, 150, 150].iter().enumerate() {
        v = boot::with(v, b(1000 * i as i64, 9000 + i as u64, *g));
    }
    v = boot::with(v, b(3010, 1, 150));
    assert_eq!(boot::last_of(&v, Some(150), 5), vec![9001.0, 9002.0, 1.0], "one boot noted once, only this generation");
    let sa = "Startup finished in 4.120s (firmware) + 1.002s (loader) + 1.500s (kernel) + 6.250s (userspace) = 12.872s\ngraphical.target reached after 1min 2.250s in userspace.\n";
    assert_eq!(boot::systemd_ms(sa).map(|x| x.round()), Some(63_750.0));
    assert_eq!(game::timedemo("969 frames  4.2 seconds 230.7 fps"), Some(4200.0 / 969.0));
    assert_eq!(game::timedemo("Timedemo"), None);
    // bitscomp's `stats` answer as its Stats::report writes it (bitscomp's stats_tests check the same keys).
    let real = "frames=120 empty=4 commits=300 render_avg_ms=2.250 build_avg_ms=0.500 prepare_avg_ms=0.250 present_avg_ms=0.750 damaged_mpx_avg=1.000 input_reports=0 output=2560x1440 launching=0 spawn=trampoline renderer=gles";
    assert!((desktop::value(real).unwrap() - 3.0).abs() < 1e-9);
    assert_eq!(desktop::value("frames=0 render_avg_ms=0.000 present_avg_ms=0.000"), None, "no frame drawn yet");
    assert_eq!(desktop::value("unknown command 'bench'"), None);
}
