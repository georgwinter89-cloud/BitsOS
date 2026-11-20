//! The native tools (plan/spec/assistent-eins K3): what Claude Code can do on this machine, for every provider
//! kind, so an HTTP or local provider can work as the abo does. Everything runs as the session user, inside the
//! assistant's process tree (wave C's tag is inherited by every command started here), so the kernel's fixed
//! rules (F3 write guard, F4 trash divert) hold for them as for any program of the assistant. English
//! descriptions and schemas; every answer is capped ([`cap`]).

pub mod command;
pub mod files;
pub mod glob;
pub mod search;
pub mod trash;
pub mod web;

use super::{Class, Grant, ToolAnswer, ToolSource, ToolSpec};
use crate::json::{self, Json};
use std::path::PathBuf;

/// The most characters of one answer: what a model can still read whole, and what one tool result may take
/// of the conversation.
pub const OUT_MAX: usize = 30_000;

/// `text` whole, or its start and its end with the count of what lies between: the end of a command's output
/// is where its result and its error usually are.
pub fn cap(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let head = max * 2 / 3;
    let tail = max - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(n - tail).collect();
    format!("{start}\n… ({} characters left out) …\n{end}", n - head - tail)
}

/// The session user's home, where a path that is not absolute starts (`HOME`, or `/data/home/<uid>` where a start
/// that dropped root left HOME at `/`).
pub fn home() -> PathBuf {
    crate::provider::program::pictures::owner_home().unwrap_or_else(|| PathBuf::from("/"))
}

/// A path as the model gives it: `~` is the home, a relative one starts in the home. Err: none at all, or one
/// with a NUL byte.
pub fn path_of(p: &str) -> Result<PathBuf, String> {
    let p = p.trim();
    if p.is_empty() || p.contains('\0') {
        return Err("a path is needed (absolute, or relative to the home folder)".into());
    }
    Ok(match p {
        "~" => home(),
        p if p.starts_with("~/") => home().join(&p[2..]),
        p if p.starts_with('/') => PathBuf::from(p),
        p => home().join(p),
    })
}

pub const COMMAND: &str = "befehl";
pub const OUTPUT: &str = "befehl_ausgabe";
pub const END: &str = "befehl_beenden";
pub const READ: &str = "datei_lesen";
pub const WRITE: &str = "datei_schreiben";
pub const EDIT: &str = "datei_aendern";
pub const FIND: &str = "dateien_finden";
pub const GREP: &str = "text_suchen";
pub const FETCH: &str = "web_holen";
pub const DELETE: &str = super::permit::DELETE_TOOL;

/// The native tools a helper with tools of its own has its own equivalents of (HELPER.md version 5: it may
/// leave these out of what it hands its model). `loeschen` is not among them: it asks and keeps the trash.
pub const EQUIVALENTS: [&str; 9] = [COMMAND, OUTPUT, END, READ, WRITE, EDIT, FIND, GREP, FETCH];

fn spec(name: &str, class: Class, description: &str, schema: &str) -> ToolSpec {
    ToolSpec { name: name.into(), description: description.into(), schema: json::parse(schema).unwrap_or_else(|_| Json::obj()), class }
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        spec(
            COMMAND,
            Class::Other,
            "Runs a shell command (/bin/sh -c) as Georg and answers its exit code and output (stdout and stderr together, capped). cwd: where it runs (default: the home folder). timeout_s: the longest it may run, 1 to 3600, default 120; it is ended then. background: true starts it and answers at once with an id; befehl_ausgabe reads its output, befehl_beenden ends it.",
            r#"{"type":"object","properties":{"command":{"type":"string","description":"the command line"},"cwd":{"type":"string"},"timeout_s":{"type":"integer","minimum":1,"maximum":3600},"background":{"type":"boolean"}},"required":["command"],"additionalProperties":false}"#,
        ),
        spec(
            OUTPUT,
            Class::Read,
            "The output of a background command since the last look, and whether it still runs. wait_s: wait up to this long (0 to 60) for new output or its end.",
            r#"{"type":"object","properties":{"id":{"type":"string"},"wait_s":{"type":"integer","minimum":0,"maximum":60}},"required":["id"],"additionalProperties":false}"#,
        ),
        spec(END, Class::Other, "Ends a background command (and what it started) and answers its last output.", r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}"#),
        spec(
            READ,
            Class::Read,
            "Reads a text file and answers its lines with line numbers. offset: the first line (1-based), limit: how many lines (default 2000). A path that is not absolute starts in the home folder; ~ is the home.",
            r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false}"#,
        ),
        spec(
            WRITE,
            Class::Change,
            "Writes a whole file (made with its folders when it is not there, else replaced in place).",
            r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}"#,
        ),
        spec(
            EDIT,
            Class::Change,
            "Replaces an exact piece of text in a file. old_string must occur exactly once, unless replace_all is true.",
            r#"{"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_string","new_string"],"additionalProperties":false}"#,
        ),
        spec(
            FIND,
            Class::Read,
            "Finds files by a glob pattern (* and ? within a name, ** across folders, [abc]), newest first. path: the folder to search (default: the home folder).",
            r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"],"additionalProperties":false}"#,
        ),
        spec(
            GREP,
            Class::Read,
            "Searches the text of files with a regular expression and answers file:line: text. path: a file or folder (default: the home folder); glob: only files whose name fits (like *.rs); ignore_case.",
            r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"ignore_case":{"type":"boolean"}},"required":["pattern"],"additionalProperties":false}"#,
        ),
        spec(
            FETCH,
            Class::Other,
            "Fetches a web page (http or https) and answers it as plain text (a page's markup is taken out), capped.",
            r#"{"type":"object","properties":{"url":{"type":"string"}},"required":["url"],"additionalProperties":false}"#,
        ),
        spec(
            DELETE,
            Class::Change,
            "Deletes a file or folder: it is moved to the trash, where Georg can get it back. Always asks Georg first.",
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
        ),
    ]
}

/// What a call does, in Georg's words (the question before it and its tool line). None: not a native tool.
pub fn describe(name: &str, args: &Json) -> Option<String> {
    let t = |k: &str| crate::term::wrap::inline(args.str(k).unwrap_or("").trim(), 400);
    Some(match name {
        COMMAND => {
            let cwd = args.str("cwd").filter(|c| !c.trim().is_empty()).map(|_| format!(" (in {})", t("cwd"))).unwrap_or_default();
            let bg = if args.get("background").and_then(Json::as_bool) == Some(true) { " im Hintergrund" } else { "" };
            format!("Befehl{bg} ausführen{cwd}: {}", crate::term::wrap::inline(args.str("command").unwrap_or("").trim(), super::helper::COMMAND_MAX))
        }
        OUTPUT => format!("Ausgabe des Befehls {} lesen", t("id")),
        END => format!("Befehl {} beenden", t("id")),
        READ => format!("Datei lesen: {}", t("path")),
        WRITE => format!("Datei schreiben: {} ({})", t("path"), files::sized(args.str("content").unwrap_or(""))),
        EDIT => format!("Datei ändern: {}: {}", t("path"), files::replaced(args)),
        FIND => format!("Dateien suchen: „{}“ in {}", t("pattern"), args.str("path").map_or_else(|| "deinem Benutzerordner".to_string(), |_| t("path"))),
        GREP => format!("Text suchen: „{}“ in {}", t("pattern"), args.str("path").map_or_else(|| "deinem Benutzerordner".to_string(), |_| t("path"))),
        FETCH => format!("Ins Internet gehen: {}", t("url")),
        // On a file system that keeps nothing there is no trash: Georg's yes must be for what really happens.
        DELETE if path_of(args.str("path").unwrap_or("")).is_ok_and(|p| !trash::has_trash(&p)) => format!("Endgültig löschen (dort gibt es keinen Papierkorb): {}", t("path")),
        DELETE => format!("Löschen (in den Papierkorb): {}", t("path")),
        _ => return None,
    })
}

/// The native tools of one chat. Background commands outlive the question that started them ([`command`]).
#[derive(Default)]
pub struct NativeTools {
    /// The chat the calls belong to, for the trash's record.
    pub chat: String,
}

impl NativeTools {
    pub fn has(name: &str) -> bool {
        specs().iter().any(|s| s.name == name)
    }
}

impl ToolSource for NativeTools {
    fn list(&self) -> Vec<ToolSpec> {
        specs()
    }

    fn call(&self, name: &str, args: &Json, grant: Grant) -> ToolAnswer {
        self.call_with(name, args, grant, &|| false)
    }

    fn call_with(&self, name: &str, args: &Json, _grant: Grant, stop: &dyn Fn() -> bool) -> ToolAnswer {
        match name {
            COMMAND => command::run(args, stop),
            OUTPUT => command::output(args, stop),
            END => command::end(args),
            READ => files::read(args),
            WRITE => files::write(args),
            EDIT => files::edit(args),
            FIND => search::find(args, stop),
            GREP => search::grep(args, stop),
            FETCH => web::fetch(args, stop),
            DELETE => {
                let path = path_of(args.str("path").unwrap_or(""))?;
                trash::put(&path, &self.chat, DELETE).map(|t| t.said())
            }
            other => Err(format!("unknown tool {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_paths() {
        assert_eq!(cap("abc", 3), "abc");
        let c = cap(&"x".repeat(100), 30);
        assert!(c.starts_with(&"x".repeat(20)) && c.contains("(70 characters left out)") && c.ends_with(&"x".repeat(10)), "{c}");
        assert_eq!(path_of("/a/b").unwrap(), PathBuf::from("/a/b"));
        assert_eq!(path_of("~").unwrap(), home());
        assert_eq!(path_of("x/y").unwrap(), home().join("x/y"));
        assert!(path_of("  ").is_err() && path_of("a\0b").is_err());
        for s in specs() {
            assert!(s.schema.get("properties").is_some(), "{}", s.name);
            assert!(describe(&s.name, &Json::obj()).is_some(), "{}", s.name);
        }
        let card = |p: &str| describe(DELETE, &json::parse(&format!(r#"{{"path":"{p}"}}"#)).unwrap()).unwrap();
        assert!(card("/proc/self/environ").starts_with("Endgültig löschen"), "proc keeps nothing: {}", card("/proc/self/environ"));
        assert!(card("/no/such/place").starts_with("Löschen (in den Papierkorb)"));
        assert_eq!(specs().iter().filter(|s| s.class == Class::Read).map(|s| s.name.as_str()).collect::<Vec<_>>(), [OUTPUT, READ, FIND, GREP]);
    }

    /// Hostile or broken arguments: the schema turns away what has the wrong shape before any tool runs, and
    /// a tool given such arguments anyway answers with a plain sentence, never a panic, and changes nothing.
    #[test]
    fn hostile_arguments_get_a_plain_answer_and_change_nothing() {
        let tools = NativeTools { chat: "c-hostile".into() };
        let spec = |n: &str| specs().into_iter().find(|s| s.name == n).unwrap();
        let j = |t: &str| json::parse(t).unwrap();
        for (name, args) in [
            (READ, r#"{"path":5}"#),
            (READ, r#"{}"#),
            (WRITE, r#"{"path":["a"],"content":"x"}"#),
            (EDIT, r#"{"path":"/x","old_string":1,"new_string":"y"}"#),
            (COMMAND, r#"{"command":{"rm":"-rf"}}"#),
            (COMMAND, r#"{"command":"true","timeout_s":"lang"}"#),
            (FETCH, r#"{"url":7}"#),
            (DELETE, r#"{"path":null}"#),
            (DELETE, r#"{"path":"/","extra":true}"#),
        ] {
            assert!(super::super::schema::check(&spec(name).schema, &j(args)).is_err(), "{name} {args}");
        }
        let d = std::env::temp_dir().join(format!("assist-hostile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.txt"), "eins\nzwei\n").unwrap();
        let f = d.join("a.txt").display().to_string();
        let run = |name: &str, args: String| tools.call(name, &j(&args), Grant::default());
        let err = |name: &str, args: String, says: &str| {
            let r = run(name, args.clone());
            assert!(r.as_ref().is_err_and(|e| e.contains(says)), "{name} {args}: {r:?}");
        };
        // Wrong shapes that got past a schema (a helper's own call, say) are read as nothing given.
        err(READ, r#"{"path":5}"#.into(), "a path is needed");
        err(WRITE, r#"{"path":"","content":"x"}"#.into(), "a path is needed");
        err(COMMAND, r#"{"command":5}"#.into(), "command is empty");
        err(COMMAND, "{\"command\":\"echo a\\u0000b\"}".into(), "command is empty");
        err(OUTPUT, r#"{"id":5}"#.into(), "no background command");
        err(END, r#"{"id":"b0; kill -9 1"}"#.into(), "no background command");
        // Extreme numbers are bounded, not obeyed.
        assert!(run(READ, format!(r#"{{"path":"{f}","offset":-5,"limit":0}}"#)).unwrap().starts_with("     1\teins"));
        let far = run(READ, format!(r#"{{"path":"{f}","offset":1000000000000000000,"limit":1000000000000000000}}"#));
        assert!(far.as_ref().is_ok_and(|o| o.contains("nothing at line")), "{far:?}");
        assert!(run(READ, format!(r#"{{"path":"{f}","offset":1e300,"limit":-1e300}}"#)).is_ok_and(|o| o.starts_with("     1\teins")));
        let t = std::time::Instant::now();
        assert!(run(COMMAND, r#"{"command":"echo da","timeout_s":-1}"#.into()).unwrap().starts_with("exit code 0"));
        assert!(run(COMMAND, r#"{"command":"echo da","timeout_s":1e30}"#.into()).unwrap().starts_with("exit code 0"));
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
        // Searches: a bad expression, one that backtracks without end elsewhere, too long a pattern.
        err(GREP, format!(r#"{{"pattern":"(","path":"{}"}}"#, d.display()), "no regular expression");
        std::fs::write(d.join("viel.txt"), "a".repeat(20_000)).unwrap();
        let t = std::time::Instant::now();
        assert!(run(GREP, format!(r#"{{"pattern":"(a*)*b","path":"{}"}}"#, d.display())).unwrap().starts_with("no line"));
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "{:?}", t.elapsed());
        err(FIND, format!(r#"{{"pattern":"{}","path":"{}"}}"#, "*".repeat(glob::PATTERN_MAX + 1), d.display()), "longer than");
        err(FIND, r#"{"pattern":"*","path":"/no/such/folder"}"#.into(), "is not a folder");
        // Addresses that are not a web page.
        for url in ["file:///etc/passwd", "ftp://example.org/x", "javascript:alert(1)", "http://", "", "http://a b/"] {
            assert!(run(FETCH, format!(r#"{{"url":"{url}"}}"#)).is_err(), "{url}");
        }
        // The trash: never the root, a mount point, the trash itself, nothing at all.
        err(DELETE, r#"{"path":"/"}"#.into(), "cannot be deleted");
        err(DELETE, r#"{"path":"  "}"#.into(), "a path is needed");
        err(DELETE, format!(r#"{{"path":"{}/{}/x"}}"#, d.display(), trash::FOLDER), "");
        std::fs::create_dir_all(d.join(trash::FOLDER).join("e1")).unwrap();
        std::fs::write(d.join(trash::FOLDER).join("e1/alt.txt"), "x").unwrap();
        err(DELETE, format!(r#"{{"path":"{}/{}/e1/alt.txt"}}"#, d.display(), trash::FOLDER), "only Georg empties");
        assert!(d.join(trash::FOLDER).join("e1/alt.txt").exists() && d.join("a.txt").exists(), "nothing changed");
        let _ = std::fs::remove_dir_all(&d);
    }
}

