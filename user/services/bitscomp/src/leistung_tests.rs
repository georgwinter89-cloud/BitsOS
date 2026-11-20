use super::*;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bitscomp-leistung-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("bench")).unwrap();
    d
}

const HISTORY: &str = r#"{"version":1,"nights":[
 {"night":"2026-11-13","generation":149,"origin":"von Hand gebaut","medians":{"boot":12500,"file_read":980}},
 {"night":"2026-11-14","generation":150,"origin":"von Hand gebaut","medians":{"boot":12000,"file_read":1010,"brave_start":2400}}]}"#;

#[test]
fn the_files_become_the_page_data() {
    let root = tmp("load");
    std::fs::write(root.join("bench/history.json"), HISTORY).unwrap();
    std::fs::write(root.join("bench/report.json"), r#"{"night":"2026-11-14","line":"Nacht-Messung: alles im Rahmen","done_ms":5}"#).unwrap();
    std::fs::write(root.join("cachyos.json"), r#"{"night":"2026-11-15","system":"linux","benchmarks":{"boot":{"runs":[9000],"median":9000},"file_read":{"missing":"x"}}}"#).unwrap();
    let d = load(&root);
    assert_eq!(d.report, Some(("2026-11-14".into(), "Nacht-Messung: alles im Rahmen".into(), 5)));
    assert_eq!(d.history.len(), 2);
    assert_eq!(d.cachyos.as_ref().map(|c| c.medians.clone()), Some([("boot".to_string(), 9000.0)].into()));
    assert_eq!(pending(&d), Some(("2026-11-14".into(), "Nacht-Messung: alles im Rahmen".into())));
    std::fs::write(root.join("bench/report.shown"), "2026-11-14\n").unwrap();
    assert_eq!(pending(&load(&root)), None, "shown once");
    assert_eq!(load(&tmp("empty")), Data::default());
}

#[test]
fn rows_values_and_lines() {
    let d = Data {
        report: Some(("2026-11-14".into(), "Nacht-Messung: Hochfahren 12 % langsamer".into(), 1)),
        history: parse_history(HISTORY),
        cachyos: Some(Night { night: "2026-11-15".into(), medians: [("boot".to_string(), 9000.0), ("file_read".to_string(), 1200.0)].into(), ..Night::default() }),
        shown: None,
    };
    let rows = rows();
    assert_eq!(rows.len(), 3 + BENCHES.len());
    assert!(rows.iter().all(|r| is_own(&r.key)));
    assert_eq!(value(&d, REPORT, Some(150)), "Nacht vom 14.11.");
    assert_eq!(sub(&d, REPORT, ""), "Nacht-Messung: Hochfahren 12 % langsamer");
    assert_eq!(value(&d, ORIGIN, Some(150)), "Generation 150");
    assert_eq!(value(&d, CACHYOS, None), "gemessen am 15.11.");
    assert_eq!(sub(&d, CACHYOS, ""), "BitsOS ist bei 0 von 2 Messungen schneller als CachyOS (Linien in den Verläufen).");
    assert_eq!(sub(&d, "leistung.verlauf.boot", ""), "zuletzt 12,0 s · CachyOS 9,0 s · weniger ist besser");
    assert_eq!(sub(&d, "leistung.verlauf.steam_start", ""), "noch keine Messung");
    // A value that rests on fewer runs than the set's says so (a boot time from the starts so far).
    let counted = r#"{"version":1,"nights":[{"night":"2026-11-14","generation":150,"origin":"von Hand gebaut","medians":{"boot":12000,"file_read":1010,"brave_start":2400},"runs":{"boot":1,"file_read":5,"brave_start":3}}]}"#;
    let c = Data { history: parse_history(counted), ..Data::default() };
    assert_eq!(c.history[0].runs.get("boot"), Some(&1));
    assert_eq!(sub(&c, "leistung.verlauf.boot", ""), "zuletzt 12,0 s aus nur 1 Messung · weniger ist besser");
    assert_eq!(sub(&c, "leistung.verlauf.brave_start", ""), "zuletzt 2,4 s aus nur 3 Messungen · weniger ist besser");
    assert_eq!(sub(&c, "leistung.verlauf.file_read", ""), "zuletzt 1010 MB/s · mehr ist besser");
    let (slots, cachy) = chart_parse(&value(&d, "leistung.verlauf.boot", None));
    assert_eq!(slots.len(), NIGHTS);
    assert_eq!(&slots[NIGHTS - 2..], &[Some(12500.0), Some(12000.0)]);
    assert!(slots[..NIGHTS - 2].iter().all(Option::is_none));
    assert_eq!(cachy, Some(9000.0));
    assert_eq!(chart_parse("|"), (vec![None], None));
    let empty = Data::default();
    assert!(sub(&empty, REPORT, "").starts_with("Noch keine Nacht-Messung"));
    assert!(sub(&empty, CACHYOS, "").starts_with("Noch nicht gemessen"));
    assert_eq!(value(&empty, REPORT, None), "");
}

#[test]
fn values_read_in_german() {
    assert_eq!(shown(12_345.0, "ms"), "12,3 s");
    assert_eq!(shown(850.4, "ms"), "850 ms");
    assert_eq!(shown(4.3125, "ms je Bild"), "4,31 ms je Bild");
    assert_eq!(shown(16.66, "ms je Bild"), "16,7 ms je Bild");
    assert_eq!(shown(1234.4, "MB/s"), "1234 MB/s");
    assert_eq!(short("2026-11-04"), "4.11.");
}

#[test]
fn the_page_holds_the_rows_after_the_keys_and_draws_the_charts() {
    use crate::page_schema::{Spec, Ty, model};
    let s = |key: &str, ty: Ty| Spec { key: key.into(), ty, default: Some("false".into()), de: key.into(), names: None, restart: "none".into(), safety: "normal".into() };
    let m = model(&[s("leistung.nachts", Ty::Bool), s("leistung.von", Ty::Int { min: 0, max: 23, unit: "Uhr".into(), word: None }), s("theme.mode", Ty::Choice(vec!["dark".into()]))]);
    let keys: Vec<&str> = m.rows.iter().map(|r| r.key.as_str()).collect();
    let at = keys.iter().position(|k| *k == "leistung.nachts").unwrap();
    assert_eq!(&keys[at..at + 4], &["leistung.nachts", "leistung.von", REPORT, ORIGIN], "the switch first, then the hours");
    assert_eq!(*keys.last().unwrap(), CACHYOS);
    assert!(m.sections.iter().any(|x| matches!(x, crate::page::Section::Rows(n, _) if n == "LEISTUNG")));
    assert!(!model(&[s("theme.mode", Ty::Choice(vec!["dark".into()]))]).rows.iter().any(|r| is_own(&r.key)), "no runner, no rows");
    assert_eq!(crate::page::prefix_of("leistung"), "leistung", "einstellungen/leistung scrolls to them");
}
