//! EINS-B (PLAN K2, K4, K8): the one level of the assistant and what it sees, as the views read them. `ai.level`
//! is the engine's key (wave A registers it); bitscomp registers `assistant.sidebar_sees` and
//! `assistant.never_see` (its views apply them). Pure: values in, words and decisions out.

use mcpreg::Json;

use crate::settings::Values;

pub const LEVEL: &str = "ai.level";
pub const SIDEBAR_SEES: &str = "assistant.sidebar_sees";
pub const NEVER_SEE: &str = "assistant.never_see";

/// The view's sentence for a window of a program on the never-see list.
pub const NOT_SEEN: &str = "Dieses Programm sieht der Assistent nicht";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Answer,
    Ask,
    Auto,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Answer, Level::Ask, Level::Auto];

    /// `ask` when the value is missing or no level (settingsd unreachable): the view then never claims more than
    /// the assistant would do, as assist's config.rs falls back. The system's default is `auto` (settingsd).
    pub fn of(v: &Values) -> Level {
        Level::of_word(v.text(LEVEL).unwrap_or("ask")).unwrap_or(Level::Ask)
    }

    pub fn of_word(w: &str) -> Option<Level> {
        Level::ALL.into_iter().find(|l| l.word() == w.trim())
    }

    pub fn word(self) -> &'static str {
        match self {
            Level::Answer => "answer",
            Level::Ask => "ask",
            Level::Auto => "auto",
        }
    }

    /// The pill's and the page's name (board 4).
    pub fn name(self) -> &'static str {
        match self {
            Level::Answer => "Nur antworten",
            Level::Ask => "Fragt vorher",
            Level::Auto => "Automodus",
        }
    }

    /// The plain sentence under the name on the page (board 4).
    pub fn sentence(self) -> &'static str {
        match self {
            Level::Answer => "Er sieht und erklärt, ändert aber nichts.",
            Level::Ask => "Er zeigt jeden Eingriff und wartet auf dein Ja.",
            Level::Auto => "Er macht alles ohne Nachfrage: Befehle, Skripte, Dateien, Programme bedienen, Internet.",
        }
    }
}

/// Whether going from `old` to `new` gives the assistant more freedom (then only Georg's own yes counts).
pub fn loosens(old: &str, new: &str) -> bool {
    match (Level::of_word(old).or(Some(Level::Ask)), Level::of_word(new)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

/// "Gilt immer, in jeder Stufe" (PLAN K2 F1-F4, board 4): not switchable.
pub const FIXED: [&str; 4] = [
    "Passwörter und Bezahldaten tippt er nie.",
    "Vor Kaufen, Bezahlen, Senden und Löschen fragt er.",
    "Geschützte Einstellungen änderst nur du selbst.",
    "Dateien löscht er nie ohne dein Ja. Bis dahin liegen sie im Papierkorb.",
];
pub const FIXED_END: &str = "Diese Regeln lassen sich nicht abschalten. Alles andere ist im Automodus frei.";
/// PLAN K2's honest limit, on the page under the rules.
pub const HONEST: &str = "Was ein Befehl selbst ins Netz schickt, kann BitsOS nicht prüfen; das Fragen vor dem Senden gilt für Schritte auf dem Bildschirm und bekannte Werkzeuge.";

/// A program's word as the never-see list holds it: lower case, trimmed.
pub fn program_word(app: &str) -> String {
    app.trim().to_lowercase()
}

/// The never-see list (program words).
pub fn never_list(v: &Values) -> Vec<String> {
    let mut l: Vec<String> = v.list(NEVER_SEE).unwrap_or_default().iter().map(|w| program_word(w)).filter(|w| !w.is_empty()).collect();
    l.dedup();
    l
}

/// Whether the assistant never sees windows of `app` (its app id or its name: either may be on the list).
pub fn never_sees(v: &Values, app: &str) -> bool {
    let a = program_word(app);
    if a.is_empty() {
        return false;
    }
    // "org.mozilla.firefox" is matched by "firefox" too: the list holds what Georg picked or typed.
    never_list(v).iter().any(|w| *w == a || a.rsplit(['.', '/']).next() == Some(w.as_str()))
}

/// The list with `app` put on it or taken off.
pub fn toggled(v: &Values, app: &str) -> String {
    let a = program_word(app);
    let mut l = never_list(v);
    if let Some(i) = l.iter().position(|w| *w == a) {
        l.remove(i);
    } else if !a.is_empty() {
        l.push(a);
    }
    l.join(", ")
}

pub fn sidebar_sees(v: &Values) -> bool {
    v.flag(SIDEBAR_SEES).unwrap_or(true)
}

fn def(key: &str, schema: Json, default: Json, de: &str, en: &str) -> Json {
    Json::obj(vec![
        ("key", Json::str(key)),
        ("schema", schema),
        ("default", default),
        ("text", Json::obj(vec![("de", Json::str(de)), ("en", Json::str(en))])),
        ("restart", Json::str("none")),
        ("safety", Json::str("normal")),
    ])
}

/// bitscomp's keys of the assistant's views (owner bitscomp).
pub fn defs() -> Vec<Json> {
    vec![
        def(
            SIDEBAR_SEES,
            Json::obj(vec![("type", Json::str("bool"))]),
            // On by default (DECISIONS 18.11.); settingsd's `defaults_on` lifts a stored old default.
            Json::Bool(true),
            "In der Seitenleiste das Fenster vorne mitsehen",
            "In the sidebar, the assistant sees the window in front with each question",
        ),
        def(
            NEVER_SEE,
            Json::obj(vec![("type", Json::str("text")), ("max_bytes", Json::Num(4096.0))]),
            Json::str(""),
            "Bei diesen Programmen sieht der Assistent nie etwas",
            "Programs whose windows the assistant never sees (program words, separated by commas)",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_and_its_default() {
        let mut v = Values::default();
        assert_eq!(Level::of(&v), Level::Ask, "Fragt vorher by default");
        v.set(LEVEL, "auto");
        assert_eq!(Level::of(&v), Level::Auto);
        v.set(LEVEL, "irgendwas");
        assert_eq!(Level::of(&v), Level::Ask);
        assert!(loosens("ask", "auto") && loosens("answer", "ask") && loosens("", "auto"));
        assert!(!loosens("auto", "ask") && !loosens("ask", "answer") && !loosens("ask", "ask") && !loosens("ask", "x"));
    }

    #[test]
    fn never_see_matches_the_program_word_and_toggles() {
        let mut v = Values::default();
        assert!(!never_sees(&v, "brave"));
        v.set(NEVER_SEE, "KeePassXC, banking ");
        assert!(never_sees(&v, "keepassxc") && never_sees(&v, "org.example.Banking") && !never_sees(&v, "brave") && !never_sees(&v, ""));
        assert_eq!(toggled(&v, "Brave"), "keepassxc, banking, brave");
        assert_eq!(toggled(&v, "banking"), "keepassxc");
    }

    #[test]
    fn the_keys_are_definitions() {
        for d in defs() {
            assert!(d.get("key").and_then(Json::as_str).is_some_and(|k| k.starts_with("assistant.")));
            let schema = d.get("schema").unwrap();
            if schema.get("type").and_then(Json::as_str) == Some("text") {
                assert!(schema.get("max_bytes").and_then(Json::as_u64).is_some(), "settingsd wants max_bytes for a text");
            }
        }
        assert_eq!(FIXED.len(), 4);
    }
}
