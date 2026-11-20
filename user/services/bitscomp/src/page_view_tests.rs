use std::collections::{BTreeSet, HashMap};

use super::*;
use crate::page_schema::{Spec, Ty, model};

/// The Mini-PC's registry (generation 151) as rows, and the values it had: a key marked changed there gets
/// another default here.
fn gen151() -> (Model, HashMap<String, String>) {
    let mut specs = Vec::new();
    let mut vals = HashMap::new();
    for l in crate::page_tabs::tests::GEN151.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        let (key, changed, value, de) = (f[0], f[1] == "*", f[2], f[3]);
        let ty = match value {
            "true" | "false" => Ty::Bool,
            v if v.parse::<i64>().is_ok() => Ty::Int { min: 0, max: 1_000_000, unit: String::new(), word: None },
            v if v.starts_with('#') && v.len() == 7 => Ty::Color,
            _ => Ty::Text { max: 4096 },
        };
        let default = if changed { if ty == Ty::Bool { (value != "true").to_string() } else { "0".into() } } else { value.to_string() };
        let safety = if key.ends_with(".key") { "secret" } else if key.starts_with("durchblick.handeln.auto.") { "confirm" } else { "normal" };
        specs.push(Spec { key: key.into(), ty, default: Some(default), de: de.into(), names: None, restart: "none".into(), safety: safety.into() });
        vals.insert(key.to_string(), value.to_string());
    }
    (model(&specs), vals)
}

struct T {
    full: Model,
    vals: HashMap<String, String>,
    open: BTreeSet<String>,
}

impl T {
    fn new() -> T {
        let (full, vals) = gen151();
        T { full, vals, open: BTreeSet::new() }
    }

    fn view(&self, tab: usize, query: &str, changed_only: bool, target: &str) -> View {
        let value = |r: &Row| match &r.kind {
            Kind::Subs(s) => s.iter().map(|s| self.vals.get(&s.key).cloned().unwrap_or_default()).collect::<Vec<_>>().join(";"),
            _ => self.vals.get(&r.key).cloned().unwrap_or_else(|| r.default.clone()),
        };
        let known = |k: &str| self.vals.contains_key(k);
        let providers = vec![crate::page_join::Provider { id: "abo".into(), name: crate::page_join::ABO.into(), line: "in der Cloud · nimmt Bilder an".into() }];
        build(&self.full, &Ctx { tab, query, changed_only, open: &self.open, target, value: &value, known: &known, programs: &[], providers: &providers })
    }
}

fn heads(m: &Model) -> Vec<String> {
    m.sections
        .iter()
        .map(|s| match s {
            Section::Rows(n, _) => n.clone(),
            Section::Favourites => "FAV".into(),
        })
        .collect()
}

fn keys(m: &Model) -> Vec<&str> {
    m.rows.iter().map(|r| r.key.as_str()).collect()
}

#[test]
fn the_look_tab_shows_the_mock_ups_cards_with_the_rare_rows_folded() {
    let mut t = T::new();
    let v = t.view(0, "", false, "");
    assert_eq!(v.title, "Aussehen");
    assert_eq!(heads(&v.model), ["Design", "Akzentfarbe", "Größe", "Schrift und Bewegung", "", ""]);
    let k = keys(&v.model);
    // Generation 151 has no hours of "Nach Uhrzeit" yet: the design card is the mode alone.
    assert_eq!(&k[..2], ["theme.mode", "theme.accent"]);
    for want in [ "theme.scale", "theme.font.size", "set.zeiger", "set.schriften", "theme.motion", "mehr.fein", "mehr.farben"] {
        assert!(k.contains(&want), "{want} in {k:?}");
    }
    for hidden in ["theme.dark.bg", "theme.glow", "theme.font.ui", "theme.cursor.size"] {
        assert!(!k.contains(&hidden), "{hidden} is folded");
    }
    assert_eq!(v.counts[0], 7, "the mock-up's 8 but the effect level: design, accent, size, font size, pointer, fonts, motion");
    let fold = v.model.rows.iter().find(|r| r.key == "mehr.farben").unwrap();
    assert_eq!(fold.kind, Kind::Fold { open: false, count: 38, changed: 0 });
    assert_eq!(v.model.heads, Heads::Title);
    // Opened: its rows follow the fold row; the address of a folded key opens its card by itself.
    t.open.insert("fein".into());
    let o = t.view(0, "", false, "");
    let at = keys(&o.model).iter().position(|k| *k == "mehr.fein").unwrap();
    assert!(matches!(o.model.rows[at].kind, Kind::Fold { open: true, count: 4, changed: 4 }), "{:?}", o.model.rows[at].kind);
    assert!(keys(&o.model)[at + 1..at + 5].contains(&"theme.glow"));
    let jump = t.view(0, "", false, "theme.dark.bg");
    assert!(keys(&jump.model).contains(&"theme.dark.bg"), "a target under Mehr opens its card");
}

#[test]
fn every_row_is_in_exactly_one_tab_and_nothing_is_lost() {
    let mut t = T::new();
    for c in CARDS.iter() {
        t.open.insert(c.id.into());
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for tab in 0..TABS.len() {
        for r in t.view(tab, "", false, "").model.rows {
            if !r.key.starts_with(FOLD) {
                *seen.entry(r.key).or_default() += 1;
            }
        }
    }
    for k in crate::page_tabs::tests::gen151_keys() {
        if k == crate::page::FAVOURITES {
            continue;
        }
        assert_eq!(seen.get(k), Some(&1), "{k}");
    }
    assert!(seen.values().all(|n| *n == 1));
    let desk = t.view(1, "", false, "");
    assert!(heads(&desk.model).contains(&"FAV".to_string()), "the favourites are on Schreibtisch");
}

#[test]
fn the_tab_assistent_is_board_4_and_shows_the_migrated_level() {
    let t = T::new();
    let v = t.view(4, "", false, "");
    assert_eq!(heads(&v.model), ["Wie viel er darf", "Gilt immer, in jeder Stufe", "Was er sieht", "Welche KI", ""]);
    assert_eq!((v.title.as_str(), v.sub.as_str()), ("Assistent", "Ein Assistent, in der Seitenleiste und mit Win+F1. Was hier steht, gilt für beide."));
    let k = keys(&v.model);
    assert_eq!(k[0], "ai.level");
    let level = &v.model.rows[0];
    assert!(matches!(&level.kind, Kind::Pills { multi: false, notes, .. } if notes.len() == 3), "three rows with their sentences");
    // PLAN K8: Georg's Mini-PC had the Automodus on for programs: the migration made it `auto`, and the page shows
    // it so (lit, and among what differs from the default).
    assert_eq!(t.vals.get("ai.level").map(String::as_str), Some("auto"));
    assert!(v.model.flags[0].changed, "the migrated level is marked as changed");
    assert_eq!(k.iter().filter(|k| k.starts_with("set.gilt.")).count(), 5, "F1-F4 and their line, not switchable");
    assert!(k.contains(&"assistant.never_see") && k.contains(&"assistant.sidebar_sees"));
    let never = v.model.rows.iter().find(|r| r.key == "assistant.never_see").unwrap();
    assert!(matches!(&never.kind, Kind::Pills { multi: true, .. }), "programs to pick");
    let line = |key: &str| v.lines[v.model.rows.iter().position(|r| r.key == key).unwrap()].clone().unwrap_or_default();
    assert!(line("assistant.never_see").starts_with("Bei Win+F1 sieht er das Fenster, das vorne ist, ohne vorher zu fragen."), "board 4");
    assert_eq!(line("assistant.sidebar_sees"), "Denselben Schalter gibt es direkt in der Seitenleiste.", "board 4");
    let chat = v.model.rows.iter().find(|r| r.key == "ai.role.chat").unwrap();
    assert!(matches!(&chat.kind, Kind::Choice(o) if o[0].0 == "abo" && o.last().unwrap().1 == "Keiner"), "Antwortet für alles: a choice of the providers");
    assert!(!k.iter().any(|k| k.starts_with("set.darf.") || k.starts_with("ai.tools.") || k.starts_with("ai.helper.")), "{k:?}");
    let more = v.model.rows.iter().find(|r| r.key == "mehr.kimehr").unwrap();
    assert_eq!(more.label, "Mehr: weitere Anbieter, Ersatz, Antwortlänge, Tastenkürzel (Win+F1)");
    let p = t.view(5, "", false, "");
    assert_eq!(heads(&p.model), ["Was BitsOS über dich gelernt hat", "Wer hat was gemacht", "Kontextdienst"]);
}

#[test]
fn search_finds_folded_rows_in_every_tab_and_counts_hits() {
    let t = T::new();
    let v = t.view(0, "sicher", false, "");
    let hits = v.hits.unwrap();
    assert!(hits[7] >= 3, "Sicherungen: {hits:?}");
    assert_eq!(v.model.heads, Heads::Crumb);
    assert!(heads(&v.model)[0] == "Sicherungen" || heads(&v.model).contains(&"Sicherungen".to_string()));
    assert!(v.title.contains("Treffer für „sicher“"), "{}", v.title);
    let r = t.view(0, "Rasterlinien", false, "");
    assert!(keys(&r.model).contains(&"theme.scanlines"), "a row under a closed Mehr is found");
    assert!(heads(&r.model).iter().any(|h| h == "Aussehen"), "breadcrumb of a card without title: the tab");
    let n = t.view(0, "Fenster vorne", false, "");
    assert!(heads(&n.model).iter().any(|h| h == "Assistent › Was er sieht"), "{:?}", heads(&n.model));
    let none = t.view(0, "qqqqzz", false, "");
    assert_eq!(none.hits.unwrap().iter().sum::<usize>(), 0);
    assert_eq!(keys(&none.model), [crate::page_join::FRAG], "nothing found: ask the assistant");
    assert!(none.title.starts_with("Keine Treffer"));
}

#[test]
fn search_shows_a_consolidation_and_its_keys_once() {
    let t = T::new();
    let v = t.view(0, "sicher", false, "");
    let k = keys(&v.model);
    assert!(k.contains(&crate::page_join::DAUER), "{k:?}");
    for (single, ..) in crate::page_join::KEEP {
        assert!(!k.contains(&single), "{single} listed beside its row Wie lange Sicherungen bleiben: {k:?}");
    }
    let darf = t.view(0, "stufe", false, "");
    let k = keys(&darf.model);
    assert!(k.contains(&"ai.level"), "{k:?}");
    assert!(!k.iter().any(|x| x.starts_with("ai.helper.") || x.starts_with("ai.tools.")), "the removed keys never show: {k:?}");
    for (q, joined, single) in [("schrift", crate::page_join::SCHRIFTEN, "theme.font.ui"), ("mauszeiger", crate::page_join::ZEIGER, "theme.cursor.size")] {
        let k = keys(&t.view(0, q, false, "").model).into_iter().map(String::from).collect::<Vec<_>>();
        assert!(k.iter().any(|x| x == joined) && !k.iter().any(|x| x == single), "{q}: {k:?}");
    }
    // A key under a closed Mehr is found on its own; a removed one never.
    let alone = t.view(0, "ai.timeout_s", false, "");
    assert!(keys(&alone.model).contains(&"ai.timeout_s"), "{:?}", keys(&alone.model));
    assert_eq!(alone.hits.unwrap().iter().sum::<usize>(), 1);
    assert_eq!(t.view(0, "ai.helper.write", false, "").hits.unwrap().iter().sum::<usize>(), 0);
    // "Nur geänderte" lists the single keys (a consolidation is no key of settingsd).
    assert!(keys(&t.view(0, "", true, "").model).contains(&"theme.cursor.size"));
}

#[test]
fn only_changed_lists_every_changed_key_with_reset_and_no_secret() {
    let t = T::new();
    let v = t.view(0, "", true, "");
    let marked: Vec<&str> = crate::page_tabs::tests::GEN151.lines().filter(|l| l.split('\t').nth(1) == Some("*")).map(|l| l.split('\t').next().unwrap()).collect();
    let listed = keys(&v.model);
    for k in &marked {
        if *k != crate::page::FAVOURITES {
            assert!(listed.contains(k), "{k} differs from its default");
        }
    }
    assert!(!listed.contains(&"ai.provider.abo.key"), "a secret is never listed");
    assert!(listed.iter().all(|k| !k.starts_with("set.")), "the page's own rows are no keys");
    assert!(v.model.flags.iter().all(|f| f.changed && f.reset));
    assert_eq!(v.changed, listed.len());
    assert!(v.title.contains("vom Standard ab"));
    // Search and filter together.
    let both = t.view(0, "snap", true, "");
    assert_eq!(keys(&both.model).iter().filter(|k| k.starts_with("snap.")).count(), 1, "snap.enabled only");
}

#[test]
fn old_addresses_land_on_their_tab() {
    let t = T::new();
    for (prefix, tab) in [
        ("gewohnheiten", 5),
        ("durchblick", 4),
        ("assistant", 4),
        ("kernel.snd_output", 2),
        ("theme.layout.statusbar", 1),
        (crate::page::FAVOURITES, 1),
        ("pkg", 6),
        ("ai.assistant.name", 4),
        ("access", 5),
        ("theme.accent", 0),
        ("journal", 8),
    ] {
        assert_eq!(tab_of_prefix(&t.full.rows, prefix), Some(tab), "{prefix}");
    }
    assert_eq!(tab_of_prefix(&t.full.rows, "nothing.here"), None);
    use crate::page_app::PageState;
    let s = PageState::new("einstellungen/schreibtisch");
    assert_eq!((s.tab, s.tab_given, s.prefix.as_str()), (1, true, ""));
    let s = PageState::new("einstellungen/aussehen/akzent");
    assert_eq!((s.tab, s.tab_given, s.prefix.as_str()), (0, true, "theme.accent"));
    let s = PageState::new("einstellungen/kernel/snd_output");
    assert_eq!((s.tab_given, s.prefix.as_str()), (false, "kernel.snd_output"));
    let s = PageState::new("settings/Assistent");
    assert_eq!((s.tab, s.tab_given), (4, true));
    let s = PageState::new("einstellungen/programme/hinzufuegen");
    assert_eq!((s.tab, s.prefix.as_str()), (6, "pkg.hinzufuegen"));
    assert_eq!(PageState::new("einstellungen/ki").kind, crate::page_kind::PageKind::Ki, "the page KI stays");
    assert_eq!(crate::page::prefix_of("sicherungen"), "", "a tab word is no key prefix");
}
