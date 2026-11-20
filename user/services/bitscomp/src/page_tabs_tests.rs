use super::*;

/// The keys of the Mini-PC's settingsd (generation 151) as EINS leaves them (PLAN K8: the removed keys gone,
/// `ai.level` migrated to `auto`, the views' two keys new): key, changed mark, value, German description.
pub(crate) const GEN151: &str = include_str!("page_tabs_keys.txt");

pub(crate) fn gen151_keys() -> Vec<&'static str> {
    GEN151.lines().filter_map(|l| l.split('\t').next()).filter(|k| !k.is_empty()).collect()
}

/// The brief's table (desk plan WY-SET 1.): the tab a key belongs in.
fn wanted(key: &str) -> usize {
    let under = |p: &str| key.starts_with(&format!("{p}."));
    match key {
        "kernel.snd_output" | "sofa.start_volume" | "sofa.overscan" => 2,
        "desktop.favourites" => 1,
        // EINS-B (board 4): what the assistant sees and its popup's keys are the tab Assistent's.
        "desktop.durchblick.programs" => 4,
        // A desktop key no rule names stays on Schreibtisch (its namespace is known), under "Weitere".
        _ if under("theme.layout") || under("desktop") || under("sofa") => 1,
        _ if under("theme") => 0,
        _ if under("net") => 3,
        _ if under("ai") || under("context.claude") || under("assistant") || under("durchblick") => 4,
        _ if under("access") || under("gewohnheiten") || under("context") || under("gelernt") => 5,
        _ if under("pkg") => 6,
        _ if under("snap") => 7,
        _ => 8,
    }
}

#[test]
fn every_key_of_generation_151_has_exactly_its_tab() {
    let keys = gen151_keys();
    assert_eq!(keys.len(), 165);
    for k in keys.iter().copied().chain([
        "net.wifi.ssid",
        "leistung.nachts",
        "theme.auto.light_from",
        "gewohnheiten.vorrang",
        "test.risky",
        "zz.new",
        "desktop.something_new",
        "pkg.hinzufuegen",
        "gelernt.alles",
        "access.ansehen",
    ]) {
        assert_eq!(tab_of(k), wanted(k), "{k}");
        let p = place(k);
        assert!(p.card < CARDS.len(), "{k}");
    }
    assert_eq!(CARDS[place("zz.new").card].id, OTHER_CARD, "a new namespace lands in System under Weitere");
    assert!(!place("zz.new").more, "and is shown at once");
}

#[test]
fn the_tab_assistent_has_the_five_cards_of_board_4_and_privatsphaere_three() {
    let ids = |t: usize| CARDS.iter().filter(|c| c.tab == t).map(|c| c.id).collect::<Vec<_>>();
    assert_eq!(ids(4), ["darf", "gilt", "sieht", "welche", "kimehr"]);
    assert_eq!(ids(5), ["gelernt", "access", "kontext"]);
    assert_eq!((TABS[4].title, TABS[5].title), ("Assistent", "Privatsphäre"));
    let card = |k: &str| (CARDS[place(k).card].id, place(k).more);
    assert_eq!(card("ai.level"), ("darf", false));
    assert_eq!(card("set.gilt.1"), ("gilt", false));
    assert_eq!(card("assistant.never_see"), ("sieht", false));
    assert_eq!(card("assistant.sidebar_sees"), ("sieht", false));
    assert_eq!(card("ai.role.chat"), ("welche", false));
    assert_eq!(card("ai.cloud_notice"), ("welche", false));
    for k in ["durchblick.taste", "ai.role.chat_fallback", "ai.max_tokens", "set.anbieter.+", "ai.local_only"] {
        assert_eq!(card(k), ("kimehr", true), "{k}: under „Mehr: weitere Anbieter, Ersatz, Antwortlänge, Tastenkürzel“");
    }
}

#[test]
fn the_removed_keys_never_show_whoever_still_registers_them() {
    for k in ["ai.tools.settings_set.allow", "ai.helper.write.allow", "ai.role.explain", "ai.role.jobs_fallback", "durchblick.antwort", "durchblick.vorschau.liste", "durchblick.bild.auto.steam", "durchblick.handeln.auto.foot", "durchblick.handeln.unumkehrbar_fragen"] {
        assert!(gone(k), "{k}");
    }
    for k in ["ai.level", "ai.role.chat", "durchblick.taste", "durchblick.pad", "assistant.never_see", "ai.mcp.ask_tools"] {
        assert!(!gone(k), "{k}");
    }
}

#[test]
fn cards_are_unique_named_by_rules_and_every_tab_has_some() {
    for (i, c) in CARDS.iter().enumerate() {
        assert_eq!(card_index(c.id), i, "{} once", c.id);
        assert!(c.tab < TABS.len());
    }
    for (_, card, _) in RULES {
        assert!(CARDS.iter().any(|c| c.id == *card), "rule names card {card}");
    }
    for t in 0..TABS.len() {
        assert!(CARDS.iter().any(|c| c.tab == t), "tab {t} has a card");
    }
    // The rules' order is the cards' row order: a catch-all never hides a later rule.
    for (i, (p, _, _)) in RULES.iter().enumerate() {
        let probe = p.replace('*', "x");
        assert_eq!(place(&probe).rank, i, "rule {p} is reached");
    }
}

#[test]
fn important_rows_first_and_the_rare_under_more() {
    for k in ["theme.mode", "theme.accent", "theme.scale", "theme.layout.statusbar", "snap.enabled", "ai.level", "gewohnheiten.enabled", "kernel.snd_output"] {
        assert!(!place(k).more, "{k} is shown at once");
    }
    for k in ["theme.dark.bg", "theme.glow", "theme.font.ui", "ai.timeout_s", "ai.mcp.ask_tools", "pkg.sources.core.keyring", "journal.max_mb", "context.index.workers", "snap.daily_days", "kernel.loglevel", "sofa.pad.map"] {
        assert!(place(k).more, "{k} is under Mehr");
    }
    assert!(!place("pkg.sources.core.servers").more, "a source's servers are its row in the list");
}

#[test]
fn tab_words_and_addresses() {
    for (i, t) in TABS.iter().enumerate() {
        assert_eq!(tab_word(t.word), Some(i));
        assert_eq!(address(i), format!("einstellungen/{}", t.word));
        assert_eq!(tab_of_path(&format!("{}/x", t.word)), Some(i));
    }
    assert_eq!(tab_word("ki-und-assistent"), Some(4), "the old title as a word");
    assert_eq!(tab_word("durchblick"), Some(4), "the old tab's word: Durchblick is the assistant's popup");
    assert_eq!(tab_word("Privatsphäre"), Some(5));
    assert_eq!(tab_word("Ton-und-Bild"), Some(2));
    assert_eq!(tab_of_path(""), None);
    assert_eq!(tab_of_path("kernel/snd_output"), None, "a key path names no tab");
    assert_eq!(tab_of_path("ki"), None, "einstellungen/ki stays the page KI");
}
