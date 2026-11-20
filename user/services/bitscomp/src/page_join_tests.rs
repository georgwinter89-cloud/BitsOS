use super::*;

fn keys() -> Vec<&'static str> {
    crate::page_tabs::tests::gen151_keys()
}

#[test]
fn the_fixed_rules_are_sentences_without_a_switch() {
    let all = keys();
    let known = |k: &str| all.contains(&k);
    let r = rows(&known, &all, &[]);
    let gilt: Vec<&Row> = r.iter().filter(|x| x.key.starts_with(GILT)).collect();
    assert_eq!(gilt.len(), 5, "F1-F4 and the line under them");
    assert_eq!(sub_line("set.gilt.ende", "", &[]).as_deref(), Some(crate::assist_level::HONEST), "the honest limit under it");
    for (row, want) in gilt.iter().zip(crate::assist_level::FIXED.iter().chain([&crate::assist_level::FIXED_END])) {
        assert_eq!(row.label, *want);
        assert!(matches!(row.kind, Kind::Fixed(_)) && quiet(&row.key), "{}: not switchable", row.key);
    }
    assert!(rows(&known, &[], &[]).iter().all(|x| !x.key.starts_with(GILT)), "only once the level is there");
}

#[test]
fn fonts_and_pointer_in_plain_words_both_ways() {
    let known = |_: &str| true;
    for (id, _, f) in FONTS {
        let w = writes(SCHRIFTEN, id, &known).unwrap();
        assert_eq!(w.len(), 3);
        let m: std::collections::HashMap<String, String> = w.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect();
        assert_eq!(value(SCHRIFTEN, &|k| m.get(k).cloned()).as_deref(), Some(id), "{id}: {f:?}");
    }
    assert_eq!(value(SCHRIFTEN, &|k| Some(if k == "theme.font.ui" { "Exo 2".into() } else { "Figtree".into() })).as_deref(), Some(OWN));
    assert_eq!(writes(SCHRIFTEN, "comic", &known), None);
    for (id, _, px) in SIZES {
        assert_eq!(writes(ZEIGER, id, &known).unwrap(), vec![(CURSOR.to_string(), Json::Num(px as f64))]);
        assert_eq!(value(ZEIGER, &|_| Some(px.to_string())).as_deref(), Some(id));
    }
    assert_eq!(value(ZEIGER, &|_| Some("30".into())).as_deref(), Some("eigene:30"), "Georg's 30 px is his own size");
    assert!(sub_line(ZEIGER, "eigene:30", &[]).unwrap().contains("30 px"));
}

#[test]
fn more_freedom_for_the_assistant_is_noticed() {
    assert!(loosens("ai.level", "ask", "auto") && loosens("ai.level", "answer", "ask"));
    assert!(!loosens("ai.level", "auto", "ask") && !loosens("ai.level", "auto", "answer"), "tightening is anyone's");
    assert!(!loosens("ai.level", "ask", "ask"));
    assert!(loosens("ai.mcp.ask_tools", "false", "true") && !loosens("ai.mcp.ask_tools", "true", "false"));
    assert!(!loosens("theme.font.ui", "a", "b"));
    // A program off the never-see list, or the sidebar seeing the window in front: the assistant sees more.
    assert!(loosens("assistant.never_see", "keepassxc, brave", "brave") && loosens("assistant.never_see", "brave", ""));
    assert!(!loosens("assistant.never_see", "brave", "brave, keepassxc") && !loosens("assistant.never_see", "Brave", "brave"));
    assert!(loosens("assistant.sidebar_sees", "false", "true") && loosens("assistant.sidebar_sees", "", "true"));
    assert!(!loosens("assistant.sidebar_sees", "true", "false"));
    assert!(leashed("assistant.never_see") && leashed("assistant.sidebar_sees"));
    assert!(leashed("ai.level") && leashed("ai.mcp.ask_tools") && leashed("context.roots"));
    assert!(!leashed("ai.helper.write.allow") && !leashed("ai.tools.settings_set.allow"), "gone with the one level");
}

#[test]
fn the_tabs_own_rows_exist_only_for_keys_settingsd_has() {
    let all = keys();
    let known = |k: &str| all.contains(&k);
    let providers = vec![Provider { id: "abo".into(), name: ABO.into(), line: "in der Cloud · nimmt Bilder an · ● erreichbar".into() }];
    let r = rows(&known, &all, &providers);
    let names: Vec<&str> = r.iter().map(|x| x.key.as_str()).collect();
    for k in [ZEIGER, SCHRIFTEN, DAUER, "set.gilt.1", "set.anbieter.abo", ANBIETER_NEU, VERBINDUNG] {
        assert!(names.contains(&k), "{k} in {names:?}");
    }
    assert!(!names.iter().any(|k| k.starts_with("set.darf.") || k.starts_with("set.auto.") || k.starts_with("set.handeln.")), "the old questions went: {names:?}");
    let keep = r.iter().find(|x| x.key == DAUER).unwrap();
    assert!(matches!(&keep.kind, Kind::Subs(s) if s.len() == 5 && !s[0].switch));
    let none = rows(&|_| false, &[], &[]);
    assert_eq!(none.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), [ANBIETER_NEU, VERBINDUNG], "nothing to consolidate without keys");
    assert!(matches!(role_kind(&providers), Kind::Choice(o) if o.len() == 2 && o[0] == ("abo".to_string(), ABO.to_string()) && o[1].1 == "Keiner"));
    // Board 4: the card names the abo plainly, its line says where it answers and that it answers now.
    let card = r.iter().find(|x| x.key == "set.anbieter.abo").unwrap();
    assert_eq!(card.label, "Claude über dein Abo");
    assert_eq!(sub_line("set.anbieter.abo", "", &providers).as_deref(), Some("in der Cloud · nimmt Bilder an · ● erreichbar"));
    assert_eq!(ROLES, ["ai.role.chat"], "one job: Antwortet für alles");
}
