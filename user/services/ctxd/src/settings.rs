//! CX-5: the `context.*` keys in settingsd (CL-1). ctxd registers them as their owner (op `register`, the
//! way bitsjournal does) and reads them with one `list` per look; a change is applied by starting ctxd anew
//! (`system::restart`). Without settingsd the defaults hold: no folder, so nothing is indexed.
//! Folders are text (settingsd has no list type): several are separated by `;` or a line break.

use crate::config::{Context, RootCfg};
use serde_json::{Value, json};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SOCKET: &str = "/run/bitsos/settings.sock";
pub const OWNER: &str = "ctxd";

/// (key, min, max, default, German, English).
const INTS: &[(&str, i64, i64, i64, &str, &str)] = &[
    (
        "context.claude.snippet_chars",
        0,
        1000,
        200,
        "So viele Zeichen Textausschnitt bekommt eine KI je gefundenem Dokument (0: keinen, nur Titel und Ort)",
        "Characters of snippet an AI gets per found document (0: none, only title and place)",
    ),
    (
        "context.index.workers",
        1,
        16,
        2,
        "So viele Dateien liest der Kontextdienst gleichzeitig",
        "Files the context service reads at the same time",
    ),
    (
        "context.index.max_file_mb",
        1,
        4096,
        64,
        "Größere Dateien (MB) lässt der Kontextdienst aus",
        "The context service leaves out larger files (MB)",
    ),
    (
        "context.index.rescan_minutes",
        1,
        1440,
        10,
        "Zur Sicherheit sieht der Kontextdienst alle so viele Minuten alle Ordner ganz durch",
        "To be safe the context service walks all folders every this many minutes",
    ),
];

/// (key, default, German, English).
const BOOLS: &[(&str, bool, &str, &str)] = &[
    (
        "context.enabled",
        true,
        "Der Kontextdienst durchsucht die genannten Ordner, damit Suche und Assistent Dokumente finden",
        "The context service searches the named folders so search and the assistant find documents",
    ),
    (
        "context.index.watch",
        true,
        "Neue, geänderte und gelöschte Dateien sofort bemerken (sonst erst beim nächsten Durchsehen)",
        "Notice new, changed and deleted files at once (else at the next walk)",
    ),
    (
        "context.claude.allow_forget",
        false,
        "Eine KI darf den Kontextdienst Dinge vergessen lassen (sonst nur Georg selbst)",
        "An AI may make the context service forget things (else only Georg himself)",
    ),
];

/// (key, max bytes, German, English); defaults below.
const TEXTS: &[(&str, i64, &str, &str)] = &[
    (
        "context.roots",
        8192,
        "Ordner, die der Kontextdienst durchsucht (mehrere mit ; trennen). Leer: er durchsucht nichts",
        "Folders the context service searches (separate several with ;). Empty: it searches nothing",
    ),
    (
        "context.private_roots",
        8192,
        "Private Ordner: werden durchsucht und auf dem Bildschirm gezeigt, gehen aber nie an eine KI (mehrere mit ; trennen)",
        "Private folders: searched and shown on screen, never given to an AI (separate several with ;)",
    ),
    (
        "context.excludes",
        4096,
        "Namen, die der Kontextdienst nie liest (mit ; trennen, * steht für beliebige Zeichen)",
        "Names the context service never reads (separate with ;, * stands for any characters)",
    ),
];

const EMBEDDER: &str = "context.embedder.kind";
const THREADS: &str = "context.embedder.threads";

pub fn register_request() -> Value {
    let text = |de: &str, en: &str| json!({"de": de, "en": en});
    let mut keys = Vec::new();
    for (k, d, de, en) in BOOLS {
        keys.push(json!({"key": k, "schema": {"type": "bool"}, "default": d, "text": text(de, en)}));
    }
    for (k, min, max, d, de, en) in INTS {
        keys.push(json!({"key": k, "schema": {"type": "int", "min": min, "max": max}, "default": d, "text": text(de, en)}));
    }
    for (k, max, de, en) in TEXTS {
        let d = if *k == "context.excludes" { crate::config::default_excludes().join(";") } else { String::new() };
        keys.push(json!({"key": k, "schema": {"type": "text", "max_bytes": max}, "default": d, "text": text(de, en)}));
    }
    keys.push(json!({
        "key": EMBEDDER,
        "schema": {"type": "choice", "choices": ["local", "keywords"],
                   "choices_de": ["Lokales Sprachmodell auf diesem Rechner", "Nur Stichwörter"]},
        "default": "local",
        "text": text("Wie der Kontextdienst nach Bedeutung sucht (nie über das Internet)",
                     "How the context service searches by meaning (never over the internet)"),
    }));
    keys.push(json!({
        "key": THREADS,
        "schema": {"type": "int", "min": 1, "max": 64, "word": "auto"},
        "default": "auto",
        "text": text("So viele Rechenkerne darf das Sprachmodell beim Einlesen nutzen (auto: die Hälfte)",
                     "Cores the language model may use while reading in (auto: half)"),
    }));
    json!({"op": "register", "owner": OWNER, "keys": keys})
}

/// Number of keys `register_request` names (a `list` with fewer means settingsd forgot some).
pub fn key_count() -> usize {
    BOOLS.len() + INTS.len() + TEXTS.len() + 2
}

pub(crate) fn ask(sock: &Path, req: &Value) -> Result<Value, String> {
    let s = UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    // B4: a registry that hangs never stops ctxd.
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    // One answer line, at most 1 MiB (a `list` of a dozen keys is a few KiB).
    let line = match crate::sock::read_line(&mut BufReader::new(s), 1 << 20).map_err(|e| e.to_string())? {
        crate::sock::Line::Text(t) => t,
        _ => return Err("settingsd sent no answer line".into()),
    };
    serde_json::from_str(line.trim()).map_err(|e| format!("settingsd answered {:?}: {e}", crate::util::cut_bytes(&line, 200)))
}

/// The values settingsd holds now, as (key, value). Registers the keys first when some are missing.
pub fn fetch(sock: &Path) -> Result<Vec<(String, Value)>, String> {
    let list = |s: &Path| ask(s, &json!({"op": "list", "prefix": "context"}));
    let mut v = list(sock)?;
    let n = v["keys"].as_array().map_or(0, Vec::len);
    if n < key_count() {
        let r = ask(sock, &register_request())?;
        if r["ok"] != json!(true) {
            return Err(format!("settingsd refused the context.* keys: {r}"));
        }
        crate::event!("info", "settings.registered", keys = key_count());
        v = list(sock)?;
    }
    if v["ok"] != json!(true) {
        return Err(format!("settingsd: {v}"));
    }
    Ok(v["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| Some((k["key"].as_str()?.to_string(), k["value"].clone())))
        .collect())
}

/// `a;b` or one per line; blanks dropped.
pub fn split_paths(s: &str) -> Vec<PathBuf> {
    s.split([';', '\n']).map(str::trim).filter(|p| !p.is_empty()).map(PathBuf::from).collect()
}

/// The values applied to `c` (the system defaults). A folder named both plain and private is private.
pub fn apply(c: &mut Context, values: &[(String, Value)]) {
    let get = |k: &str| values.iter().find(|(n, _)| n == k).map(|(_, v)| v);
    let int = |k: &str| get(k).and_then(Value::as_i64).map(|n| n.max(0) as u64);
    let flag = |k: &str| get(k).and_then(Value::as_bool);
    let text = |k: &str| get(k).and_then(Value::as_str).map(str::to_string);
    let private = text("context.private_roots").map(|t| split_paths(&t)).unwrap_or_default();
    let plain = text("context.roots").map(|t| split_paths(&t)).unwrap_or_default();
    let mut roots: Vec<RootCfg> = private.iter().map(|p| RootCfg { path: p.clone(), private: true }).collect();
    roots.extend(plain.into_iter().filter(|p| !private.contains(p)).map(|p| RootCfg { path: p, private: false }));
    c.roots = if flag("context.enabled") == Some(false) { Vec::new() } else { roots };
    if let Some(t) = text("context.excludes") {
        c.excludes = t.split(';').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect();
    }
    if let Some(n) = int("context.claude.snippet_chars") {
        c.claude.snippet_chars = n as usize;
    }
    if let Some(b) = flag("context.claude.allow_forget") {
        c.claude.allow_forget = b;
    }
    if let Some(n) = int("context.index.workers") {
        c.index.threads = (n as usize).max(1);
    }
    if let Some(n) = int("context.index.max_file_mb") {
        c.index.max_file_mb = n.max(1);
    }
    if let Some(n) = int("context.index.rescan_minutes") {
        c.index.rescan_seconds = n.max(1) * 60;
    }
    if let Some(b) = flag("context.index.watch") {
        c.index.watch = b;
    }
    match get(EMBEDDER).and_then(Value::as_str) {
        Some("keywords") => c.embedder.kind = "none".into(),
        Some("local") => c.embedder.kind = "llama".into(),
        _ => {}
    }
    if let Some(n) = int(THREADS) {
        c.embedder.threads = (n as usize).max(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registration_names_every_key_once_with_german_text() {
        let r = register_request();
        let keys = r["keys"].as_array().unwrap();
        assert_eq!(keys.len(), key_count());
        // settingsd refuses a key that is also a group of keys (its TOML files could not hold both).
        for a in keys.iter().map(|k| k["key"].as_str().unwrap()) {
            for b in keys.iter().map(|k| k["key"].as_str().unwrap()) {
                assert!(!b.starts_with(&format!("{a}.")), "{a} would be a group of {b}");
            }
        }
        let mut names: Vec<&str> = keys.iter().map(|k| k["key"].as_str().unwrap()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), keys.len());
        for k in keys {
            assert!(k["key"].as_str().unwrap().starts_with("context."));
            let de = k["text"]["de"].as_str().unwrap();
            assert!(!de.is_empty() && !de.contains('_') && !de.contains("ctxd"), "plain German: {de}");
        }
    }

    #[test]
    fn values_reach_the_context_and_private_wins() {
        let mut c = Context::default();
        let v = |k: &str, j: Value| (k.to_string(), j);
        apply(
            &mut c,
            &[
                v("context.roots", json!("/data/home/1000/Dokumente; /data/home/1000/Briefe\n")),
                v("context.private_roots", json!("/data/home/1000/Briefe")),
                v("context.claude.snippet_chars", json!(0)),
                v("context.index.rescan_minutes", json!(3)),
                v("context.embedder.kind", json!("keywords")),
                v("context.embedder.threads", json!("auto")),
                v("context.excludes", json!(".ssh; *.kdbx")),
            ],
        );
        assert_eq!(
            c.roots,
            vec![
                RootCfg { path: "/data/home/1000/Briefe".into(), private: true },
                RootCfg { path: "/data/home/1000/Dokumente".into(), private: false },
            ]
        );
        assert_eq!((c.claude.snippet_chars, c.index.rescan_seconds), (0, 180));
        assert_eq!(c.embedder.kind, "none");
        assert_eq!(c.embedder.threads, Context::default().embedder.threads, "auto keeps the system's own count");
        assert_eq!(c.excludes, vec![".ssh".to_string(), "*.kdbx".to_string()]);
        apply(&mut c, &[v("context.enabled", json!(false)), v("context.roots", json!("/x"))]);
        assert!(c.roots.is_empty(), "switched off: nothing is read");
    }

    #[test]
    fn no_settingsd_is_an_error_not_a_hang() {
        assert!(fetch(Path::new("/nonexistent/settings.sock")).is_err());
    }
}
