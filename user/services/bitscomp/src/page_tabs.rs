//! WY-SET: the settings in nine tabs (Georg 14.11.: "übersichtlicher, in Tabs, zusammengefasst und vereinfacht").
//! Every key has exactly one place: a card of a tab, shown at once or under the card's collapsed "Mehr". The place
//! comes from the rules below (first match wins, a rule's position is the row's order in its card); a key no rule
//! names lands in System under "Weitere", so a new namespace is never lost. Pure: the page builds its view from it
//! (`page_view`), the addresses `einstellungen/<tab>` name the tabs.

/// A tab of the rail: its address word, its name and the line under its title.
pub struct Tab {
    pub word: &'static str,
    pub title: &'static str,
    pub sub: &'static str,
}

pub const TABS: [Tab; 9] = [
    Tab { word: "aussehen", title: "Aussehen", sub: "Wie BitsOS aussieht: Design, Farbe, Größe und Schrift." },
    Tab { word: "schreibtisch", title: "Schreibtisch", sub: "Wie der Bildschirm aufgebaut ist: Leisten, Favoriten, Arbeitsräume und der Sofa-Modus." },
    Tab { word: "ton", title: "Ton und Bild", sub: "Wo der Ton spielt und wie das Bild auf den Fernseher passt." },
    Tab { word: "netz", title: "Netz und WLAN", sub: "Wie dieser Rechner mit dem Netz verbunden ist." },
    Tab { word: "assistent", title: "Assistent", sub: "Ein Assistent, in der Seitenleiste und mit Win+F1. Was hier steht, gilt für beide." },
    Tab { word: "privatsphaere", title: "Privatsphäre", sub: "Was BitsOS über dich gelernt hat, wer was benutzt hat, und der Kontextdienst." },
    Tab { word: "programme", title: "Programme", sub: "Wie Programme installiert und aktualisiert werden, und woher sie kommen." },
    Tab { word: "sicherungen", title: "Sicherungen", sub: "Wie oft /data gesichert wird und wie lange Sicherungen bleiben." },
    Tab { word: "system", title: "System", sub: "Kern, Journal, Leistung und der Einstellungsdienst." },
];

/// The key of a card's "Mehr" row (`mehr.<card>`): no key of settingsd, it names the row for `ctl hot` and DB1.
pub const FOLD: &str = "mehr.";
/// Rows the page makes itself (consolidations, provider cards, the network's state): never keys of settingsd.
pub const VIRTUAL: &str = "set.";

/// A card: its tab, its title (empty: none, the card is only its "Mehr") and the text of its "Mehr".
pub struct Card {
    pub id: &'static str,
    pub tab: usize,
    pub title: &'static str,
    pub fold: &'static str,
}

const fn c(id: &'static str, tab: usize, title: &'static str, fold: &'static str) -> Card {
    Card { id, tab, title, fold }
}

/// The cards in the order the page shows them.
pub const CARDS: [Card; 30] = [
    c("design", 0, "Design", "Mehr: Uhrzeiten für „Nach Uhrzeit“"),
    c("akzent", 0, "Akzentfarbe", "Mehr"),
    c("groesse", 0, "Größe", "Mehr: Mauszeiger in Pixeln"),
    c("schrift", 0, "Schrift und Bewegung", "Mehr: einzelne Schriften"),
    c("fein", 0, "", "Mehr: Ecken, Abstände, Leuchten, Rasterlinien"),
    c("farben", 0, "", "Eigene Farben für hell und dunkel"),
    c("aufbau", 1, "Aufbau", "Mehr"),
    c(FAV_CARD, 1, "Favoriten", "Mehr"),
    c("absicht", 1, "Absicht der Arbeitsräume", "Mehr"),
    c("sofa", 1, "Sofa-Modus", "Mehr: Gamepad"),
    c("desk", 1, "Weitere", "Mehr"),
    c("ton", 2, "Ton", "Mehr"),
    c("bild", 2, "Bild", "Mehr"),
    c("verbindung", 3, "Verbindung", "Mehr"),
    // EINS-B (board 4): one assistant, one level, four fixed rules, what it sees, which AI.
    c("darf", 4, "Wie viel er darf", "Mehr"),
    c("gilt", 4, "Gilt immer, in jeder Stufe", "Mehr"),
    c("sieht", 4, "Was er sieht", "Mehr: fremde Programme lesen"),
    c("welche", 4, "Welche KI", "Mehr"),
    c("kimehr", 4, "", "Mehr: weitere Anbieter, Ersatz, Antwortlänge, Tastenkürzel (Win+F1)"),
    c("gelernt", 5, "Was BitsOS über dich gelernt hat", "Mehr: was BitsOS lernt"),
    c("access", 5, "Wer hat was gemacht", "Mehr: Aufbewahrung und Ausnahmen"),
    c("kontext", 5, "Kontextdienst", "Mehr: Ausnahmen, Grenzen und Suchmodell"),
    c("programme", 6, "Programme", "Mehr: Prozessor und Platz"),
    c("quellen", 6, "Quellen", "Mehr: Reihenfolge, Schlüssel und Signaturen"),
    c("snap", 7, "Sicherungen", "Mehr: jede Aufbewahrung einzeln"),
    c("kern", 8, "Kern", "Mehr: Konsole, Protokoll, Vorladen"),
    c("journal", 8, "Journal", "Mehr"),
    c("leistung", 8, "Leistung", "Mehr"),
    c("dienst", 8, "Einstellungsdienst", "Mehr"),
    c(OTHER_CARD, 8, "Weitere", "Mehr"),
];

pub const FAV_CARD: &str = "favoriten";
pub const OTHER_CARD: &str = "weitere";

/// (pattern, card, under "Mehr"). A pattern is a key, or has one `*` that stands for one or more characters.
const RULES: &[(&str, &str, bool)] = &[
    ("theme.mode", "design", false),
    ("theme.auto.light_from", "design", true),
    ("theme.auto.dark_from", "design", true),
    ("theme.auto.*", "design", true),
    ("theme.accent", "akzent", false),
    ("theme.scale", "groesse", false),
    ("theme.font.size", "groesse", false),
    ("set.zeiger", "groesse", false),
    ("theme.cursor.size", "groesse", true),
    ("set.schriften", "schrift", false),
    ("theme.motion", "schrift", false),
    ("theme.font.*", "schrift", true),
    ("theme.dark.*", "farben", true),
    ("theme.light.*", "farben", true),
    ("theme.layout.preset", "aufbau", false),
    ("theme.layout.address_bar", "aufbau", false),
    ("theme.layout.favourites_bar", "aufbau", false),
    ("theme.layout.workspaces_bar", "aufbau", false),
    ("theme.layout.claude_sidebar", "aufbau", false),
    ("theme.layout.statusbar", "aufbau", false),
    ("theme.layout.*", "aufbau", true),
    ("theme.*", "fein", true),
    ("desktop.favourites", FAV_CARD, false),
    ("desktop.absicht.*", "absicht", false),
    ("kernel.snd_output", "ton", false),
    ("sofa.start_volume", "ton", false),
    ("sofa.overscan", "bild", false),
    ("sofa.start_on_pad", "sofa", false),
    ("sofa.sort", "sofa", false),
    ("sofa.motion", "sofa", false),
    ("sofa.*", "sofa", true),
    ("set.netz.*", "verbindung", false),
    ("net.*", "verbindung", false),
    ("ai.level", "darf", false),
    ("set.gilt.*", "gilt", false),
    ("assistant.never_see", "sieht", false),
    ("assistant.sidebar_sees", "sieht", false),
    ("desktop.durchblick.programs", "sieht", true),
    ("set.anbieter.+", "kimehr", true),
    ("set.anbieter.*", "welche", false),
    ("ai.role.chat", "welche", false),
    ("ai.cloud_notice", "welche", false),
    ("ai.role.chat_fallback", "kimehr", true),
    ("ai.local_only", "kimehr", true),
    ("ai.providers", "kimehr", true),
    ("ai.provider.*", "kimehr", true),
    ("ai.assistant.name", "kimehr", true),
    ("durchblick.taste", "kimehr", true),
    ("durchblick.pad", "kimehr", true),
    ("ai.*", "kimehr", true),
    ("assistant.*", "kimehr", true),
    ("durchblick.*", "kimehr", true),
    ("context.claude.*", "kimehr", true),
    ("gewohnheiten.enabled", "gelernt", false),
    ("gelernt.*", "gelernt", false),
    ("gewohnheiten.*", "gelernt", true),
    ("access.enabled", "access", false),
    ("access.ansehen", "access", false),
    ("access.*", "access", true),
    ("context.enabled", "kontext", false),
    ("context.roots", "kontext", false),
    ("context.private_roots", "kontext", false),
    ("context.*", "kontext", true),
    ("pkg.updates.check", "programme", false),
    ("pkg.updates.install", "programme", false),
    ("pkg.optional.ask", "programme", false),
    ("pkg.hinzufuegen", "programme", false),
    ("pkg.sources.*.servers", "quellen", false),
    ("pkg.sources.*", "quellen", true),
    ("pkg.*", "programme", true),
    ("snap.enabled", "snap", false),
    ("snap.interval_min", "snap", false),
    ("set.sicherung.*", "snap", false),
    ("snap.*", "snap", true),
    // GP-CPU2: the processor's energy preference sits with the measurements that judge it (Gaming-PC only).
    ("kernel.cpu_policy", "leistung", false),
    ("kernel.preload.enabled", "kern", false),
    ("kernel.*", "kern", true),
    ("journal.enabled", "journal", false),
    ("journal.*", "journal", true),
    ("leistung.*", "leistung", false),
    ("settings.language", "dienst", false),
    ("settings.*", "dienst", true),
    // Last: Durchblick's switch under `desktop` must find its own rule first.
    ("desktop.*", "desk", false),
];

/// PLAN K8: keys the one assistant took away. A settingsd or an engine from before still registering them must not
/// put them back on the page (the desk merges the waves one after another; the page is the same either way).
const GONE: &[&str] = &[
    "ai.tools.*",
    "ai.helper.*",
    "ai.role.explain",
    "ai.role.explain_fallback",
    "ai.role.search",
    "ai.role.search_fallback",
    "ai.role.jobs",
    "ai.role.jobs_fallback",
    "durchblick.antwort",
    "durchblick.vorschau.*",
    "durchblick.bild.auto.*",
    "durchblick.handeln.*",
];

/// Whether the page never shows `key` (PLAN K8: removed).
pub fn gone(key: &str) -> bool {
    GONE.iter().any(|p| matches(p, key))
}

/// Plain short names for the rows shown at once (settingsd's description then stands under the name, as the
/// mock-ups' rows read: "Nur KI auf diesem Rechner", "Gespräch").
const LABELS: &[(&str, &str)] = &[
    ("theme.mode", "Hell oder dunkel"),
    ("theme.accent", "Akzentfarbe"),
    ("theme.scale", "Oberfläche"),
    ("theme.font.size", "Schrift"),
    ("theme.motion", "Bewegung"),
    ("theme.layout.preset", "Aufbau des Bildschirms"),
    ("theme.layout.address_bar", "Adresszeile"),
    ("theme.layout.favourites_bar", "Favoritenleiste"),
    ("theme.layout.workspaces_bar", "Liste der Arbeitsräume"),
    ("theme.layout.claude_sidebar", "Seitenleiste des Assistenten"),
    ("theme.layout.statusbar", "Statusleiste"),
    ("sofa.start_on_pad", "Mit dem Gamepad starten"),
    ("sofa.sort", "Sortierung der Bibliothek"),
    ("sofa.motion", "Kacheln heben sich"),
    ("kernel.snd_output", "Ton spielt über"),
    ("sofa.start_volume", "Lautstärke beim Start"),
    ("sofa.overscan", "Bildausschnitt für Fernseher"),
    ("ai.local_only", "Nur KI auf diesem Rechner"),
    ("ai.cloud_notice", "Zeigen, wenn etwas in die Cloud geht"),
    ("ai.role.chat", "Antwortet für alles"),
    ("ai.role.chat_fallback", "Ersatz, wenn er nicht erreichbar ist"),
    ("ai.level", "Stufe"),
    ("assistant.never_see", "Bei diesen Programmen nie"),
    ("assistant.sidebar_sees", "In der Seitenleiste das Fenster vorne mitsehen"),
    ("durchblick.taste", "Tastenkürzel für das Popup"),
    ("durchblick.pad", "Gamepad"),
    ("desktop.durchblick.programs", "Fremde Programme lesen"),
    ("gewohnheiten.enabled", "Lernen und Nachmachen"),
    ("access.enabled", "Aufschreiben, wer was benutzt"),
    ("context.enabled", "Kontextdienst"),
    ("context.roots", "Ordner, die er durchsucht"),
    ("context.private_roots", "Private Ordner"),
    ("pkg.updates.check", "Nach Aktualisierungen schauen"),
    ("pkg.updates.install", "Gefundene Aktualisierungen"),
    ("pkg.optional.ask", "Zusätze zur Auswahl zeigen"),
    ("kernel.preload.enabled", "Programme vorladen"),
    ("journal.enabled", "Journal"),
    ("leistung.nachts", "Nachts messen"),
    ("kernel.cpu_policy", "Prozessor-Abstimmung"),
];

/// The plain short name of a row the tabs show at once, if it has one.
pub fn label_of(key: &str) -> Option<&'static str> {
    LABELS.iter().find(|(k, _)| *k == key).map(|(_, l)| *l)
}

fn matches(pattern: &str, key: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == key,
        Some((pre, post)) => key.len() > pre.len() + post.len() && key.starts_with(pre) && key.ends_with(post),
    }
}

/// Where a key is shown: its card (index into CARDS), whether under "Mehr", and its order in the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub card: usize,
    pub more: bool,
    pub rank: usize,
}

pub fn card_index(id: &str) -> usize {
    CARDS.iter().position(|c| c.id == id).unwrap_or(CARDS.len() - 1)
}

pub fn place(key: &str) -> Place {
    match RULES.iter().enumerate().find(|(_, (p, _, _))| matches(p, key)) {
        Some((rank, (_, card, more))) => Place { card: card_index(card), more: *more, rank },
        None => Place { card: card_index(OTHER_CARD), more: false, rank: RULES.len() },
    }
}

pub fn tab_of(key: &str) -> usize {
    CARDS[place(key).card].tab
}

/// The tab of an address word (`schreibtisch`), also its title in lower case without spaces.
pub fn tab_word(w: &str) -> Option<usize> {
    let w = w.trim().to_lowercase();
    // The tabs' old words: Durchblick's keys are the assistant's now, its other cards are Privatsphäre.
    match w.as_str() {
        "durchblick" | "ki-und-assistent" => return Some(4),
        "privatsphäre" | "durchblick-und-privatsphäre" => return Some(5),
        _ => {}
    }
    TABS.iter().position(|t| t.word == w || t.title.to_lowercase().replace(' ', "-") == w)
}

/// The address of tab `i`.
pub fn address(i: usize) -> String {
    format!("einstellungen/{}", TABS.get(i).map_or("", |t| t.word))
}

/// The tab named by a settings path (after `einstellungen/`), when its first word is a tab's.
pub fn tab_of_path(path: &str) -> Option<usize> {
    tab_word(path.split(['/', '.']).find(|s| !s.is_empty())?)
}

#[cfg(test)]
#[path = "page_tabs_tests.rs"]
pub(crate) mod tests;
