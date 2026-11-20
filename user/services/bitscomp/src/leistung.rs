//! SELFOPT-1: what the settings page "Leistung" shows besides its three keys, from bitsbench's files under
//! /data/leistung (`BITSCOMP_LEISTUNG_DIR` for tests): the last morning line (`bench/report.json`), the medians of
//! the last 30 nights (`bench/history.json`), the CachyOS comparison (`cachyos.json`), and the running
//! generation's origin. The rows are bitscomp's own (keys settingsd never has, like the programs' link row);
//! their values travel as text, so the page's model stays fixed while the data changes. The benchmark ids, names
//! and units are bitsbench's (user/services/bitsbench/src/catalog.rs): both lists change together.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use mcpreg::Json;

use crate::page::{Kind, Row};

pub const DIR: &str = "/data/leistung";
pub const PREFIX: &str = "leistung.";
pub const REPORT: &str = "leistung.bericht";
pub const ORIGIN: &str = "leistung.herkunft";
pub const CACHYOS: &str = "leistung.cachyos";
pub const CHART: &str = "leistung.verlauf.";
/// Nights a chart shows (bitsbench keeps as many).
pub const NIGHTS: usize = 30;
/// Runs a night's value normally rests on (bitsbench's set); fewer are said under the chart.
pub const RUNS: usize = 5;
const MAX_FILE: u64 = 4 << 20;

/// (id, name, unit, lower is better).
pub const BENCHES: [(&str, &str, &str, bool); 11] = [
    ("boot", "Hochfahren", "ms", true),
    ("brave_start", "Brave starten", "ms", true),
    ("brave_load", "Seite laden", "ms", true),
    ("brave_scroll", "Scrollen in Brave", "ms je Bild", true),
    ("desktop_frame", "Bild der Arbeitsfläche", "ms je Bild", true),
    ("game_frame", "Spiel (vkQuake)", "ms je Bild", true),
    ("farthest_frontier", "Farthest Frontier (von Hand gemessen)", "ms je Bild", true),
    ("steam_start", "Steam starten", "ms", true),
    ("file_read", "Dateien lesen", "MB/s", false),
    ("file_mix", "Dateien schreiben und lesen", "ms", true),
    ("memory_load", "Speicher unter Last", "ms", true),
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Night {
    pub night: String,
    pub generation: Option<u64>,
    pub origin: String,
    pub medians: BTreeMap<String, f64>,
    /// How many runs stand behind each median (histories before the counts have none).
    pub runs: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Data {
    /// (night, line, done_ms) of report.json.
    pub report: Option<(String, String, u64)>,
    pub history: Vec<Night>,
    pub cachyos: Option<Night>,
    /// The night whose line was shown already (`bench/report.shown`).
    pub shown: Option<String>,
}

pub fn dir() -> PathBuf {
    PathBuf::from(DIR)
}

fn read(p: &Path) -> Option<String> {
    let mut v = Vec::new();
    std::fs::File::open(p).ok()?.take(MAX_FILE).read_to_end(&mut v).ok()?;
    String::from_utf8(v).ok()
}

fn medians(j: Option<&Json>) -> BTreeMap<String, f64> {
    match j {
        Some(Json::Obj(m)) => m.iter().filter_map(|(k, v)| v.as_f64().filter(|x| x.is_finite()).map(|x| (k.clone(), x))).collect(),
        _ => BTreeMap::new(),
    }
}

/// A results.json's medians (the CachyOS run): each benchmark's `median`.
fn result_medians(j: &Json) -> BTreeMap<String, f64> {
    match j.get("benchmarks") {
        Some(Json::Obj(m)) => m.iter().filter_map(|(k, v)| v.get("median").and_then(Json::as_f64).map(|x| (k.clone(), x))).collect(),
        _ => BTreeMap::new(),
    }
}

pub fn parse_history(text: &str) -> Vec<Night> {
    let Ok(j) = Json::parse(text) else { return Vec::new() };
    let Some(a) = j.get("nights").and_then(Json::as_array) else { return Vec::new() };
    let mut v: Vec<Night> = a
        .iter()
        .filter_map(|n| {
            Some(Night {
                night: n.get("night")?.as_str()?.to_string(),
                generation: n.get("generation").and_then(Json::as_u64),
                origin: n.get("origin").and_then(Json::as_str).unwrap_or("").to_string(),
                medians: medians(n.get("medians")),
                runs: medians(n.get("runs")).into_iter().map(|(k, v)| (k, v as usize)).collect(),
            })
        })
        .collect();
    v.sort_by(|a, b| a.night.cmp(&b.night));
    let cut = v.len().saturating_sub(NIGHTS);
    v.drain(..cut);
    v
}

/// Everything from the folder (missing or broken files leave their part empty).
pub fn load(root: &Path) -> Data {
    let bench = root.join("bench");
    let report = read(&bench.join("report.json")).and_then(|t| Json::parse(&t).ok()).and_then(|j| {
        Some((j.get("night")?.as_str()?.to_string(), j.get("line")?.as_str()?.to_string(), j.get("done_ms").and_then(Json::as_u64).unwrap_or(0)))
    });
    let cachyos = read(&root.join("cachyos.json")).and_then(|t| Json::parse(&t).ok()).and_then(|j| {
        Some(Night { night: j.get("night")?.as_str()?.to_string(), medians: result_medians(&j), ..Night::default() })
    });
    Data {
        report,
        history: read(&bench.join("history.json")).map(|t| parse_history(&t)).unwrap_or_default(),
        cachyos,
        shown: read(&bench.join("report.shown")).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
    }
}

/// `2026-11-14` -> `14.11.`
pub fn short(date: &str) -> String {
    match date.split('-').collect::<Vec<_>>().as_slice() {
        [_, m, d] => format!("{}.{}.", d.trim_start_matches('0'), m.trim_start_matches('0')),
        _ => date.to_string(),
    }
}

/// A value with its unit, German style: 12,3 s from ms above a second, 850 ms, 4,31 ms je Bild, 1234 MB/s.
pub fn shown(v: f64, unit: &str) -> String {
    let comma = |s: String| s.replace('.', ",");
    match unit {
        "ms" if v >= 1000.0 => comma(format!("{:.1} s", v / 1000.0)),
        "ms" => format!("{} ms", v.round() as i64),
        "MB/s" => format!("{} MB/s", v.round() as i64),
        u if v < 10.0 => comma(format!("{v:.2} {u}")),
        u => comma(format!("{v:.1} {u}")),
    }
}

/// The rows the page adds after the `leistung.*` keys: the report, the origin, one chart per benchmark, CachyOS.
pub fn rows() -> Vec<Row> {
    let fixed = |key: &str, label: &str| Row {
        key: key.into(),
        kind: Kind::Fixed(String::new()),
        default: String::new(),
        label: label.into(),
        short: label.into(),
        note: String::new(),
        confirm: false,
        stack: false,
    };
    let mut v = vec![fixed(REPORT, "Letzter Bericht"), fixed(ORIGIN, "Diese Generation")];
    for (id, name, _, _) in BENCHES {
        v.push(Row { kind: Kind::Chart, ..fixed(&format!("{CHART}{id}"), name) });
    }
    v.push(fixed(CACHYOS, "Vergleich mit CachyOS"));
    v
}

pub fn is_own(key: &str) -> bool {
    key == REPORT || key == ORIGIN || key == CACHYOS || key.starts_with(CHART)
}

/// A chart's value as text: the last NIGHTS nights' values (empty where a night has none), then `|` and the
/// CachyOS value (empty without one). The page draws from it (`page_draw::chart`).
pub fn chart_value(d: &Data, id: &str) -> String {
    let pad = NIGHTS.saturating_sub(d.history.len());
    let mut parts: Vec<String> = vec![String::new(); pad];
    parts.extend(d.history.iter().map(|n| n.medians.get(id).map_or(String::new(), |v| format!("{v}"))));
    let c = d.cachyos.as_ref().and_then(|c| c.medians.get(id)).map_or(String::new(), |v| format!("{v}"));
    format!("{}|{c}", parts.join(";"))
}

/// The chart's values back: (one slot per night, the CachyOS value).
pub fn chart_parse(v: &str) -> (Vec<Option<f64>>, Option<f64>) {
    let (a, c) = v.split_once('|').unwrap_or((v, ""));
    let slots = a.split(';').map(|x| x.parse::<f64>().ok().filter(|x| x.is_finite())).collect();
    (slots, c.parse().ok().filter(|x: &f64| x.is_finite()))
}

/// What a row of the page shows on the right (its value).
pub fn value(d: &Data, key: &str, generation: Option<u32>) -> String {
    match key {
        REPORT => d.report.as_ref().map_or(String::new(), |(n, _, _)| format!("Nacht vom {}", short(n))),
        ORIGIN => generation.map_or("Generation unbekannt".into(), |g| format!("Generation {g}")),
        CACHYOS => d.cachyos.as_ref().map_or(String::new(), |c| format!("gemessen am {}", short(&c.night))),
        k => k.strip_prefix(CHART).map(|id| chart_value(d, id)).unwrap_or_default(),
    }
}

/// " aus nur 2 Messungen" when a value rests on fewer runs than the set's (a boot time from the starts so far).
fn few(runs: Option<usize>) -> String {
    match runs {
        Some(1) => " aus nur 1 Messung".into(),
        Some(n) if n < RUNS => format!(" aus nur {n} Messungen"),
        _ => String::new(),
    }
}

/// The line under a row's label (instead of its address).
pub fn sub(d: &Data, key: &str, origin: &str) -> String {
    match key {
        REPORT => d.report.as_ref().map_or("Noch keine Nacht-Messung. Sie läuft nur, wenn „Nachts messen“ an ist.".into(), |(_, l, _)| l.clone()),
        ORIGIN => origin.to_string(),
        CACHYOS => match &d.cachyos {
            None => "Noch nicht gemessen: einmal vom CachyOS-Stick aus, wie in der Anleitung beschrieben.".into(),
            Some(c) => compare(d, c),
        },
        k => {
            let Some((_, _, unit, lower)) = k.strip_prefix(CHART).and_then(|id| BENCHES.iter().find(|b| b.0 == id)) else { return String::new() };
            let id = &k[CHART.len()..];
            let last = d.history.iter().rev().find_map(|n| n.medians.get(id).map(|v| (*v, n.runs.get(id).copied())));
            let way = if *lower { "weniger ist besser" } else { "mehr ist besser" };
            let cachy = d.cachyos.as_ref().and_then(|c| c.medians.get(id)).map(|v| format!(" · CachyOS {}", shown(*v, unit))).unwrap_or_default();
            match last {
                Some((v, runs)) => format!("zuletzt {}{}{cachy} · {way}", shown(v, unit), few(runs)),
                None => format!("noch keine Messung{cachy}"),
            }
        }
    }
}

/// "BitsOS ist bei 3 von 8 Messungen schneller als CachyOS": the last night's medians against CachyOS's.
pub fn compare(d: &Data, c: &Night) -> String {
    let Some(last) = d.history.last() else { return "Für BitsOS fehlt noch eine Nacht-Messung zum Vergleichen.".into() };
    let (mut faster, mut both) = (0, 0);
    for (id, _, _, lower) in BENCHES {
        if let (Some(b), Some(x)) = (last.medians.get(id), c.medians.get(id)) {
            both += 1;
            if (b < x) == lower && b != x {
                faster += 1;
            }
        }
    }
    if both == 0 {
        return "Keine Messung gibt es auf beiden Systemen.".into();
    }
    format!("BitsOS ist bei {faster} von {both} Messungen schneller als CachyOS (Linien in den Verläufen).")
}

/// The night whose line waits for Georg: made, and not shown yet.
pub fn pending(d: &Data) -> Option<(String, String)> {
    let (n, l, _) = d.report.as_ref()?;
    (d.shown.as_deref() != Some(n.as_str())).then(|| (n.clone(), l.clone()))
}

/// Notes that night `n`'s line was shown (on a thread of its own: /data may be slow).
pub fn mark_shown(root: PathBuf, n: String) {
    let _ = std::thread::Builder::new().name("bitscomp-leistung-shown".into()).spawn(move || {
        let p = root.join("bench").join("report.shown");
        if let Err(e) = std::fs::write(&p, format!("{n}\n")) {
            eprintln!("bitscomp: leistung: {} not written: {e} (the line may show once more)", p.display());
        }
    });
}

/// The kernel's generation line.
fn generation_file() -> PathBuf {
    PathBuf::from("/proc/bitsos/generation")
}

/// The running generation (`generation=<n>` of /proc/bitsos/generation), read once: it does not change while
/// the system runs.
pub fn generation() -> Option<u32> {
    static GEN: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *GEN.get_or_init(|| {
        let t = std::fs::read_to_string(generation_file()).ok()?;
        t.split_whitespace().find_map(|w| w.strip_prefix("generation="))?.parse().ok().filter(|n| *n > 0)
    })
}

/// The running generation's origin (`origin=` on /proc/bitsos/generation; level 1 builds nothing itself).
pub fn origin() -> String {
    let t = std::fs::read_to_string(generation_file()).unwrap_or_default();
    match t.split_whitespace().find_map(|w| w.strip_prefix("origin=")) {
        Some(o) if !o.is_empty() => o.replace('_', " "),
        _ => "von Hand gebaut".into(),
    }
}

#[cfg(test)]
#[path = "leistung_tests.rs"]
mod tests;
