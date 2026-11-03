//! The confirmer's protocol (bitsinstall <-> bitscomp, MC-9b): one JSON object per line over the stream socket
//! `SOCKET` (bitsinstall listens; the session's bitscomp connects and stays connected; while none is, every
//! request is refused). bitsinstall takes a peer only when its executable (SO_PEERCRED's pid, then
//! /proc/<pid>/exe) is the expected confirmer (`/bin/bitscomp`; CONFIG.TXT `mcp_confirmer=test` names
//! `bitsinstall test-confirmer` instead, for QEMU scenarios) and runs as root or the desktop user, and only one at a
//! time (the first holds the place until it disconnects).
//!
//! installer -> confirmer:
//! - `{"type":"hello","version":1,"grants":[{"kind","until"}]}` once accepted;
//! - `{"type":"ask","id":N,"kind":"install","title":"...","text":"...","timeout_ms":60000}`: show the dialog on
//!   top, with exactly `title` and `text`, focus on "Nein"; answer within `timeout_ms`;
//! - `{"type":"cancel","id":N,"reason":"..."}`: the request ended (timeout); close its dialog;
//! - `{"type":"grants","grants":[...]}`: the grants now (after every change, and on `list`);
//! - `{"type":"refused","what":"grant"|"answer","reason":"..."}`: a message was not taken.
//! - MC-10: `hello` also carries `"access":[{"client","name","until","nvme"}]` (live full-access grants) and
//!   `"clients":[{"client","name"}]` (the paired clients, for Georg's own dialog, Strg+Alt+V);
//!   `{"type":"ask","id":N,"kind":"access","title","text","timeout_ms","client","name","hours"}` asks for full
//!   access (bitscomp draws the access dialog: 1 h, 8 h, 24 h preselected, 7 Tage, Ablehnen, the NVMe box);
//!   `{"type":"access","access":[...],"clients":[...]}` after every change.
//!
//! confirmer -> installer:
//! - `{"type":"answer","id":N,"yes":true|false,"input":"physical"|"injected"|"none"}`: a yes counts only with
//!   `input` = `physical` (real keyboard or mouse; events marked injected, R8c, never confirm);
//! - `{"type":"grant","kind":"install","until":U,"input":"physical"}`: a standing grant set at the physical
//!   keyboard until Unix time U (at most 24 h ahead);
//! - `{"type":"revoke","kind":"install"|"access"|"all"}` (`all` ends full access too), `{"type":"list"}`;
//! - MC-10: `{"type":"access","id":N?,"client":"<64 hex>","hours":H,"nvme":bool,"input":"physical"}`: full access
//!   for that client for H hours (1, 8, 24 or 168), the NVMe write gate too when `nvme`; with `id` the answer to
//!   that question, without it Georg's own grant from Strg+Alt+V. Only `input` = `physical` sets one.

use crate::access::Client;
use crate::grant::Grant;
use mcp_wire::Json;
use mcp_wire::installer::Kind;

pub const SOCKET: &str = "/run/bitsos/install.sock";
pub const VERSION: u64 = 1;
/// The longest line either side sends or takes.
pub const LINE_MAX: usize = 64 * 1024;
/// What the dialog may hold (bitscomp draws all of it; the installer cuts arguments to fit).
pub const TEXT_MAX: usize = 4000;

/// Where a confirmation came from, as the confirmer saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Physical,
    Injected,
    None,
}

impl Input {
    fn parse(s: &str) -> Input {
        match s {
            "physical" => Input::Physical,
            "injected" => Input::Injected,
            _ => Input::None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Input::Physical => "physical",
            Input::Injected => "injected",
            Input::None => "none",
        }
    }
}

/// A message from the confirmer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromConfirmer {
    Answer { id: u64, yes: bool, input: Input },
    Grant { kind: Option<Kind>, kind_text: String, until: i64, input: Input },
    /// None: all.
    Revoke(Option<Kind>),
    List,
    /// MC-10: full access (`id`: the answer to that question).
    Access { id: Option<u64>, client: String, hours: i64, nvme: bool, input: Input },
}

pub fn parse(line: &str) -> Option<FromConfirmer> {
    if line.len() > LINE_MAX {
        return None;
    }
    let j = Json::parse(line).ok()?;
    let input = || Input::parse(j.get("input").and_then(Json::as_str).unwrap_or(""));
    match j.get("type")?.as_str()? {
        "answer" => Some(FromConfirmer::Answer { id: j.get("id")?.as_u64()?, yes: j.get("yes")?.as_bool()?, input: input() }),
        "grant" => {
            let k = j.get("kind")?.as_str()?;
            Some(FromConfirmer::Grant { kind: Kind::parse(k), kind_text: k.chars().take(16).collect(), until: j.get("until")?.as_i64()?, input: input() })
        }
        "revoke" => match j.get("kind")?.as_str()? {
            "all" => Some(FromConfirmer::Revoke(None)),
            k => Kind::parse(k).map(|k| FromConfirmer::Revoke(Some(k))),
        },
        "list" => Some(FromConfirmer::List),
        "access" => Some(FromConfirmer::Access {
            id: match j.get("id") {
                None | Some(Json::Null) => None,
                Some(v) => Some(v.as_u64()?),
            },
            client: String::from(j.get("client").and_then(Json::as_str).unwrap_or("")),
            hours: j.get("hours")?.as_i64()?,
            nvme: j.get("nvme").and_then(Json::as_bool).unwrap_or(false),
            input: input(),
        }),
        _ => None,
    }
}

fn s(v: &str) -> Json {
    Json::str(v)
}
fn n(v: i64) -> Json {
    Json::Num(v as f64)
}

pub fn grants_json(g: &[Grant]) -> Json {
    Json::Arr(g.iter().map(|g| Json::obj(vec![("kind", s(g.kind.name())), ("until", n(g.until))])).collect())
}

fn line(j: Json) -> String {
    let mut t = j.to_string();
    t.push('\n');
    t
}

pub fn hello(g: &[Grant], access: &[Shown], clients: &[Client]) -> String {
    line(Json::obj(vec![("type", s("hello")), ("version", n(VERSION as i64)), ("grants", grants_json(g)), ("access", access_json(access)), ("clients", clients_json(clients))]))
}

/// MC-10: a live full-access grant as the confirmer shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub client: String,
    pub name: String,
    pub until: i64,
    pub nvme: bool,
}

pub fn access_json(a: &[Shown]) -> Json {
    Json::Arr(a.iter().map(|x| Json::obj(vec![("client", s(&x.client)), ("name", s(&x.name)), ("until", n(x.until)), ("nvme", Json::Bool(x.nvme))])).collect())
}
pub fn clients_json(c: &[Client]) -> Json {
    Json::Arr(c.iter().map(|x| Json::obj(vec![("client", s(&x.fp)), ("name", s(&x.name))])).collect())
}
pub fn access(a: &[Shown], clients: &[Client]) -> String {
    line(Json::obj(vec![("type", s("access")), ("access", access_json(a)), ("clients", clients_json(clients))]))
}
/// MC-10: the question for full access.
pub fn ask_access(id: u64, title: &str, text: &str, timeout_ms: u64, c: &Client, hours: i64) -> String {
    line(Json::obj(vec![
        ("type", s("ask")),
        ("id", n(id as i64)),
        ("kind", s(Kind::Access.name())),
        ("title", s(title)),
        ("text", s(text)),
        ("timeout_ms", n(timeout_ms as i64)),
        ("client", s(&c.fp)),
        ("name", s(&c.name)),
        ("hours", n(hours)),
    ]))
}
pub fn ask(id: u64, kind: Kind, title: &str, text: &str, timeout_ms: u64) -> String {
    line(Json::obj(vec![("type", s("ask")), ("id", n(id as i64)), ("kind", s(kind.name())), ("title", s(title)), ("text", s(text)), ("timeout_ms", n(timeout_ms as i64))]))
}
pub fn cancel(id: u64, reason: &str) -> String {
    line(Json::obj(vec![("type", s("cancel")), ("id", n(id as i64)), ("reason", s(reason))]))
}
pub fn grants(g: &[Grant]) -> String {
    line(Json::obj(vec![("type", s("grants")), ("grants", grants_json(g))]))
}
pub fn refused(what: &str, reason: &str) -> String {
    line(Json::obj(vec![("type", s("refused")), ("what", s(what)), ("reason", s(reason))]))
}

// The confirmer's side (bitscomp, and `bitsinstall test-confirmer`).

pub fn answer(id: u64, yes: bool, input: Input) -> String {
    line(Json::obj(vec![("type", s("answer")), ("id", n(id as i64)), ("yes", Json::Bool(yes)), ("input", s(input.name()))]))
}
pub fn grant(kind: &str, until: i64, input: Input) -> String {
    line(Json::obj(vec![("type", s("grant")), ("kind", s(kind)), ("until", n(until)), ("input", s(input.name()))]))
}
pub fn revoke(kind: &str) -> String {
    line(Json::obj(vec![("type", s("revoke")), ("kind", s(kind))]))
}
/// MC-10: full access from the confirmer (`id`: the answer to that question).
pub fn access_answer(id: Option<u64>, client: &str, hours: i64, nvme: bool, input: Input) -> String {
    let id = id.map_or(Json::Null, |i| n(i as i64));
    line(Json::obj(vec![("type", s("access")), ("id", id), ("client", s(client)), ("hours", n(hours)), ("nvme", Json::Bool(nvme)), ("input", s(input.name()))]))
}

/// An `ask` as the confirmer reads it: (id, kind, title, text, timeout_ms).
pub fn parse_ask(line: &str) -> Option<(u64, String, String, String, u64)> {
    let j = Json::parse(line).ok()?;
    if j.get("type")?.as_str()? != "ask" {
        return None;
    }
    let t = |k: &str| j.get(k).and_then(Json::as_str).map(String::from);
    Some((j.get("id")?.as_u64()?, t("kind")?, t("title")?, t("text")?, j.get("timeout_ms")?.as_u64()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_directions() {
        assert_eq!(parse(&answer(5, true, Input::Physical)), Some(FromConfirmer::Answer { id: 5, yes: true, input: Input::Physical }));
        assert_eq!(parse(&answer(5, true, Input::Injected)), Some(FromConfirmer::Answer { id: 5, yes: true, input: Input::Injected }));
        assert_eq!(parse(r#"{"type":"answer","id":5,"yes":true}"#), Some(FromConfirmer::Answer { id: 5, yes: true, input: Input::None }));
        assert_eq!(
            parse(&grant("install", 1_800_000_000, Input::Physical)),
            Some(FromConfirmer::Grant { kind: Some(Kind::Install), kind_text: "install".into(), until: 1_800_000_000, input: Input::Physical })
        );
        assert!(matches!(parse(&grant("everything", 1, Input::Physical)), Some(FromConfirmer::Grant { kind: None, .. })));
        assert_eq!(parse(&revoke("all")), Some(FromConfirmer::Revoke(None)));
        assert_eq!(parse(&revoke("reload")), Some(FromConfirmer::Revoke(Some(Kind::Reload))));
        assert_eq!(parse(r#"{"type":"list"}"#), Some(FromConfirmer::List));
        let c = "c".repeat(64);
        assert_eq!(parse(&access_answer(Some(4), &c, 24, true, Input::Physical)), Some(FromConfirmer::Access { id: Some(4), client: c.clone(), hours: 24, nvme: true, input: Input::Physical }));
        assert_eq!(parse(&access_answer(None, &c, 1, false, Input::Injected)), Some(FromConfirmer::Access { id: None, client: c.clone(), hours: 1, nvme: false, input: Input::Injected }));
        assert_eq!(parse(r#"{"type":"access","client":"x"}"#), None, "hours missing");
        assert_eq!(parse(r#"{"type":"access","id":-2,"hours":1}"#), None);
        assert_eq!(parse(&revoke("access")), Some(FromConfirmer::Revoke(Some(Kind::Access))));
        let cl = Client { fp: c.clone(), name: "bitsos-desk".into() };
        let q = ask_access(3, "Vollzugriff", "Text", 240_000, &cl, 24);
        assert_eq!(parse_ask(&q), Some((3, "access".into(), "Vollzugriff".into(), "Text".into(), 240_000)));
        assert!(q.contains(&c) && q.contains("\"hours\":24"));
        for bad in ["", "{", r#"{"type":"answer"}"#, r#"{"type":"answer","id":-1,"yes":true}"#, r#"{"type":"frob"}"#, r#"{"type":"revoke","kind":"x"}"#] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(parse(&" ".repeat(LINE_MAX + 1)), None);
        let a = ask(9, Kind::Reload, "Kernel neu laden", "Zeile 1\nZeile \"2\"", 60_000);
        assert!(a.ends_with('\n') && !a[..a.len() - 1].contains('\n'), "one line");
        assert_eq!(parse_ask(&a), Some((9, "reload".into(), "Kernel neu laden".into(), "Zeile 1\nZeile \"2\"".into(), 60_000)));
    }
}
