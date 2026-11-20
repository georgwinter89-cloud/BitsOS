//! `assist-fake-serve` (examples/fake_serve.rs): the real service of protocol v2 (K1) with a scripted model, for
//! the views' tests (wave B) and for trying a view by hand. Everything but the model is the real code: chats on
//! disk, workers, the level, questions with `ask_id`, desktop calls with `desk`/`desk_result`. The model is a
//! small OpenAI-compatible server on a loopback port that answers from a script; the settings live in memory;
//! every caller of the same user is served (this is no compositor's socket).
//!
//! Script (JSON): `{"level":"ask|answer|auto", "name":"Claude", "answers":[{"match":"such", "rounds":[{"text":
//! "...", "tool":"handeln", "args":{...}, "wait_ms":0}, ...]}]}`. A question takes the first answer whose `match`
//! is in its words (an answer without `match` fits any), each one once; every request of that question takes
//! its next round: text, and a tool call when the round names one. Without a fitting answer the model says
//! [`UNSCRIPTED`].

use super::events::Events;
use super::peer::Gate;
use super::{Ctx, Parts, Seat};
use crate::json::{self, Json};
use crate::settings::{Refused, Settings};
use crate::tools::desk::{self, Desk, DeskTools};
use crate::tools::{Class, Grant, ToolAnswer, ToolSource, ToolSpec};
use settingsd::peer::{Peer, Source};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

pub const UNSCRIPTED: &str = "Darauf hat das Skript keine Antwort.";
/// The fake's own changing tool: it does nothing, so a view can show a question and its answer.
pub const PROBE: &str = "probe_aendern";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Round {
    pub text: String,
    pub tool: Option<(String, Json)>,
    pub wait_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub matches: Option<String>,
    pub rounds: Vec<Round>,
}

#[derive(Clone, Debug, Default)]
pub struct Script {
    pub level: String,
    pub name: String,
    pub answers: Vec<Answer>,
}

impl Script {
    pub fn parse(text: &str) -> Result<Script, String> {
        let j = json::parse(text).map_err(|e| format!("the script is no JSON: {e}"))?;
        let answers = match j.get("answers") {
            Some(Json::Arr(a)) => a
                .iter()
                .map(|a| Answer {
                    matches: a.str("match").map(str::to_string),
                    rounds: match a.get("rounds") {
                        Some(Json::Arr(r)) => r
                            .iter()
                            .map(|r| Round {
                                text: r.str("text").unwrap_or("").to_string(),
                                tool: r.str("tool").map(|t| (t.to_string(), r.get("args").cloned().unwrap_or_else(Json::obj))),
                                wait_ms: r.get("wait_ms").and_then(Json::as_i64).and_then(|w| u64::try_from(w).ok()).unwrap_or(0),
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok(Script { level: j.str("level").unwrap_or("ask").to_string(), name: j.str("name").unwrap_or("Claude").to_string(), answers })
    }
}

/// The settings of the fake: values in memory, the `ai` keys the engine reads.
pub struct Mem(Mutex<BTreeMap<String, Json>>);

impl Settings for Mem {
    fn request(&self, req: &Json) -> Result<Json, Refused> {
        let v = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let ok = Json::obj().with("ok", true);
        Ok(match req.str("op") {
            Some("list") => {
                let prefix = req.str("prefix").unwrap_or("");
                let keys: Vec<Json> = v.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, val)| Json::obj().with("key", k.as_str()).with("value", val.clone()).with("changed", true)).collect();
                ok.with("keys", keys)
            }
            Some("get") => match req.str("key").and_then(|k| v.get(k)) {
                Some(val) => ok.with("value", val.clone()).with("changed", true),
                None => Json::obj().with("ok", false).with("error", "unknown_key").with("message", "no such setting"),
            },
            _ => Json::obj().with("ok", false).with("error", "unknown_op").with("message", "the fake keeps only what it was given"),
        })
    }
}

/// The desktop tools and [`PROBE`].
struct FakeTools<'a> {
    desk: Option<DeskTools<'a>>,
}

fn probe() -> ToolSpec {
    ToolSpec { name: PROBE.into(), description: "Changes nothing (assist-fake-serve).".into(), schema: json::parse(r#"{"type":"object","properties":{"was":{"type":"string"}}}"#).unwrap_or_else(|_| Json::obj()), class: Class::Change }
}

impl ToolSource for FakeTools<'_> {
    fn list(&self) -> Vec<ToolSpec> {
        let mut v = vec![probe()];
        if self.desk.is_some() {
            v.extend(desk::specs());
        }
        v
    }
    fn call(&self, name: &str, args: &Json, grant: Grant) -> ToolAnswer {
        self.call_with(name, args, grant, &|| false)
    }
    fn call_with(&self, name: &str, args: &Json, grant: Grant, stop: &dyn Fn() -> bool) -> ToolAnswer {
        match (&self.desk, name) {
            (_, PROBE) => Ok(format!("probe_aendern: nothing changed ({})", args.str("was").unwrap_or(""))),
            (Some(d), _) => d.call_with(name, args, grant, stop),
            _ => Err(format!("unknown tool {name:?}")),
        }
    }
    fn take_pictures(&self) -> Vec<crate::conv::Image> {
        self.desk.as_ref().map(ToolSource::take_pictures).unwrap_or_default()
    }
}

fn sse(chunk: &Json) -> String {
    format!("data: {}\n\n", chunk.to_text())
}

/// The model's answer to one request, as an OpenAI-compatible stream.
fn stream(round: &Round, n: u64) -> String {
    let delta = |d: Json, finish: Option<&str>| sse(&Json::obj().with("choices", vec![Json::obj().with("index", 0i64).with("delta", d).with("finish_reason", finish.map(Json::from).unwrap_or(Json::Null))]));
    let mut out = String::new();
    if !round.text.is_empty() {
        out.push_str(&delta(Json::obj().with("content", round.text.as_str()), None));
    }
    let finish = match &round.tool {
        Some((name, args)) => {
            let f = Json::obj().with("name", name.as_str()).with("arguments", args.to_text());
            out.push_str(&delta(Json::obj().with("tool_calls", vec![Json::obj().with("index", 0i64).with("id", format!("call_fake_{n}")).with("type", "function").with("function", f)]), None));
            "tool_calls"
        }
        None => "stop",
    };
    out.push_str(&delta(Json::obj(), Some(finish)));
    out.push_str("data: [DONE]\n\n");
    out
}

struct Brain {
    answers: Vec<Answer>,
    rounds: VecDeque<Round>,
    n: u64,
}

impl Brain {
    /// The next round for a request whose conversation ends with `last` (`role`, words).
    fn next(&mut self, role: &str, words: &str) -> Round {
        self.n += 1;
        if role != "tool" {
            let i = self.answers.iter().position(|a| a.matches.as_deref().is_none_or(|m| words.contains(m)));
            self.rounds = i.map(|i| self.answers.remove(i).rounds.into()).unwrap_or_default();
        }
        self.rounds.pop_front().unwrap_or_else(|| Round { text: if role == "tool" { String::new() } else { UNSCRIPTED.into() }, tool: None, wait_ms: 0 })
    }
}

fn serve_model(s: TcpStream, brain: &Mutex<Brain>) {
    let Ok(mut w) = s.try_clone() else { return };
    let mut r = BufReader::new(s);
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0u8; len];
    if r.read_exact(&mut body).is_err() {
        return;
    }
    let req = json::parse(&String::from_utf8_lossy(&body)).unwrap_or_else(|_| Json::obj());
    let last = match req.get("messages") {
        Some(Json::Arr(m)) => m.last().cloned().unwrap_or_else(Json::obj),
        _ => Json::obj(),
    };
    let words = match last.get("content") {
        Some(Json::Str(t)) => t.clone(),
        Some(c) => c.to_text(),
        None => String::new(),
    };
    let round = brain.lock().unwrap_or_else(PoisonError::into_inner).next(last.str("role").unwrap_or(""), &words);
    std::thread::sleep(Duration::from_millis(round.wait_ms));
    let n = brain.lock().unwrap_or_else(PoisonError::into_inner).n;
    let body = stream(&round, n);
    let _ = write!(w, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

/// The service with `script` on `listen`; the chats in `chats`. Returns when it listens.
pub fn start(listen: &std::path::Path, chats: PathBuf, script: Script) -> std::io::Result<Arc<Ctx>> {
    let model = TcpListener::bind("127.0.0.1:0")?;
    let port = model.local_addr()?.port();
    let brain = Arc::new(Mutex::new(Brain { answers: script.answers.clone(), rounds: VecDeque::new(), n: 0 }));
    std::thread::spawn(move || {
        for s in model.incoming().flatten() {
            let b = brain.clone();
            std::thread::spawn(move || serve_model(s, &b));
        }
    });
    let mut v = BTreeMap::new();
    for (k, val) in [
        ("ai.providers", "fake".to_string()),
        ("ai.role.chat", "fake".into()),
        ("ai.provider.fake.kind", "openai_compatible".into()),
        ("ai.provider.fake.address", format!("http://127.0.0.1:{port}")),
        ("ai.provider.fake.model", "skript".into()),
        ("ai.provider.fake.place", "local".into()),
        ("ai.provider.fake.key", String::new()),
        ("ai.level", script.level.clone()),
        ("ai.assistant.name", script.name.clone()),
    ] {
        v.insert(k.to_string(), Json::from(val));
    }
    let uid = usys::geteuid().ok().and_then(|u| u32::try_from(u).ok()).unwrap_or(0);
    let gate = Gate { uid, seen: Box::new(move |_| Peer { pid: 1, uid: Some(uid), exe: Some("/boot/BITSOS2/bin/bitscomp".into()), by: Source::Kernel }), holder: Box::new(|| Ok(None)) };
    let parts = Parts {
        settings: Arc::new(Mem(Mutex::new(v))),
        tools: Box::new(|_, seat: Seat<'_>| Box::new(FakeTools { desk: seat.desk.map(|d: &dyn Desk| DeskTools::new(d)) })),
        budget: Box::new(|| 64 << 20),
        gate,
        events: Events::system(),
        question_wait: super::QUESTION_WAIT,
        desk_wait: super::DESK_WAIT,
        stranger_idle: super::conn::STRANGER_IDLE,
        chats,
    };
    let ctx = super::start(parts)?;
    let l = std::os::unix::net::UnixListener::bind(listen)?;
    let c = ctx.clone();
    std::thread::spawn(move || super::serve(&c, &l));
    Ok(ctx)
}

/// A client line by line, for tests and for trying the fake by hand.
pub fn send(s: &mut std::os::unix::net::UnixStream, line: &Json) -> std::io::Result<()> {
    writeln!(s, "{}", line.to_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_and_its_rounds() {
        let s = Script::parse(r#"{"level":"auto","answers":[{"match":"such","rounds":[{"text":"Ich suche.","tool":"handeln","args":{"element":"w1/2"}},{"text":"Erledigt."}]},{"rounds":[{"text":"Sonst."}]}]}"#).unwrap();
        assert_eq!((s.level.as_str(), s.answers.len(), s.answers[0].rounds.len()), ("auto", 2, 2));
        let mut b = Brain { answers: s.answers, rounds: VecDeque::new(), n: 0 };
        let r = b.next("user", "bitte such nach Georg");
        assert_eq!((r.text.as_str(), r.tool.as_ref().map(|t| t.0.as_str())), ("Ich suche.", Some("handeln")));
        assert!(stream(&r, 1).contains(r#""finish_reason":"tool_calls""#));
        assert_eq!(b.next("tool", "").text, "Erledigt.");
        assert_eq!(b.next("user", "was anderes").text, "Sonst.");
        assert_eq!(b.next("user", "noch was").text, UNSCRIPTED);
        assert!(Script::parse("kein json").is_err());
    }
}
