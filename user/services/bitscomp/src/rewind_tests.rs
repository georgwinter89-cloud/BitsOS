use super::*;

/// 2026-11-12 14:30 UTC (a Thursday), Berlin's winter offset.
const NOW: u64 = 1_794_493_800;
const OFF: i64 = 60;

fn list(snaps: &[(u64, u64, &str, &str)], data: &str) -> Json {
    let s: Vec<Json> = snaps
        .iter()
        .map(|(id, t, kind, label)| Json::obj(vec![("id", Json::Num(*id as f64)), ("created_s", Json::Num(*t as f64)), ("kind", Json::str(kind)), ("label", Json::str(label))]))
        .collect();
    Json::obj(vec![("ok", Json::Bool(true)), ("data", Json::str(data)), ("snapshots", Json::Arr(s)), ("utc_offset_min", Json::Num(OFF as f64)), ("now_s", Json::Num(NOW as f64))])
}

#[test]
fn times_in_german() {
    assert_eq!(clock(NOW, OFF), "15:30");
    assert_eq!(clock(NOW, 0), "14:30");
    assert_eq!(day(NOW - 600, OFF, NOW), "Heute");
    assert_eq!(day(NOW - 86_400, OFF, NOW), "Gestern");
    assert_eq!(day(NOW - 2 * 86_400, OFF, NOW), "Di 10.11.");
    assert_eq!(date_long(NOW, OFF), "Do 12.11.2026");
    assert_eq!(when(NOW - 600, OFF, NOW), "heute um 15:20");
    assert_eq!(when(NOW - 3 * 86_400, OFF, NOW), "am Mo 09.11. um 15:30");
    assert_eq!((ago(NOW - 20, NOW), ago(NOW - 720, NOW), ago(NOW - 7200, NOW)), ("gerade eben".into(), "vor 12 Min".into(), "vor 2 Std".into()));
    assert_eq!((ago(NOW - 86_400, NOW), ago(NOW - 3 * 86_400, NOW)), ("vor 1 Tag".into(), "vor 3 Tagen".into()));
    // Midnight local is a new day although UTC still says yesterday.
    let midnight = 1_794_438_000; // 2026-11-12 00:00 in Berlin = 11-11 23:00 UTC
    assert_eq!((day(midnight, OFF, NOW), day(midnight - 1, OFF, NOW)), ("Heute".into(), "Gestern".into()));
}

#[test]
fn sizes_and_tallies_in_words() {
    assert_eq!((size(999), size(12_345), size(12_345_678)), ("999 Byte".into(), "12,3 kB".into(), "12,3 MB".into()));
    let t = |s: &str| tally(&Json::parse(s).unwrap());
    assert_eq!(t(r#"{"kind":"file","files":1,"dirs":0,"bytes":12345}"#), "eine Datei, 12,3 kB");
    assert_eq!(t(r#"{"kind":"folder","files":3,"dirs":1,"bytes":2000000}"#), "3 Dateien in 1 Ordner, 2,0 MB");
    assert_eq!(t(r#"{"kind":"folder","files":1,"dirs":2,"bytes":5,"more":true}"#), "mehr als 3 Einträge, mindestens 5 Byte");
    assert_eq!(t(r#"{"kind":"folder","files":20000,"dirs":0,"bytes":0,"more":true}"#), "mehr als 20000 Einträge");
    assert_eq!(t(r#"{"kind":"link"}"#), "eine Verknüpfung");
}

#[test]
fn a_list_is_newest_first_and_keeps_the_chosen_snapshot() {
    let mut r = Rewind::default();
    assert_eq!(r.avail, Avail::Unknown);
    assert!(r.avail.reason().is_some() && !r.avail.ready());
    assert!(r.take_list(&list(&[], "ready")));
    assert_eq!(r.avail, Avail::Empty);
    assert!(r.take_list(&list(&[(3, NOW - 300, "auto", "auto"), (9, NOW - 60, "before-restore", "before-restore"), (5, NOW - 120, "manual", "vor Urlaub")], "ready")));
    assert_eq!(r.snaps.iter().map(|s| s.id).collect::<Vec<_>>(), [9, 5, 3]);
    assert_eq!(r.avail, Avail::Ready(3));
    assert_eq!(r.snaps[0].kind.word(), "vor dem Zurückholen");
    r.pick = 2;
    assert!(!r.take_list(&list(&[(3, NOW - 300, "auto", "auto"), (9, NOW - 60, "before-restore", "before-restore"), (5, NOW - 120, "manual", "vor Urlaub")], "ready")), "the same list changes nothing");
    assert!(r.take_list(&list(&[(3, NOW - 300, "auto", "auto"), (11, NOW, "auto", "auto"), (9, NOW - 60, "before-restore", "before-restore")], "ready")));
    assert_eq!(r.chosen().map(|s| s.id), Some(3), "a new snapshot does not move the pick off the one chosen");
    for (data, a) in [("pending", Avail::Pending), ("never", Avail::Never)] {
        r.take_list(&list(&[], data));
        assert_eq!(r.avail, a);
        assert!(r.avail.reason().is_some() && !r.avail.short().is_empty());
    }
    assert!(r.take_list(&Json::parse(r#"{"ok":false,"error":"failed"}"#).unwrap()));
    assert_eq!(r.avail, Avail::Failed);
}

#[test]
fn only_the_real_request_says_confirmed() {
    let p = Ask::Path { snap: 4, path: "/data/home/1000/a b.txt".into() };
    let dry = Json::parse(&p.line(true).unwrap()).unwrap();
    assert_eq!((dry.get("op").and_then(Json::as_str), dry.get("dry_run").and_then(Json::as_bool), dry.get("confirmed")), (Some("restore-path"), Some(true), None));
    let real = Json::parse(&p.line(false).unwrap()).unwrap();
    assert_eq!((real.get("confirmed").and_then(Json::as_bool), real.get("dry_run")), (Some(true), None));
    assert_eq!(real.get("path").and_then(Json::as_str), Some("/data/home/1000/a b.txt"));
    let a = Json::parse(&Ask::All { snap: 4, at: 1_800_000_000 }.line(false).unwrap()).unwrap();
    assert_eq!((a.get("op").and_then(Json::as_str), a.get("at").and_then(Json::as_u64)), (Some("restore-system"), Some(1_800_000_000)));
    assert_eq!(Ask::Undo.line(true), None, "an undo has no preview");
    assert!(Ask::Undo.line(false).unwrap().contains(r#""op":"undo""#));
    assert_eq!(Ask::Restart.line(false), None, "a restart is init's, not bitssnap's");
}

#[test]
fn the_folders_stay_below_data() {
    assert_eq!(parent("/data/home/1000/Dokumente").as_deref(), Some("/data/home/1000"));
    // Never above the home folder: bitssnap restores nothing of the desktop user's elsewhere.
    assert_eq!((parent("/data/home/1000"), parent("/data/home"), parent("/data/bitsos-access")), (None, None, None));
    assert_eq!((parent("/data"), parent("/data/"), parent("/etc")), (None, None, None));
    assert!(mine("/data/home/1000") && mine("/data/home/1000/") && mine("/data/home/1000/a/b"));
    assert!(!mine("/data/home/10000") && !mine("/data/home") && !mine("/data/bitsos-access"));
    let e = entries(&Json::parse(r#"{"entries":[{"name":"Dokumente","kind":"folder","open":true},{"name":"a.txt","kind":"file","bytes":5,"open":false},{"name":"x","kind":"socket"}]}"#).unwrap());
    assert_eq!(e.iter().map(|e| (e.kind, e.open)).collect::<Vec<_>>(), [(EntryKind::Folder, true), (EntryKind::File, false), (EntryKind::Other, false)]);
    assert_eq!(e[1].bytes, Some(5));
}

#[test]
fn the_dialogs_say_what_changes() {
    let s = Snap { id: 7, created_s: NOW - 600, kind: SnapKind::Auto, label: "auto".into() };
    let j = Json::parse(r#"{"path":"/data/home/1000/brief.txt","then":{"kind":"file","files":1,"bytes":1200},"now":{"kind":"file","files":1,"bytes":1500}}"#).unwrap();
    let l = path_lines(&s, &j, OFF, NOW);
    assert_eq!(l[0], "/data/home/1000/brief.txt");
    assert_eq!(l[1], "Stand von heute um 15:20 (automatisch)");
    assert_eq!((l[2].as_str(), l[3].as_str()), ("Damals: eine Datei, 1,2 kB", "Jetzt: eine Datei, 1,5 kB"));
    let gone = Json::parse(r#"{"path":"/data/x","then":{"kind":"file","bytes":1},"now":null}"#).unwrap();
    assert_eq!(path_lines(&s, &gone, OFF, NOW)[3], "Jetzt: nichts (es kommt neu dazu)");
    let p = Json::parse(r#"{"program":"brave","version":"1.2","now":"1.3","data_then":{"kind":"folder","files":2,"dirs":1,"bytes":10},"data_now":null,"data_path":"/data/appdata/brave"}"#).unwrap();
    let l = program_lines(&s, &p, OFF, NOW);
    assert_eq!(l[0], "Programm „brave“ wie heute um 15:20");
    assert_eq!(l[1], "Damals: Version 1.2 · jetzt: Version 1.3");
    assert!(l[3].ends_with("keiner") && l[4] == "/data/appdata/brave");
    let a = Json::parse(r#"{"snapshot":7,"snapshot_s":1794493200,"pin":1,"running":2,"next":2}"#).unwrap();
    let l = all_lines(&a, OFF);
    assert_eq!(l[0], "Alles wie am Do 12.11.2026 um 15:20");
    assert_eq!(l[1], "Daten: Stand Nr. 7 von Do 12.11.2026 15:20");
    assert_eq!(l[2], "System: Generation 1 (läuft jetzt: Generation 2)");
    assert!(l.iter().any(|x| x.contains("vor dem Zurücksetzen")) && l.iter().any(|x| x.starts_with("Rückgängig")));
    let same = Json::parse(r#"{"snapshot":7,"snapshot_s":1794493200,"running":2}"#).unwrap();
    assert!(all_lines(&same, OFF)[2].starts_with("System: bleibt Generation 2"));
    for code in ["needs_confirm", "not_confirmer", "absent", "late", "busy", "failed", "whatever"] {
        assert!(!failed(code).is_empty());
    }
}
