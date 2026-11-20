//! DB4: the page "Was BitsOS über dich gelernt hat" (address `gelernt`, = `einstellungen/gewohnheiten`): ONE page for
//! the one memory, in the settings window like the other groups (design v2's page look). Its switches are
//! settingsd's `gewohnheiten.*` keys (ctxd registers them); below them every learned entry from ctxd's
//! `learned_list` (habits, programs together, times, sound, picks, the file learning, the search memory, the
//! workspaces) with what BitsOS does with it and why, and a "Löschen" each; at the end "Alles vergessen", which
//! asks first. The rows are rebuilt when the list changes (`App::gelernt_changed`).

use std::sync::Mutex;

use crate::habit_app::{Entry, Learned};
use crate::page::{Kind, Model, Row, Section};

pub const PREFIX: &str = "gelernt.";
pub const GROUP: &str = "gewohnheiten";
pub const ALL_KEY: &str = "gelernt.alles";
pub const EMPTY_KEY: &str = "gelernt.leer";
pub const ADDRESS: &str = "einstellungen/gewohnheiten";
pub const MASTER: &str = "gewohnheiten.enabled";

/// The rows as the newest list made them (page_schema's model takes them when it is built anew).
static ROWS: Mutex<Vec<Row>> = Mutex::new(Vec::new());

pub fn is_own(key: &str) -> bool {
    key.starts_with(PREFIX)
}

fn row(key: String, label: String, kind: Kind) -> Row {
    Row { key, kind, default: String::new(), short: label.clone(), label, note: String::new(), confirm: false, stack: false }
}

/// What BitsOS does with an entry, in front of its reason.
pub fn doing(state: &str) -> &'static str {
    match state {
        "nachgemacht" => "Macht BitsOS so",
        "vorgeschlagen" => "Nur ein Vorschlag",
        "abgelehnt" => "Nicht mehr nachgemacht: du hast „Rückgängig“ gewählt",
        _ => "Gelernt",
    }
}

pub fn rows(l: &Learned) -> Vec<Row> {
    if !l.loaded {
        return Vec::new();
    }
    let mut v: Vec<Row> = l.entries.iter().map(|e| row(format!("{PREFIX}{}", e.id), e.title.clone(), Kind::Buttons(vec![("forget".into(), "Löschen".into())]))).collect();
    if v.is_empty() {
        v.push(row(EMPTY_KEY.into(), "BitsOS hat noch nichts über dich gelernt".into(), Kind::Fixed(String::new())));
    }
    v.push(row(ALL_KEY.into(), "Alles vergessen".into(), Kind::Buttons(vec![("all".into(), "Alles vergessen …".into())])));
    v
}

/// The line under an entry: what BitsOS does, why, and whether its part is switched off.
pub fn sub(l: &Learned, e: &Entry) -> String {
    let off = l.parts.iter().any(|(p, _, on)| *p == e.part && !on) || !l.enabled;
    let mut s = format!("{} · {}", doing(&e.state), e.reason);
    if off {
        s.push_str(" · abgeschaltet: lernt nichts dazu und macht nichts nach");
    }
    s
}

pub fn sub_of(l: &Learned, key: &str) -> Option<String> {
    if key == ALL_KEY {
        return Some("Löscht alles, was BitsOS auf diesem Rechner über dich gelernt hat. Deine Arbeitsbereiche bleiben.".into());
    }
    let id = key.strip_prefix(PREFIX)?;
    l.entries.iter().find(|e| e.id == id).map(|e| sub(l, e))
}

pub fn set_now(rows: Vec<Row>) {
    if let Ok(mut r) = ROWS.lock() {
        *r = rows;
    }
}

pub fn rows_now() -> Vec<Row> {
    ROWS.lock().map(|r| r.clone()).unwrap_or_default()
}

/// Where the learned rows go among a section's `rows`: after the master switch (else at the end).
pub fn insert_at(rows: &[Row]) -> usize {
    rows.iter().position(|r| r.key == MASTER).map_or(rows.len(), |i| i + 1)
}

/// `m` with the learned rows of the gewohnheiten section replaced by `new` (the other sections move with it);
/// unchanged without that section.
pub fn splice(m: &Model, new: &[Row]) -> Model {
    let Some((at, range)) = m.sections.iter().enumerate().find_map(|(i, s)| match s {
        Section::Rows(_, r) if m.rows[r.clone()].iter().any(|x| x.key.starts_with(&format!("{GROUP}.")) || is_own(&x.key)) => Some((i, r.clone())),
        _ => None,
    }) else {
        return m.clone();
    };
    let mut rows: Vec<Row> = m.rows[..range.start].to_vec();
    let mut own: Vec<Row> = m.rows[range.clone()].iter().filter(|r| !is_own(&r.key)).cloned().collect();
    let pos = insert_at(&own);
    own.splice(pos..pos, new.iter().cloned());
    rows.extend(own);
    let end = rows.len();
    let delta = end as isize - range.end as isize;
    rows.extend(m.rows[range.end..].iter().cloned());
    let sections = m
        .sections
        .iter()
        .enumerate()
        .map(|(i, s)| match s {
            Section::Rows(n, r) if i == at => Section::Rows(n.clone(), r.start..end),
            Section::Rows(n, r) if i > at => Section::Rows(n.clone(), (r.start as isize + delta) as usize..(r.end as isize + delta) as usize),
            other => other.clone(),
        })
        .collect();
    Model { rows, sections, heads: m.heads, flags: Vec::new(), col_min: m.col_min }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learned(n: usize) -> Learned {
        Learned {
            loaded: true,
            enabled: true,
            parts: vec![("fenster".into(), "Fenster".into(), false)],
            entries: (0..n).map(|i| Entry { id: format!("fenster:a{i}"), part: "fenster".into(), title: format!("A{i} startet maximiert"), reason: "weil du A 3 von 3 Mal maximiert hast".into(), state: "nachgemacht".into() }).collect(),
        }
    }

    #[test]
    fn every_entry_has_a_delete_and_its_reason_and_the_end_asks_for_all() {
        let l = learned(2);
        let r = rows(&l);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].kind, Kind::Buttons(vec![("forget".into(), "Löschen".into())]));
        assert_eq!(r[2].key, ALL_KEY);
        let s = sub_of(&l, &r[0].key).unwrap();
        assert!(s.starts_with("Macht BitsOS so · weil du A 3 von 3 Mal"), "{s}");
        assert!(s.ends_with("abgeschaltet: lernt nichts dazu und macht nichts nach"), "its part is off: {s}");
        assert_eq!(rows(&learned(0))[0].key, EMPTY_KEY);
        assert!(rows(&Learned::default()).is_empty(), "nothing before ctxd answered");
    }

    #[test]
    fn a_new_list_replaces_the_rows_and_moves_the_later_sections() {
        let sw = |k: &str| row(k.into(), k.into(), Kind::Switch);
        let m = Model {
            rows: vec![sw("a.x"), sw("gewohnheiten.enabled"), row("gelernt.old".into(), "o".into(), Kind::Fixed(String::new())), sw("gewohnheiten.zeiten"), sw("z.y")],
            sections: vec![Section::Rows("A".into(), 0..1), Section::Favourites, Section::Rows("G".into(), 1..4), Section::Rows("Z".into(), 4..5)],
            ..Default::default()
        };
        let new = rows(&learned(2));
        let out = splice(&m, &new);
        let keys: Vec<&str> = out.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["a.x", "gewohnheiten.enabled", "gelernt.fenster:a0", "gelernt.fenster:a1", ALL_KEY, "gewohnheiten.zeiten", "z.y"], "under the master switch, the parts' switches after");
        assert_eq!(out.sections[2], Section::Rows("G".into(), 1..6));
        assert_eq!(out.sections[3], Section::Rows("Z".into(), 6..7));
        assert_eq!(splice(&Model::default(), &new), Model::default());
    }
}
