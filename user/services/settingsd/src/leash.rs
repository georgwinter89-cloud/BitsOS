//! How much the assistant may do without asking: one level (`ai.level`: answer | ask | auto, plan/spec/
//! assistent-eins K2), read by `assist` for every question.
//!
//! It is `auto` by default (DECISIONS 18.11.: everything built is on). Anyone may tighten it. Loosening it again
//! is Georg's own yes (`vouch.rs`: the settings page or the pill of the desktop, or root at the console), because
//! of what it guards: a model that may run commands as Georg could otherwise undo the level he chose and turn one
//! yes into a standing one. An undo that loosens is a change like any other.
//!
//! The same holds for `ai.mcp.ask_tools` (on by default) turned back on after Georg turned it off: it would let a
//! program at `assist --mcp` make BitsOS's own tools run without a question for good.
//!
//! And for the privacy rules of the context service ctxd (Georg 15.10.: an AI sees title, place and about two
//! sentences, never a private folder; nothing is read until he names folders): a folder more in
//! `context.roots`, one less in `context.private_roots`, a name less in `context.excludes`, a longer
//! `context.claude.snippet_chars`, `context.claude.allow_forget` or `context.enabled` turned on. The other
//! direction (a private folder more, a shorter snippet) is anyone's, and so is a private folder taken off the
//! list when no folder of `context.roots` lies in it or around it (it is then read by nobody).

use crate::value::Value;

/// The assistant's one level.
pub const LEVEL: &str = "ai.level";

/// How much a level holds back: the greater allows less. None: not a level.
fn strictness(level: &Value) -> Option<u8> {
    match level {
        Value::Text(t) => ["auto", "ask", "answer"].iter().position(|l| l == t).and_then(|i| u8::try_from(i).ok()),
        _ => None,
    }
}

/// True for a key whose loosening is Georg's own yes (some change of it may loosen).
pub fn leashed(key: &str) -> bool {
    let on = Value::Bool(true);
    CONTEXT.iter().any(|(k, _)| *k == key)
        || loosens(key, &Value::Text("answer".into()), &Value::Text("auto".into())).is_some()
        || loosens(key, &Value::Bool(false), &on).is_some()
}

/// ctxd's privacy keys and what loosens each (ctxd registers them, so they are not in `builtin.rs`).
#[derive(Clone, Copy)]
enum Loose {
    /// A list (`;` or a line break between entries) that gains an entry.
    Gains,
    /// A list that loses an entry.
    Loses,
    /// A number that grows.
    Grows,
    /// A switch turned on.
    On,
}

const CONTEXT: [(&str, Loose); 6] = [
    ("context.roots", Loose::Gains),
    ("context.private_roots", Loose::Loses),
    ("context.excludes", Loose::Loses),
    ("context.claude.snippet_chars", Loose::Grows),
    ("context.claude.allow_forget", Loose::On),
    ("context.enabled", Loose::On),
];

/// The entries of a list value, as ctxd splits them (`settings.rs` split_paths). A value of another type
/// is no list: nothing.
fn entries(v: &Value) -> Vec<&str> {
    match v {
        Value::Text(t) => t.split([';', '\n']).map(str::trim).filter(|e| !e.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// One folder in the other or the same (component-wise, so `/a/` is `/a` and `/ab` is not in `/a`).
fn overlap(a: &str, b: &str) -> bool {
    let (a, b) = (std::path::Path::new(a), std::path::Path::new(b));
    a.starts_with(b) || b.starts_with(a)
}

fn context_loosens(key: &str, how: Loose, old: &Value, new: &Value, now: &dyn Fn(&str) -> Option<Value>) -> bool {
    match how {
        Loose::Gains => {
            let was = entries(old);
            entries(new).iter().any(|e| !was.contains(e))
        }
        Loose::Loses => {
            let will = entries(new);
            let lost = entries(old).into_iter().filter(|e| !will.contains(e));
            if key != "context.private_roots" {
                return lost.count() > 0;
            }
            // Unknown roots: every private folder taken off counts.
            let roots = now("context.roots");
            let roots = roots.as_ref().map(entries);
            lost.into_iter().any(|e| roots.as_ref().is_none_or(|r| r.iter().any(|r| overlap(r, e))))
        }
        // Anything but a plain number is taken as no snippet: leaving it never loosens, reaching past it does.
        Loose::Grows => match (old, new) {
            (Value::Int(a), Value::Int(b)) => b > a,
            (_, Value::Int(b)) => *b > 0,
            _ => false,
        },
        Loose::On => *old != Value::Bool(true) && *new == Value::Bool(true),
    }
}

/// Some: `key` is one of these levels and the change from `old` to `new` allows more than before (the German
/// sentence of what would happen). Judged alone: as if every other key could make it matter.
pub fn loosens(key: &str, old: &Value, new: &Value) -> Option<String> {
    loosens_beside(key, old, new, &|_| None)
}

/// `loosens` with the other keys' values as they stand now (`now`: None for a key that is not there).
pub fn loosens_beside(key: &str, old: &Value, new: &Value, now: &dyn Fn(&str) -> Option<Value>) -> Option<String> {
    let sentence = || "Das erlaubt dem Assistenten mehr als bisher, ohne dich zu fragen.".to_string();
    if let Some((_, how)) = CONTEXT.iter().find(|(k, _)| *k == key) {
        return context_loosens(key, *how, old, new, now)
            .then(|| "Das zeigt einer KI mehr von deinen Dateien als bisher.".to_string());
    }
    if key == "ai.mcp.ask_tools" {
        // Anything but a plain false before counts as on: leaving it is never loosening.
        return (*old == Value::Bool(false) && *new != Value::Bool(false)).then(sentence);
    }
    if key != LEVEL {
        return None;
    }
    // A value that is no level cannot be stored (the key is a choice); should one ever stand there, it is
    // taken as the strictest, so leaving it never counts as loosening and reaching past it does.
    let (was, will) = (strictness(old).unwrap_or(2), strictness(new).unwrap_or(2));
    (will < was).then(sentence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_change_that_allows_more_loosens() {
        let v = |s: &str| Value::Text(s.into());
        for (old, new, loose) in [("ask", "auto", true), ("answer", "ask", true), ("answer", "auto", true), ("auto", "ask", false), ("ask", "answer", false), ("ask", "ask", false)] {
            assert_eq!(loosens(LEVEL, &v(old), &v(new)).is_some(), loose, "{old} -> {new}");
        }
        assert!(leashed(LEVEL));
        // The per-tool and per-kind levels of before EINS are no levels any more.
        for other in ["ai.helper.run.allow", "ai.tools.settings_set.allow", "ai.level.x", "theme.mode"] {
            assert!(loosens(other, &v("answer"), &v("auto")).is_none() && loosens(other, &v("never"), &v("always")).is_none(), "{other}");
        }
        let b = Value::Bool;
        assert!(loosens("ai.mcp.ask_tools", &b(false), &b(true)).is_some());
        assert!(loosens("ai.mcp.ask_tools", &b(true), &b(false)).is_none() && loosens("ai.mcp.ask_tools", &b(true), &b(true)).is_none());
        // A value that is no level counts as the strictest: leaving it never loosens, reaching past it does.
        assert!(loosens(LEVEL, &Value::Bool(true), &v("ask")).is_some() && loosens(LEVEL, &v("ask"), &Value::Int(0)).is_none());
    }

    #[test]
    fn the_context_services_privacy_only_georg_loosens() {
        let v = |s: &str| Value::Text(s.into());
        let (b, n) = (Value::Bool, Value::Int);
        let cases: &[(&str, Value, Value, bool)] = &[
            ("context.roots", v(""), v("/data/home/1000/Briefe"), true),
            ("context.roots", v("/a"), v("/a; /b"), true),
            ("context.roots", v("/a;/b"), v("/b\n/a"), false),
            ("context.roots", v("/a; /b"), v("/a"), false),
            ("context.private_roots", v("/a; /b"), v("/a"), true),
            ("context.private_roots", v("/a"), v(""), true),
            ("context.private_roots", v("/a"), v("/a;/b"), false),
            ("context.excludes", v(".ssh;*.kdbx;*.pem"), v(".ssh;*.pem"), true),
            ("context.excludes", v(".ssh"), v(".ssh;*.key"), false),
            ("context.claude.snippet_chars", n(200), n(1000), true),
            ("context.claude.snippet_chars", n(200), n(0), false),
            ("context.claude.snippet_chars", n(200), n(200), false),
            ("context.claude.allow_forget", b(false), b(true), true),
            ("context.claude.allow_forget", b(true), b(false), false),
            ("context.enabled", b(false), b(true), true),
            ("context.enabled", b(true), b(false), false),
        ];
        for (k, old, new, loose) in cases {
            assert_eq!(loosens(k, old, new).is_some(), *loose, "{k}: {old:?} -> {new:?}");
            assert!(leashed(k), "{k}");
        }
        for free in ["context.embedder.kind", "context.index.workers", "context.index.watch"] {
            assert!(!leashed(free), "{free}");
        }
        // A private folder off the list is free while no folder an AI sees lies in it or around it.
        let roots = |r: &'static str| move |k: &str| (k == "context.roots").then(|| v(r));
        let p = "context.private_roots";
        assert!(loosens_beside(p, &v("/d/Privat"), &v(""), &roots("/d/Dokumente")).is_none());
        assert!(loosens_beside(p, &v("/d/Privat"), &v(""), &roots("")).is_none());
        assert!(loosens_beside(p, &v("/d/Privat"), &v(""), &roots("/d/Privatsachen")).is_none(), "a name that only starts alike");
        assert!(loosens_beside(p, &v("/d/Privat/"), &v(""), &roots("/d/Privat")).is_some(), "the same folder");
        assert!(loosens_beside(p, &v("/d/Privat"), &v(""), &roots("/d")).is_some(), "a root around it");
        assert!(loosens_beside(p, &v("/d"), &v(""), &roots("/d/Briefe")).is_some(), "a root in it");
    }
}
