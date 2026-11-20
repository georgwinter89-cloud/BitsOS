//! WY-SET: the settings tabs' consolidations: one row that stands for several keys of settingsd (or for one key
//! in plainer words). Each writes the keys under it exactly; the keys themselves stay as rows under the card's
//! "Mehr", so nothing is lost and search finds them. Pure: values from a getter, writes as (key, value) pairs.
//! Table of all mappings: plan/spec/bitscomp.md §5.

use mcpreg::Json;

use crate::page::{Kind, Row, Sub};

pub const ZEIGER: &str = "set.zeiger";
pub const SCHRIFTEN: &str = "set.schriften";
pub const DAUER: &str = "set.sicherung.dauer";
pub const VERBINDUNG: &str = "set.netz.verbindung";
/// A provider's card (`set.anbieter.<id>`) and the way to add one.
pub const ANBIETER: &str = "set.anbieter.";
pub const ANBIETER_NEU: &str = "set.anbieter.+";
/// EINS-B (board 4): the rules that hold at every level, one sentence each (`set.gilt.1` .. `4`), and the line
/// under them (`set.gilt.ende`): no control, not switchable.
pub const GILT: &str = "set.gilt.";
/// The search's last row, and a tab's only row while the service that owns its keys has not registered them.
pub const FRAG: &str = "set.frag";
pub const LEER: &str = "set.leer";

/// A row that is only a sentence (no value, no control).
pub fn quiet(key: &str) -> bool {
    key == FRAG || key == LEER || key.starts_with(GILT)
}
pub const OWN: &str = "eigene";

pub const FONT_KEYS: [&str; 3] = ["theme.font.display", "theme.font.ui", "theme.font.mono"];
/// "Schriften": (value, name, display / ui / mono).
pub const FONTS: [(&str, &str, [&str; 3]); 3] = [
    ("bitsos", "BitsOS", ["Bricolage Grotesque", "Figtree", "JetBrains Mono"]),
    ("klassisch", "Klassisch", ["Figtree", "Figtree", "JetBrains Mono"]),
    ("technisch", "Technisch", ["JetBrains Mono", "Exo 2", "JetBrains Mono"]),
];

pub const CURSOR: &str = "theme.cursor.size";
/// The pointer's size in plain words (px).
pub const SIZES: [(&str, &str, i64); 3] = [("klein", "Klein", 24), ("mittel", "Mittel", 32), ("gross", "Groß", 48)];

/// "Wie lange Sicherungen bleiben": (key, name, unit).
pub const KEEP: [(&str, &str, &str); 5] = [
    ("snap.keep_all_hours", "jede", "Std"),
    ("snap.hourly_days", "stündlich", "Tage"),
    ("snap.daily_days", "täglich", "Tage"),
    ("snap.weekly_months", "wöchentlich", "Monate"),
    ("snap.min_free_pct", "frei halten", "%"),
];

/// The job that names one provider ("Antwortet für alles"; its fallback list stays text under "Mehr").
pub const ROLES: [&str; 1] = ["ai.role.chat"];

fn row(key: &str, kind: Kind, label: &str) -> Row {
    Row { key: key.into(), kind, default: String::new(), label: label.into(), short: label.into(), note: String::new(), confirm: false, stack: false }
}

/// A provider as its card shows it: its id (the key's word), a plain name, the line under it.
#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub line: String,
}

/// Board 4: the abo through Claude Code is named as Georg knows it, not by its id.
pub const ABO: &str = "Claude über dein Abo";

/// The page's own rows for the keys settingsd has (`known`): each consolidation whose keys exist, the providers'
/// cards, the network's state.
pub fn rows(known: &dyn Fn(&str) -> bool, keys: &[&str], providers: &[Provider]) -> Vec<Row> {
    let mut v = Vec::new();
    if known(CURSOR) {
        v.push(row(ZEIGER, Kind::Choice(SIZES.iter().map(|(a, b, _)| (a.to_string(), b.to_string())).collect()), "Mauszeiger"));
    }
    if FONT_KEYS.iter().any(|k| known(k)) {
        v.push(row(SCHRIFTEN, Kind::Choice(FONTS.iter().map(|(a, b, _)| (a.to_string(), b.to_string())).collect()), "Schriften"));
    }
    let keep: Vec<Sub> = KEEP.iter().filter(|(k, _, _)| known(k)).map(|(k, l, u)| Sub { key: (*k).into(), label: (*l).into(), unit: (*u).into(), switch: false }).collect();
    if !keep.is_empty() {
        v.push(Row { stack: true, ..row(DAUER, Kind::Subs(keep), "Wie lange Sicherungen bleiben") });
    }
    if keys.contains(&crate::assist_level::LEVEL) {
        let gilt = crate::assist_level::FIXED.iter().enumerate().map(|(i, t)| (format!("{GILT}{}", i + 1), *t));
        for (key, text) in gilt.chain([(format!("{GILT}ende"), crate::assist_level::FIXED_END)]) {
            v.push(row(&key, Kind::Fixed(String::new()), text));
        }
    }
    for p in providers {
        v.push(row(&format!("{ANBIETER}{}", p.id), Kind::Link(format!("einstellungen/ki/{}", p.id)), &p.name));
    }
    v.push(row(ANBIETER_NEU, Kind::Link("einstellungen/ki".into()), "+ Anbieter hinzufügen"));
    v.push(row(VERBINDUNG, Kind::Fixed(String::new()), "Verbindung"));
    v
}

/// The line under a consolidated row (None: the row's address as usual).
pub fn sub_line(key: &str, value: &str, providers: &[Provider]) -> Option<String> {
    if let Some(which) = key.strip_prefix(GILT) {
        // PLAN K2's honest limit under the last line.
        return Some(if which == "ende" { crate::assist_level::HONEST.to_string() } else { String::new() });
    }
    if key == ANBIETER_NEU {
        return Some("Auf der Seite „KI“: Art, Adresse und Schlüssel".into());
    }
    if let Some(id) = key.strip_prefix(ANBIETER) {
        return providers.iter().find(|p| p.id == id).map(|p| p.line.clone());
    }
    Some(match key {
        SCHRIFTEN if value == OWN => "Eigene Zusammenstellung (die einzelnen Schriften stehen unter „Mehr“)".into(),
        SCHRIFTEN => "Überschriften, Oberfläche und Beschriftungen zusammen".into(),
        ZEIGER if value.starts_with(OWN) => format!("Eigene Größe: {} px (unter „Mehr“)", value.trim_start_matches(OWN).trim_start_matches(':')),
        ZEIGER => "Größe des Mauszeigers".into(),
        DAUER => format!("fasst {} Einstellungen in einer Zeile zusammen; ein Klick auf eine Zahl ändert sie", KEEP.len()),
        VERBINDUNG => "wie die Statusleiste es zeigt".into(),
        FRAG => "Win+F1 erklärt jede Einstellung".into(),
        LEER => "Seine Einstellungen erscheinen hier, sobald er läuft.".into(),
        _ => return None,
    })
}

/// What a consolidated row shows, from the keys' values (`get`: settingsd's value, else the key's default; None:
/// no such key). None: not a consolidation.
pub fn value(key: &str, get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    Some(match key {
        SCHRIFTEN => {
            let now: Vec<String> = FONT_KEYS.iter().map(|k| get(k).unwrap_or_default()).collect();
            FONTS.iter().find(|(_, _, f)| f.iter().zip(&now).all(|(a, b)| a.eq_ignore_ascii_case(b.trim()))).map_or(OWN.into(), |(v, _, _)| v.to_string())
        }
        ZEIGER => {
            let px = get(CURSOR).and_then(|v| v.trim().parse::<i64>().ok());
            match px.and_then(|n| SIZES.iter().find(|(_, _, s)| *s == n)) {
                Some((v, _, _)) => v.to_string(),
                None => format!("{OWN}:{}", px.map_or("–".into(), |n| n.to_string())),
            }
        }
        _ => return None,
    })
}

/// The consolidated row that shows `key` among others (the search lists the key alone only without it).
pub fn joined(key: &str) -> Option<String> {
    Some(match key {
        CURSOR => ZEIGER,
        _ if FONT_KEYS.contains(&key) => SCHRIFTEN,
        _ if KEEP.iter().any(|(k, ..)| *k == key) => DAUER,
        _ => return None,
    }
    .to_string())
}

/// The keys a click on choice `choice` of consolidated row `key` writes (only keys settingsd has). None: not a
/// consolidation with choices.
pub fn writes(key: &str, choice: &str, known: &dyn Fn(&str) -> bool) -> Option<Vec<(String, Json)>> {
    match key {
        SCHRIFTEN => {
            let (_, _, f) = FONTS.iter().find(|(v, _, _)| *v == choice)?;
            Some(FONT_KEYS.iter().zip(f).filter(|(k, _)| known(k)).map(|(k, v)| (k.to_string(), Json::str(v))).collect())
        }
        ZEIGER => {
            let (_, _, px) = SIZES.iter().find(|(v, _, _)| *v == choice)?;
            Some(vec![(CURSOR.into(), Json::Num(*px as f64))])
        }
        _ => None,
    }
}

/// Whether setting `key` from `old` to `new` gives the assistant more freedom (only Georg's own confirmed yes:
/// PLAN K2, the page asks before it sends, settingsd's leash too).
pub fn loosens(key: &str, old: &str, new: &str) -> bool {
    let words = |s: &str| s.split(',').map(crate::assist_level::program_word).filter(|w| !w.is_empty()).collect::<Vec<_>>();
    match key {
        crate::assist_level::LEVEL => crate::assist_level::loosens(old, new),
        // A program taken off the never-see list is seen again; the sidebar seeing the window in front sends it.
        crate::assist_level::NEVER_SEE => words(old).iter().any(|w| !words(new).contains(w)),
        crate::assist_level::SIDEBAR_SEES => old.trim() != "true" && new.trim() == "true",
        // settingsd's leash: turning it on lets MCP make BitsOS's own tools run without a question for good.
        "ai.mcp.ask_tools" => old.trim() == "false" && new.trim() != "false",
        _ => false,
    }
}

/// settingsd's leash (`leash.rs` leashed): a key whose loosening only Georg's own yes may confirm. bitscomp is
/// the confirmer settingsd vouches for, so its dialogs take that yes from real input only.
pub fn leashed(key: &str) -> bool {
    const CONTEXT: [&str; 6] = ["context.roots", "context.private_roots", "context.excludes", "context.claude.snippet_chars", "context.claude.allow_forget", "context.enabled"];
    const VIEWS: [&str; 3] = [crate::assist_level::LEVEL, crate::assist_level::NEVER_SEE, crate::assist_level::SIDEBAR_SEES];
    VIEWS.contains(&key) || key == "ai.mcp.ask_tools" || CONTEXT.contains(&key)
}

/// The rows of "Wer macht was" as a choice of the providers (and "Keiner"), instead of a text field.
pub fn role_kind(providers: &[Provider]) -> Kind {
    let mut v: Vec<(String, String)> = providers.iter().map(|p| (p.id.clone(), p.name.clone())).collect();
    v.push((String::new(), "Keiner".into()));
    Kind::Choice(v)
}

#[cfg(test)]
#[path = "page_join_tests.rs"]
mod tests;
