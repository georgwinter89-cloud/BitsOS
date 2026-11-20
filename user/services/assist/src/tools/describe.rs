//! What a tool call is about to do, in plain German and on one line: the text of the question Georg answers
//! with j or n, and of the note when a call runs without asking. Names and values in it come from the model,
//! so each is bounded and shown without line breaks or invisible characters (`wrap::inline`): text that
//! reached the model through a tool result or a page must not be able to add lines to the question, push the
//! real change off the screen or dress a harmful change up as a harmless one.

use super::ToolSpec;
use super::permit::own_leash;
use crate::json::Json;
use crate::settings::Settings;
use crate::term::wrap;

/// The most characters of one name or value in a question; the rest is cut and said so (`wrap::inline`).
const SHOWN: usize = 200;
/// The most changes one undo question lists one by one.
const LISTED: usize = 8;

/// A name or text the model chose, as one bounded line.
pub(super) fn plain(s: &str) -> String {
    wrap::inline(s, SHOWN)
}

fn show(v: Option<&Json>) -> String {
    match v {
        None | Some(Json::Null) => "–".into(),
        Some(Json::Str(s)) => format!("„{}“", plain(s)),
        Some(Json::Bool(true)) => "an".into(),
        Some(Json::Bool(false)) => "aus".into(),
        Some(v) => plain(&v.to_text()),
    }
}

/// One change `settings_undo` would take back.
pub(super) struct Target {
    /// Its journal number.
    pub seq: Option<u64>,
    pub key: String,
    now: Option<Json>,
    after: Option<Json>,
}

/// The changes `settings_undo` with `n` would take back, newest first, as settingsd picks them: not an undo
/// itself and not undone yet. Err when the journal could not be read.
pub(super) fn undo_targets(settings: &dyn Settings, n: usize) -> Result<Vec<Target>, String> {
    // The journal is asked for more than n: undos and entries already undone do not count.
    let mut want = n.max(1);
    loop {
        let a = settings.request(&Json::obj().with("op", "history").with("n", want)).map_err(|e| e.to_string())?;
        let Some(Json::Arr(entries)) = a.get("entries") else { return Err("history without entries".into()) };
        let out: Vec<Target> = entries
            .iter()
            .filter(|e| e.str("op") != Some("undo") && e.get("undone").and_then(Json::as_bool) != Some(true))
            .take(n)
            .map(|e| Target {
                seq: e.get("seq").and_then(Json::as_i64).and_then(|s| u64::try_from(s).ok()),
                key: e.str("key").unwrap_or("?").to_string(),
                now: e.get("new").cloned(),
                after: e.get("old").cloned(),
            })
            .collect();
        if out.len() >= n || entries.len() < want {
            return Ok(out);
        }
        want = want.saturating_mul(2);
    }
}

pub(super) fn undo_count(args: &Json) -> usize {
    args.get("n").and_then(Json::as_i64).and_then(|n| usize::try_from(n).ok()).unwrap_or(1)
}

pub(super) fn describe_undo(targets: &[Target]) -> String {
    let mut list: Vec<String> =
        targets.iter().take(LISTED).map(|t| format!("„{}“: {} → {}", plain(&t.key), show(t.now.as_ref()), show(t.after.as_ref()))).collect();
    if let Some(rest) = targets.get(LISTED..).filter(|r| !r.is_empty()) {
        let own = if rest.iter().any(|t| own_leash(&t.key)) { ", darunter Einstellungen des Assistenten (ai.*)" } else { "" };
        list.push(format!("und {} weitere{own}", rest.len()));
    }
    format!("Änderung zurücknehmen: {}", list.join("; "))
}

/// What a call is about to do, in plain German and on one line, for the question or the note in the chat.
/// Names and values come from the model: each is bounded and shown without line breaks.
pub fn describe(spec: &ToolSpec, args: &Json, settings: &dyn Settings) -> String {
    match spec.name.as_str() {
        "settings_set" => {
            let key = args.str("key").unwrap_or("?");
            let old = crate::settings::get(settings, key).ok();
            format!("Einstellung „{}“ ändern: {} → {}", plain(key), show(old.as_ref()), show(args.get("value")))
        }
        "settings_undo" => match undo_targets(settings, undo_count(args)) {
            Ok(t) if !t.is_empty() => describe_undo(&t),
            _ => UNDO_UNSEEN.into(),
        },
        "settings_get" | "settings_describe" => format!("Einstellung „{}“ lesen", plain(args.str("key").unwrap_or("?"))),
        "settings_list" => match args.str("prefix").filter(|p| !p.is_empty()) {
            Some(p) => format!("Einstellungen der Gruppe „{}“ auflisten", plain(p)),
            None => "Alle Einstellungen auflisten".into(),
        },
        "settings_history" => "Die letzten Änderungen an den Einstellungen lesen".into(),
        // The desktop's own tool (bitscomp): an entry of the program library, not a command line.
        // Settled first (`Desktop::settle`): the entry's own name and id, not the model's word.
        "program_start" => match args.str("shown") {
            Some(shown) => format!("Programm starten: {} ({})", plain(shown), plain(args.str("name").unwrap_or("?"))),
            None => format!("Programm starten: {}", plain(args.str("name").unwrap_or("?"))),
        },
        // The context service's workspaces (ctxd, CX-5): what changes, in Georg's words.
        "workspace_open" => match (args.str("name"), args.str("query"), args.str("workspace")) {
            (Some(n), _, _) => format!("Arbeitsbereich „{}“ öffnen oder anlegen", plain(n)),
            (None, Some(q), _) => format!("Arbeitsbereich aus der Suche nach „{}“ anlegen", plain(q)),
            (None, None, Some(w)) => format!("Arbeitsbereich {} öffnen", plain(w)),
            _ => "Einen Arbeitsbereich öffnen".into(),
        },
        "workspace_add" => format!("Dokumente zum Arbeitsbereich {} hinzufügen: {}", plain(args.str("workspace").unwrap_or("?")), plain(&args.get("paths").or(args.get("docs")).map(Json::to_text).unwrap_or_default())),
        "workspace_rename" => format!("Arbeitsbereich {} umbenennen in „{}“", plain(args.str("workspace").unwrap_or("?")), plain(args.str("name").unwrap_or("?"))),
        "workspace_pin" => {
            let off = args.get("pinned").and_then(Json::as_bool) == Some(false);
            format!("Arbeitsbereich {} {}", plain(args.str("workspace").unwrap_or("?")), if off { "lösen" } else { "anheften" })
        }
        "dismiss" => {
            let back = args.get("undo").and_then(Json::as_bool) == Some(true);
            // query and everywhere also lower the document in later searches (ctxd learn.rs): Georg's yes covers that.
            let how = match (back, args.str("scope")) {
                (true, _) => "wieder zeigen",
                (false, Some("query")) => "ausblenden, auch bei ähnlichen Suchen weiter unten zeigen",
                (false, Some("everywhere")) => "ausblenden, auch bei allen anderen Suchen weiter unten zeigen",
                (false, _) => "ausblenden",
            };
            format!("Dokument {} im Arbeitsbereich {} {how}", plain(args.str("doc").unwrap_or("?")), plain(args.str("workspace").unwrap_or("?")))
        }
        name if let Some(d) = super::native::describe(name, args).or_else(|| super::desk::describe(name, args)) => d,
        name if matches!(args, Json::Obj(fields) if fields.is_empty()) => format!("Werkzeug „{}“ aufrufen", plain(name)),
        name => format!("Werkzeug „{}“ aufrufen mit {}", plain(name), plain(&args.to_text())),
    }
}

pub(super) const UNDO_UNSEEN: &str = "Die letzte Änderung an den Einstellungen zurücknehmen (welche das ist, ließ sich gerade nicht lesen)";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Refused;
    use crate::tools::Class;

    struct NoSettings;

    impl Settings for NoSettings {
        fn request(&self, _: &Json) -> Result<Json, Refused> {
            Err(Refused { code: "unreachable".into(), message: "test".into() })
        }
    }

    #[test]
    fn a_dismissal_says_how_far_it_reaches() {
        let spec = ToolSpec { name: "dismiss".into(), description: String::new(), schema: Json::obj(), class: Class::Change };
        let say = |scope: Option<&str>, undo: bool| {
            let mut a = Json::obj().with("doc", "d1").with("workspace", "w1");
            if let Some(s) = scope {
                a = a.with("scope", s);
            }
            if undo {
                a = a.with("undo", true);
            }
            describe(&spec, &a, &NoSettings)
        };
        assert!(say(None, false).ends_with("w1 ausblenden"), "{}", say(None, false));
        assert!(say(Some("workspace"), false).ends_with("w1 ausblenden"));
        assert!(say(Some("query"), false).contains("auch bei ähnlichen Suchen"));
        assert!(say(Some("everywhere"), false).contains("auch bei allen anderen Suchen"));
        assert!(say(Some("everywhere"), true).ends_with("wieder zeigen"));
    }
}
