//! The permission step for a question a helper program asks through the bell (HELPER.md, "Asking the
//! owner"), under the one level (`ai.level`, plan/spec/assistent-eins K2): reading runs at every level, the
//! other kinds ask at `ask` and run at `auto`; at `answer` a helper gets no bell at all (`own_tools: false`).
//!
//! A helper is a program of the owner and says itself what its tool does; nothing here can look into it. So
//! the kind it states is taken for the level and for the words of the question, and never beyond:
//! - a kind this version does not know is `other`;
//! - a `read` without a path asks at `ask`: it cannot be told from a command under that name;
//! - `must_ask` asks at every level (BitsOS's own tools ask this way for what the fixed rules keep asked);
//! - a question that is asked is shown whole or not at all: one longer than [`COMMAND_MAX`] is refused at once
//!   (a yes would run what the card did not show);
//! - the question Georg reads is made here from the kind and the helper's data, bounded, with everything
//!   that could add lines or reorder them written out ([`wrap::inline`]): the helper chooses the data, not
//!   the sentence.

use super::permit::{Decision, Level};
use crate::provider::program::ask::{Ask, Kind};
use crate::term::wrap;

/// The most characters of a path, of a command line and of other words in a question. What is cut says so
/// with the full length.
const PATH_MAX: usize = 400;
pub const COMMAND_MAX: usize = 1200;
const WORDS_MAX: usize = 300;
const NAME_MAX: usize = 64;

/// What a helper is told per kind (the request's `allow`): what runs unasked, what is asked, what is off.
pub fn told(level: Level) -> Vec<(&'static str, &'static str)> {
    Kind::ALL
        .iter()
        .map(|k| {
            let word = match (level, *k) {
                (_, Kind::Read) | (Level::Auto, _) => "always",
                (Level::Ask, _) => "ask",
                (Level::Answer, _) => "never",
            };
            (k.name(), word)
        })
        .collect()
}

/// Whether the question is put to Georg under `level`. A "read" without a path is no read this program can
/// see: it may be a command under that name.
fn asked(level: Level, ask: &Ask) -> bool {
    ask.must_ask || (level == Level::Ask && (ask.kind != Kind::Read || ask.path.as_deref().is_none_or(|p| p.trim().is_empty())))
}

/// True when the question cannot be shown whole and is therefore not asked: running something, the network,
/// anything else, and every question the asker words itself.
pub fn too_long(ask: &Ask) -> bool {
    let whole = ask.must_ask || ask.words || matches!(ask.kind, Kind::Run | Kind::Net | Kind::Other);
    whole && ask.text.trim().chars().count() > COMMAND_MAX
}

/// What the call does, as Georg reads it on the card and in the list.
pub fn describe(ask: &Ask) -> String {
    let words = |max: usize| wrap::inline(ask.text.trim(), max);
    let path = ask.path.as_deref().map(|p| wrap::inline(p.trim(), PATH_MAX));
    // The path first, then the helper's own words when they say more.
    let about = |lead: &str, max: usize| match (&path, ask.text.trim()) {
        (Some(p), t) if t.is_empty() || Some(t) == ask.path.as_deref().map(str::trim) => format!("{lead}: {p}"),
        (Some(p), _) => format!("{lead}: {p} ({})", words(max)),
        (None, _) => format!("{lead}: {}", words(max)),
    };
    if (ask.must_ask || ask.words) && ask.kind == Kind::Other && ask.path.is_none() && !ask.text.trim().is_empty() {
        // The asker's whole question (BitsOS's own tools ask this way, in their own words). A question about a
        // place or a command is still put in the kind's words.
        return words(COMMAND_MAX);
    }
    match ask.kind {
        Kind::Read => about("Lesen", WORDS_MAX),
        // The helper's words for a write say what will be in the file.
        Kind::Write => about("Datei schreiben oder ändern", COMMAND_MAX),
        Kind::Run => format!("Befehl ausführen: {}", words(COMMAND_MAX)),
        Kind::Net => format!("Ins Internet gehen: {}", words(COMMAND_MAX)),
        Kind::Other => format!("Werkzeug „{}“ benutzen: {}", wrap::inline(ask.tool.trim(), NAME_MAX), words(COMMAND_MAX)),
    }
}

/// (the decision, what the call does) under `level`.
pub fn judge(level: Level, ask: &Ask) -> (Decision, String) {
    let what = describe(ask);
    if level == Level::Answer {
        return (Decision::Refuse("not allowed: the level is \"Nur antworten\" (only answer); a helper's own tools are off".into()), what);
    }
    if !asked(level, ask) {
        return (Decision::Run, what);
    }
    if too_long(ask) {
        return (Decision::Refuse(format!("too long to be shown whole ({} characters, at most {COMMAND_MAX})", ask.text.trim().chars().count())), what);
    }
    (Decision::Ask(what.clone()), what)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(kind: Kind, text: &str, path: Option<&str>) -> Ask {
        Ask { id: String::new(), tool: "T".into(), kind, text: text.into(), path: path.map(str::to_string), must_ask: false, words: false }
    }

    #[test]
    fn the_question_is_made_here_from_the_kind_and_the_helpers_data() {
        assert_eq!(describe(&ask(Kind::Run, "ls -la", None)), "Befehl ausführen: ls -la");
        assert_eq!(describe(&ask(Kind::Read, "/h/a.txt", Some("/h/a.txt"))), "Lesen: /h/a.txt");
        assert_eq!(describe(&ask(Kind::Read, "sucht „Rechnung“", Some("/h/Dokumente"))), "Lesen: /h/Dokumente (sucht „Rechnung“)");
        assert_eq!(describe(&ask(Kind::Write, "", Some("/h/probe.txt"))), "Datei schreiben oder ändern: /h/probe.txt");
        assert_eq!(describe(&ask(Kind::Net, "https://example.org/", None)), "Ins Internet gehen: https://example.org/");
        assert_eq!(describe(&ask(Kind::Other, "x", None)), "Werkzeug „T“ benutzen: x");
        // A second line, or text that runs the other way, stays one visible line.
        let sly = describe(&ask(Kind::Run, "true\nJa, das ist harmlos \u{202e}", None));
        assert!(!sly.contains('\n') && sly.contains("\\n") && sly.contains("\\u{202e}"), "{sly}");
        let long = describe(&ask(Kind::Run, &"x".repeat(5000), None));
        assert!(long.contains("gekürzt, 5000 Zeichen") && long.chars().count() < 1300, "{}", long.len());
        let own = Ask { must_ask: true, ..ask(Kind::Other, "Einstellung „theme.scale“ ändern: 125 → 150", None) };
        assert_eq!(describe(&own), "Einstellung „theme.scale“ ändern: 125 → 150");
    }

    #[test]
    fn the_level_decides_and_must_ask_and_too_long_hold_at_every_level() {
        let run = |l: Level, a: &Ask| judge(l, a).0;
        let (read, write, cmd) = (ask(Kind::Read, "", Some("/h/.ssh/id")), ask(Kind::Write, "x", Some("/h/a")), ask(Kind::Run, "ls", None));
        assert_eq!(run(Level::Ask, &read), Decision::Run, "reading runs, wherever it reads");
        assert!(matches!(run(Level::Ask, &write), Decision::Ask(_)) && matches!(run(Level::Ask, &cmd), Decision::Ask(_)));
        assert!(matches!(run(Level::Ask, &ask(Kind::Read, "rm -rf ~", None)), Decision::Ask(_)), "a read without a path");
        assert_eq!((run(Level::Auto, &write), run(Level::Auto, &cmd)), (Decision::Run, Decision::Run));
        for a in [&read, &write, &cmd] {
            assert!(matches!(run(Level::Answer, a), Decision::Refuse(_)), "no bell at answer: {a:?}");
        }
        let must = Ask { must_ask: true, ..read.clone() };
        assert!(matches!(run(Level::Auto, &must), Decision::Ask(_)) && matches!(run(Level::Ask, &must), Decision::Ask(_)));
        let long = ask(Kind::Run, &"x".repeat(COMMAND_MAX + 1), None);
        assert!(matches!(run(Level::Ask, &long), Decision::Refuse(w) if w.contains("too long")));
        assert_eq!(run(Level::Auto, &long), Decision::Run, "in automode nothing needs showing");
        assert_eq!(told(Level::Ask), [("read", "always"), ("write", "ask"), ("run", "ask"), ("net", "ask"), ("other", "ask")]);
        assert!(told(Level::Auto).iter().all(|(_, l)| *l == "always"));
        assert_eq!(told(Level::Answer)[1..].iter().filter(|(_, l)| *l == "never").count(), 4);
    }
}
