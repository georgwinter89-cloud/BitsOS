//! The files under /data/leistung (Georg 13.11.: measurements stay on the device, only there):
//! - `bench/<date>/results.json`: one night, every run of every benchmark, median and spread;
//! - `bench/<date>/profil/`: the profiler's files and its report;
//! - `bench/history.json`: the medians of the last 30 nights (the page's history);
//! - `bench/report.json`: the last morning line;
//! - `cachyos.json`: the same set measured once under CachyOS (the fair comparison).
//! Each file is written whole to a temporary name and renamed. A file that cannot be read counts as missing; a
//! broken history is moved aside (`history.json.bad`) and started anew, never overwritten unread.

use mcpreg::Json;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Nights kept, in the history and as folders.
pub const KEEP: usize = 30;
/// Bound of any file read here (R1: per input).
pub const MAX_FILE: u64 = 4 << 20;

/// One benchmark of one night.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Runs(Vec<f64>),
    /// Why it has no value tonight, in plain German (the program is missing, it did not start, ...).
    Missing(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Night {
    pub night: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub generation: Option<u64>,
    pub origin: String,
    /// "bitsos" or "linux" (the CachyOS run).
    pub system: String,
    pub benches: BTreeMap<String, Outcome>,
    /// The profiler's outcome: Ok(report file relative to the night's folder) or why there is none.
    pub profile: Result<String, String>,
}

pub fn bench_dir(root: &Path) -> PathBuf {
    root.join("bench")
}

pub fn read_json(p: &Path) -> Result<Option<Json>, String> {
    let f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    let mut v = Vec::new();
    f.take(MAX_FILE + 1).read_to_end(&mut v).map_err(|e| format!("{}: {e}", p.display()))?;
    if v.len() as u64 > MAX_FILE {
        return Err(format!("{}: larger than {} KiB", p.display(), MAX_FILE >> 10));
    }
    let t = String::from_utf8(v).map_err(|_| format!("{}: not UTF-8", p.display()))?;
    Json::parse(&t).map(Some).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn write_json(p: &Path, j: &Json) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = p.with_extension("json.new");
    std::fs::write(&tmp, format!("{j}\n")).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| format!("{}: {e}", p.display()))
}

fn num(x: f64) -> Json {
    Json::Num(crate::stats::round3(x))
}

impl Night {
    pub fn medians(&self) -> BTreeMap<String, f64> {
        self.benches
            .iter()
            .filter_map(|(k, o)| match o {
                Outcome::Runs(r) => crate::stats::median(r).map(|m| (k.clone(), m)),
                Outcome::Missing(_) => None,
            })
            .collect()
    }

    pub fn run_counts(&self) -> BTreeMap<String, usize> {
        self.benches
            .iter()
            .filter_map(|(k, o)| match o {
                Outcome::Runs(r) if !r.is_empty() => Some((k.clone(), r.len())),
                _ => None,
            })
            .collect()
    }

    /// The benchmarks with fewer runs than `min` (or than `want`, the set's count, if that is less): kept, not compared.
    pub fn few(&self, want: usize) -> std::collections::BTreeSet<String> {
        let min = crate::report::MIN_RUNS.min(want.max(1));
        self.run_counts().into_iter().filter(|(_, n)| *n < min).map(|(k, _)| k).collect()
    }

    pub fn to_json(&self) -> Json {
        let benches = self
            .benches
            .iter()
            .map(|(k, o)| {
                let unit = crate::catalog::get(k).map_or("", |b| b.unit);
                let v = match o {
                    Outcome::Runs(r) => Json::obj(vec![
                        ("runs", Json::Arr(r.iter().map(|x| num(*x)).collect())),
                        ("median", crate::stats::median(r).map_or(Json::Null, num)),
                        ("spread", num(crate::stats::spread(r))),
                        ("unit", Json::str(unit)),
                    ]),
                    Outcome::Missing(why) => Json::obj(vec![("missing", Json::str(why)), ("unit", Json::str(unit))]),
                };
                (k.clone(), v)
            })
            .collect();
        let profile = match &self.profile {
            Ok(f) => Json::obj(vec![("report", Json::str(f))]),
            Err(why) => Json::obj(vec![("missing", Json::str(why))]),
        };
        Json::obj(vec![
            ("version", Json::Num(1.0)),
            ("night", Json::str(&self.night)),
            ("started_ms", Json::Num(self.started_ms as f64)),
            ("ended_ms", Json::Num(self.ended_ms as f64)),
            ("generation", self.generation.map_or(Json::Null, |g| Json::Num(g as f64))),
            ("origin", Json::str(&self.origin)),
            ("system", Json::str(&self.system)),
            ("benchmarks", Json::Obj(benches)),
            ("profile", profile),
        ])
    }

    pub fn from_json(j: &Json) -> Option<Night> {
        let mut benches = BTreeMap::new();
        if let Some(Json::Obj(b)) = j.get("benchmarks") {
            for (k, v) in b {
                let o = match (v.get("runs").and_then(Json::as_array), v.get("missing").and_then(Json::as_str)) {
                    (Some(r), _) => Outcome::Runs(r.iter().filter_map(Json::as_f64).collect()),
                    (None, Some(w)) => Outcome::Missing(w.to_string()),
                    _ => continue,
                };
                benches.insert(k.clone(), o);
            }
        }
        let profile = match j.get("profile") {
            Some(p) => match (p.get("report").and_then(Json::as_str), p.get("missing").and_then(Json::as_str)) {
                (Some(r), _) => Ok(r.to_string()),
                (None, Some(m)) => Err(m.to_string()),
                _ => Err(String::new()),
            },
            None => Err(String::new()),
        };
        Some(Night {
            night: j.get("night")?.as_str()?.to_string(),
            started_ms: j.get("started_ms").and_then(Json::as_u64).unwrap_or(0),
            ended_ms: j.get("ended_ms").and_then(Json::as_u64).unwrap_or(0),
            generation: j.get("generation").and_then(Json::as_u64),
            origin: j.get("origin").and_then(Json::as_str).unwrap_or("").to_string(),
            system: j.get("system").and_then(Json::as_str).unwrap_or("bitsos").to_string(),
            benches,
            profile,
        })
    }
}

/// One night of the history: its date, generation, origin, medians and how many runs stand behind each median
/// (the page says when a value rests on fewer than the set's five).
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub night: String,
    pub generation: Option<u64>,
    pub origin: String,
    pub medians: BTreeMap<String, f64>,
    pub runs: BTreeMap<String, usize>,
}

impl Entry {
    pub fn of(n: &Night) -> Entry {
        Entry { night: n.night.clone(), generation: n.generation, origin: n.origin.clone(), medians: n.medians(), runs: n.run_counts() }
    }
}

pub fn history_to_json(h: &[Entry]) -> Json {
    let nights = h
        .iter()
        .map(|e| {
            Json::obj(vec![
                ("night", Json::str(&e.night)),
                ("generation", e.generation.map_or(Json::Null, |g| Json::Num(g as f64))),
                ("origin", Json::str(&e.origin)),
                ("medians", Json::Obj(e.medians.iter().map(|(k, v)| (k.clone(), num(*v))).collect())),
                ("runs", Json::Obj(e.runs.iter().map(|(k, n)| (k.clone(), Json::Num(*n as f64))).collect())),
            ])
        })
        .collect();
    Json::obj(vec![("version", Json::Num(1.0)), ("nights", Json::Arr(nights))])
}

pub fn history_of_json(j: &Json) -> Vec<Entry> {
    let Some(a) = j.get("nights").and_then(Json::as_array) else { return Vec::new() };
    a.iter()
        .filter_map(|n| {
            let medians = match n.get("medians") {
                Some(Json::Obj(m)) => m.iter().filter_map(|(k, v)| v.as_f64().map(|x| (k.clone(), x))).collect(),
                _ => BTreeMap::new(),
            };
            // Older histories have no counts.
            let runs = match n.get("runs") {
                Some(Json::Obj(m)) => m.iter().filter_map(|(k, v)| v.as_u64().map(|x| (k.clone(), x as usize))).collect(),
                _ => BTreeMap::new(),
            };
            Some(Entry {
                night: n.get("night")?.as_str()?.to_string(),
                generation: n.get("generation").and_then(Json::as_u64),
                origin: n.get("origin").and_then(Json::as_str).unwrap_or("").to_string(),
                medians,
                runs,
            })
        })
        .collect()
}

/// The history with `e` in its place (a night measured again replaces itself), sorted, the last KEEP nights.
pub fn history_with(mut h: Vec<Entry>, e: Entry) -> Vec<Entry> {
    h.retain(|x| x.night != e.night);
    h.push(e);
    h.sort_by(|a, b| a.night.cmp(&b.night));
    let cut = h.len().saturating_sub(KEEP);
    h.drain(..cut);
    h
}

pub fn load_history(root: &Path) -> Vec<Entry> {
    let p = bench_dir(root).join("history.json");
    match read_json(&p) {
        Ok(Some(j)) => history_of_json(&j),
        Ok(None) => Vec::new(),
        Err(e) => {
            let aside = p.with_extension("json.bad");
            let moved = std::fs::rename(&p, &aside).is_ok();
            crate::events::event("history.unreadable", &format!("why={} kept_as={}", crate::events::q(&e), if moved { aside.display().to_string() } else { "-".into() }));
            Vec::new()
        }
    }
}

/// Night folders (`YYYY-MM-DD`) beyond the newest KEEP are removed, nothing else in bench/.
pub fn prune(root: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(bench_dir(root)) else { return Vec::new() };
    let mut nights: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_night_name(n))
        .collect();
    nights.sort();
    let cut = nights.len().saturating_sub(KEEP);
    let gone: Vec<String> = nights.drain(..cut).collect();
    for n in &gone {
        let _ = std::fs::remove_dir_all(bench_dir(root).join(n));
    }
    gone
}

pub fn is_night_name(n: &str) -> bool {
    let b = n.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// The morning line and when it was made.
pub fn report_json(night: &str, line: &str, done_ms: u64) -> Json {
    Json::obj(vec![("night", Json::str(night)), ("line", Json::str(line)), ("done_ms", Json::Num(done_ms as f64))])
}
