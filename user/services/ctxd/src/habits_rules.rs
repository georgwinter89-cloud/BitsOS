//! DB4: what the habit records mean (DECISIONS 24.10.: counting, thresholds, decay, time-of-day frecency; AI
//! never decides). Small things (a window's size, a tile's width) are imitated once they held three times in a
//! row; new things (two programs as one button) are only proposed; times, sound and picks are learned and rank
//! "Gerade passend". Every item says why, in plain German with the counts it was decided on.

use crate::habits::Rec;
use std::collections::{BTreeMap, HashMap};

/// Habits fade with this half-life (they change faster than which files belong together).
pub const HALF_LIFE_DAYS: f64 = 30.0;
/// The same state this many times in a row before it is imitated.
pub const IN_A_ROW: usize = 3;
/// ... and at least this share of the (decayed) closes.
pub const SHARE: f64 = 0.75;
/// The closes a window rule looks at.
pub const LAST: usize = 10;
/// A tile width counts when it differs this much from the plain one (1.0) and the last three agree this well.
pub const WIDTH_OFF: f64 = 0.15;
pub const WIDTH_AGREE: f64 = 0.10;
/// Programs started together: at least this often, and in this share of the rarer one's starts.
pub const TOGETHER_MIN: u32 = 3;
pub const TOGETHER_SHARE: f64 = 0.6;
/// A time of day: at least this many starts, this share of them in one window of `HOURS` hours.
pub const TIME_MIN: u32 = 5;
pub const TIME_SHARE: f64 = 0.6;
pub const HOURS: u8 = 4;
/// Sound and picks: learned from this many.
pub const SOUND_MIN: usize = 3;
pub const PICK_MIN: u32 = 3;
/// Sound changes of one program closer than this are one change (a slider dragged).
pub const SOUND_MERGE_S: i64 = 30;
/// "Gerade passend" holds at most this many entries.
pub const SUGGEST_MAX: usize = 4;
/// A rule holds while the decayed weight of what it counts is at least this share of its threshold: three
/// maximised closes of a year ago are no habit any more, and an old pair is no longer proposed.
pub const FADED: f64 = 0.5;
/// The counts in a reason are of this recent time (two half-lives), so they say what the decision weighed.
pub const FRESH_DAYS: i64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub enum Apply {
    State(String),
    Width(f64),
    /// VORRANG: the program's priority at its start; `ask`: only proposed, Georg says yes first (hoch).
    Vorrang { value: String, ask: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub part: String,
    pub app: String,
    pub title: String,
    pub reason: String,
    /// nachgemacht (imitated), vorgeschlagen (proposed), gelernt (learned, ranks suggestions), abgelehnt (Georg
    /// chose Rückgängig: not imitated again until he deletes it).
    pub state: &'static str,
    pub count: u32,
    pub apply: Option<Apply>,
    /// The one-time hint was shown already.
    pub hinted: bool,
    pub apps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub kind: &'static str,
    pub app: Option<String>,
    pub apps: Vec<String>,
    pub action: Option<String>,
    pub name: String,
    pub reason: String,
    pub score: f64,
}

pub fn weight(ts: i64, now: i64) -> f64 {
    2f64.powf(-((now - ts).max(0) as f64) / 86_400.0 / HALF_LIFE_DAYS)
}

fn mass<'a>(rs: impl IntoIterator<Item = &'a Rec>, now: i64) -> f64 {
    rs.into_iter().map(|r| weight(r.ts, now)).sum()
}

fn fresh(r: &Rec, now: i64) -> bool {
    now - r.ts <= FRESH_DAYS * 86_400
}

fn hour_of(r: &Rec) -> u8 {
    r.h.unwrap_or(((r.ts.rem_euclid(86_400)) / 3600) as u8)
}

/// The newest German name the desktop gave `key`, else the key itself.
fn names(recs: &[Rec]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for r in recs {
        if let Some(n) = &r.name {
            m.insert(r.key.clone(), n.clone());
        }
    }
    m
}

fn mal(n: u32) -> String {
    if n == 1 { "1 Mal".into() } else { format!("{n} Mal") }
}

/// Records grouped by item id, oldest first.
fn groups(recs: &[Rec]) -> BTreeMap<String, Vec<&Rec>> {
    let mut g: BTreeMap<String, Vec<&Rec>> = BTreeMap::new();
    for r in recs {
        g.entry(r.item()).or_default().push(r);
    }
    for v in g.values_mut() {
        v.sort_by_key(|r| r.ts);
    }
    g
}

/// The best window of `HOURS` hours of `hours`: (first hour, count in it).
pub fn best_window(hours: &[u8]) -> (u8, u32) {
    let mut best = (0u8, 0u32);
    for start in 0..24u8 {
        let n = hours.iter().filter(|h| (24 + **h as i32 - start as i32) % 24 < HOURS as i32).count() as u32;
        if n > best.1 {
            best = (start, n);
        }
    }
    best
}

/// Every learned item, newest knowledge first within its part.
pub fn items(recs: &[Rec], now: i64) -> Vec<Item> {
    let g = groups(recs);
    let nm = names(recs);
    let name = |k: &str| nm.get(k).cloned().unwrap_or_else(|| k.to_string());
    let marks = |id: &str, part: &str| g.get(id).map(|v| v.iter().filter(|r| r.part == part).count()).unwrap_or(0);
    let mut out = Vec::new();
    let mut starts: HashMap<&str, u32> = HashMap::new();
    for r in recs.iter().filter(|r| r.part == "zeiten") {
        *starts.entry(r.key.as_str()).or_default() += 1;
    }
    for (id, v) in &g {
        let own: Vec<&&Rec> = v.iter().filter(|r| r.id() == *id).collect();
        let Some(first) = own.first() else { continue };
        let (part, key) = (first.part.as_str(), first.key.as_str());
        let declined = marks(id, "undo") > 0;
        let hinted = marks(id, "hint") > 0;
        let item = |title: String, reason: String, state: &'static str, count: u32, apply: Option<Apply>| Item {
            id: id.clone(),
            part: part.into(),
            app: key.into(),
            title,
            reason,
            state: if declined && state == "nachgemacht" { "abgelehnt" } else { state },
            count,
            apply: if declined { None } else { apply },
            hinted,
            apps: Vec::new(),
        };
        if part == crate::habits_vorrang::PART {
            let own: Vec<&Rec> = own.iter().map(|r| **r).collect();
            if let Some(l) = crate::habits_vorrang::learned(&own, v, now, &name(key)) {
                out.push(Item { id: id.clone(), part: part.into(), app: key.into(), title: l.title, reason: l.reason, state: l.state, count: l.count, apply: l.apply, hinted: l.hinted, apps: Vec::new() });
            }
            continue;
        }
        // Rückgängig: shown (and deletable) whatever the records say now; never imitated until deleted.
        if declined && matches!(part, "fenster" | "breite") {
            let n = name(key);
            let what = if part == "fenster" { "Fenstergröße" } else { "Kachelbreite" };
            out.push(item(format!("{n}: {what} wird nicht mehr nachgemacht"), "weil du beim Nachmachen „Rückgängig“ gewählt hast".into(), "abgelehnt", own.len() as u32, None));
            continue;
        }
        match part {
            "fenster" => {
                let last: Vec<&&Rec> = own.iter().rev().take(LAST).copied().collect();
                let s = last[0].val.clone().unwrap_or_default();
                let row = last.len() >= IN_A_ROW && last[..IN_A_ROW].iter().all(|r| r.val.as_deref() == Some(s.as_str()));
                let total: f64 = own.iter().map(|r| weight(r.ts, now)).sum();
                let same: f64 = own.iter().filter(|r| r.val.as_deref() == Some(s.as_str())).map(|r| weight(r.ts, now)).sum();
                let held = row && mass(last[..IN_A_ROW].iter().map(|r| **r), now) >= IN_A_ROW as f64 * FADED;
                let recent: Vec<&&Rec> = last.iter().copied().filter(|r| fresh(r, now)).collect();
                let k = recent.iter().filter(|r| r.val.as_deref() == Some(s.as_str())).count() as u32;
                let (how, done) = match s.as_str() {
                    "maximised" => ("maximiert", "maximiert hast"),
                    "fullscreen" => ("im Vollbild", "im Vollbild benutzt hast"),
                    _ => continue,
                };
                if held && total > 0.0 && same / total >= SHARE {
                    let n = name(key);
                    out.push(item(format!("{n} startet {how}"), format!("weil du {n} {k} von {} Mal {done}", recent.len()), "nachgemacht", k, Some(Apply::State(s))));
                }
            }
            "breite" => {
                let w: Vec<f64> = own.iter().rev().take(IN_A_ROW).filter_map(|r| r.num).collect();
                if w.len() < IN_A_ROW || mass(own.iter().rev().take(IN_A_ROW).map(|r| **r), now) < IN_A_ROW as f64 * FADED {
                    continue;
                }
                let mut sorted = w.clone();
                sorted.sort_by(f64::total_cmp);
                let med = sorted[sorted.len() / 2];
                let agree = w.iter().all(|x| (x - med).abs() <= WIDTH_AGREE * med);
                if agree && (med - 1.0).abs() >= WIDTH_OFF {
                    let n = name(key);
                    let how = if med > 1.0 { "breitere" } else { "schmalere" };
                    let k = own.len() as u32;
                    out.push(item(format!("{n} bekommt eine {how} Kachel"), format!("weil du die Kachel von {n} {} so breit gezogen hast", mal(k)), "nachgemacht", k, Some(Apply::Width(med))));
                }
            }
            "zusammen" => {
                let c = own.len() as u32;
                let apps: Vec<String> = key.split('+').map(str::to_string).collect();
                let rarer = apps.iter().map(|a| starts.get(a.as_str()).copied().unwrap_or(0)).min().unwrap_or(0);
                let den = rarer.max(c);
                let held = mass(own.iter().map(|r| **r), now) >= TOGETHER_MIN as f64 * FADED;
                if held && c >= TOGETHER_MIN && c as f64 / den as f64 >= TOGETHER_SHARE && apps.len() == 2 {
                    let (a, b) = (name(&apps[0]), name(&apps[1]));
                    let mut it = item(format!("{a} und {b} zusammen starten"), format!("weil du {a} und {b} {c} von {den} Mal zusammen startest"), "vorgeschlagen", c, None);
                    it.apps = apps;
                    out.push(it);
                }
            }
            "zeiten" | "sofa" => {
                let hours: Vec<u8> = own.iter().map(|r| hour_of(r)).collect();
                let n = hours.len() as u32;
                let (h0, k) = best_window(&hours);
                let min = if part == "sofa" { 3 } else { TIME_MIN };
                if n >= min && k as f64 / n as f64 >= TIME_SHARE && mass(own.iter().map(|r| **r), now) >= min as f64 * FADED {
                    let h1 = (h0 + HOURS) % 24;
                    let who = if part == "sofa" { "den Sofa-Modus".to_string() } else { name(key) };
                    let head = if part == "sofa" { "Sofa-Modus".to_string() } else { name(key) };
                    out.push(item(format!("{head} meist zwischen {h0} und {h1} Uhr"), format!("weil du {who} {k} von {n} Mal in dieser Zeit gestartet hast"), "gelernt", n, None));
                }
            }
            "ton" => {
                let merged = merge_sound(&own);
                if merged.len() < SOUND_MIN || mass(merged.iter().copied(), now) < SOUND_MIN as f64 * FADED {
                    continue;
                }
                let last = &merged[merged.len() - SOUND_MIN..];
                let out_same = last.iter().all(|r| r.val == last[0].val);
                let vols: Vec<f64> = last.iter().filter_map(|r| r.num).collect();
                let vol_same = vols.len() == last.len() && vols.iter().all(|v| (v - vols[0]).abs() <= 10.0);
                if out_same && vol_same {
                    let n = name(key);
                    let what = match (&last[0].val, vols.last()) {
                        (Some(o), Some(v)) => format!("Ton über {o} mit {v:.0} %"),
                        (Some(o), None) => format!("Ton über {o}"),
                        (None, Some(v)) => format!("Lautstärke {v:.0} %"),
                        (None, None) => continue,
                    };
                    let k = merged.len() as u32;
                    out.push(item(format!("{n}: {what}"), format!("weil du bei {n} {} diesen Ton eingestellt hast", mal(k)), "gelernt", k, None));
                }
            }
            "auswahl" => {
                let k = own.len() as u32;
                if k >= PICK_MIN && mass(own.iter().map(|r| **r), now) >= PICK_MIN as f64 * FADED {
                    let n = name(key);
                    let via = match own.last().and_then(|r| r.val.as_deref()) {
                        Some("library") => "in der Programmübersicht",
                        Some("sofa") => "im Sofa-Modus",
                        Some("passend") => "unter „Gerade passend“",
                        _ => "in der Adresszeile",
                    };
                    out.push(item(format!("Oft gewählt: {n}"), format!("weil du es {} {via} gewählt hast", mal(k)), "gelernt", k, None));
                }
            }
            _ => {}
        }
    }
    out
}

/// Sound records of one program with the changes of one slider drag merged (the last one counts).
fn merge_sound<'a>(own: &[&&'a Rec]) -> Vec<&'a Rec> {
    let mut out: Vec<&Rec> = Vec::new();
    for r in own {
        if out.last().is_some_and(|l| r.ts - l.ts <= SOUND_MERGE_S) {
            out.pop();
        }
        out.push(r);
    }
    out
}

/// Frecency of picks per target (the address bar ranks by it); only targets the page lists (`auswahl` items),
/// so every rank has an entry Georg can delete.
pub fn ranks(recs: &[Rec], now: i64) -> BTreeMap<String, f64> {
    let listed: std::collections::HashSet<String> = items(recs, now).into_iter().filter(|i| i.part == "auswahl").map(|i| i.id).collect();
    let mut m = BTreeMap::new();
    for r in recs.iter().filter(|r| r.part == "auswahl" && listed.contains(&r.id())) {
        *m.entry(r.key.clone()).or_default() += weight(r.ts, now);
    }
    m
}

/// "9 von 10 Mal zusammen" out of an item's reason.
fn together_counts(reason: &str) -> String {
    reason.split_once(" Mal ").map_or_else(String::new, |(head, _)| {
        let n: Vec<&str> = head.rsplit(' ').take(3).collect();
        format!("{} {} {} Mal zusammen", n.get(2).unwrap_or(&""), n.get(1).unwrap_or(&""), n.first().unwrap_or(&""))
    })
}

/// How near `h` is to `hour` (circular): the same hour counts fully, a neighbour half.
fn near(h: u8, hour: u8) -> f64 {
    match (24 + h as i32 - hour as i32) % 24 {
        0 => 1.0,
        1 | 23 => 0.5,
        _ => 0.0,
    }
}

/// C4 `suggest`: what fits now (`hour`, the programs `open`), best first, each with its reason. Programs
/// already open are not offered; a declined item never comes back here. Only what the page lists feeds it, so the
/// reason of every suggestion has an entry Georg can delete.
pub fn suggest(recs: &[Rec], now: i64, hour: u8, open: &[String]) -> Vec<Suggestion> {
    let nm = names(recs);
    let name = |k: &str| nm.get(k).cloned().unwrap_or_else(|| k.to_string());
    let is_open = |a: &str| open.iter().any(|o| o == a);
    let mut out: Vec<Suggestion> = Vec::new();
    let all = items(recs, now);
    let listed: std::collections::HashSet<&str> = all.iter().filter(|i| i.state != "abgelehnt").map(|i| i.id.as_str()).collect();
    for it in all.iter().filter(|i| i.part == "zusammen" && i.state != "abgelehnt") {
        let (a, b) = (&it.apps[0], &it.apps[1]);
        let (oa, ob) = (is_open(a), is_open(b));
        if oa && ob {
            continue;
        }
        if oa || ob {
            let (run, other) = if oa { (a, b) } else { (b, a) };
            out.push(Suggestion {
                kind: "program",
                app: Some(other.clone()),
                apps: Vec::new(),
                action: None,
                name: name(other),
                reason: format!("passt zu {}, das gerade läuft: du startest beide {}", name(run), together_counts(&it.reason)),
                score: 3.0 + it.count as f64,
            });
        } else {
            out.push(Suggestion {
                kind: "together",
                app: None,
                apps: it.apps.clone(),
                action: Some(format!("start:{a}+{b}")),
                name: it.title.clone(),
                reason: it.reason.clone(),
                score: 1.0 + it.count as f64 * 0.1,
            });
        }
    }
    let mut by_app: BTreeMap<&str, (f64, u32)> = BTreeMap::new();
    for r in recs.iter().filter(|r| (r.part == "zeiten" || r.part == "sofa") && listed.contains(r.id().as_str())) {
        let k = near(hour_of(r), hour);
        if k > 0.0 {
            let e = by_app.entry(r.key.as_str()).or_default();
            e.0 += k * weight(r.ts, now);
            e.1 += 1;
        }
    }
    let (from, to) = ((hour + 23) % 24, (hour + 2) % 24);
    for (app, (score, n)) in by_app {
        if n < 2 || is_open(app) || out.iter().any(|s| s.app.as_deref() == Some(app)) {
            continue;
        }
        let sofa = app == "sofa";
        out.push(Suggestion {
            kind: if sofa { "sofa" } else { "program" },
            app: (!sofa).then(|| app.to_string()),
            apps: Vec::new(),
            action: sofa.then(|| "sofa".to_string()),
            name: if sofa { "Sofa-Modus".into() } else { name(app) },
            reason: format!("weil du {} oft um diese Zeit startest ({n} Mal zwischen {from} und {to} Uhr)", if sofa { "den Sofa-Modus".to_string() } else { name(app) }),
            score,
        });
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    out.truncate(SUGGEST_MAX);
    out
}
