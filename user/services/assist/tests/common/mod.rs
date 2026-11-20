//! What the tests of assist share: a real settingsd in this process (its registry on a fresh temporary stick,
//! reachable both directly and through its socket), a fake provider on a loopback port that answers from the
//! recordings in host/tests/cl/assist-rec, and a screen that writes everything down.
#![allow(dead_code)]

pub mod door;
pub mod stage;

use assist::config;
use assist::engine::{Answer, Ui};
use assist::json::{self, Json};
use assist::settings::{self, Refused, Settings};
use settingsd::events::Events;
use settingsd::peer::Peer;
use settingsd::proto::{self, Reply as ProtoReply};
use settingsd::registry::Registry;
use settingsd::server::{self, Shared};
use settingsd::store::Paths;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A key no test may ever see in output, logs, events or history.
pub const KEY: &str = "sk-test-7f3a9c-NEVER-SHOWN-b21e";

pub fn recording(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/cl/assist-rec").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

pub struct World {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub shared: Arc<Shared>,
    /// Every event settingsd would write to the kernel log.
    pub events: Arc<Mutex<Vec<String>>>,
    /// Who the requests made here come from: the `assist` program of this build, as the user of this test, so
    /// the real program started by a test owns the same secrets (settingsd tells owners by program, CL-S).
    pub me: Peer,
}

impl World {
    pub fn new(name: &str) -> World {
        // Under the target directory (cargo names it at build time), never in /tmp.
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("assist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // A chat removed here goes into a trash below the target directory, never into the host's own.
        assist::tools::native::trash::keep_tests_in(&PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("assist-trash").join(assist::tools::native::trash::FOLDER));
        std::fs::create_dir_all(root.join("sys")).unwrap();
        std::fs::create_dir_all(root.join("stick")).unwrap();
        let socket = root.join("settings.sock");
        let paths = Paths { stick: root.join("stick"), fallback: root.join("fb"), sys: root.join("sys"), socket: socket.clone() };
        let (ev, events) = Events::capture();
        let mut reg = Registry::open(paths, ev, Duration::from_millis(500));
        // As the kernel names the running program (a build directory may be reached through a link).
        let assist = std::fs::canonicalize(env!("CARGO_BIN_EXE_assist")).unwrap();
        reg.set_system_dir(assist.parent().unwrap());
        let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&root).unwrap());
        let me = Peer::program(uid, assist.to_str().unwrap());
        let shared = Arc::new(Shared::new(reg));
        World { root, socket, shared, events, me }
    }

    /// Serves the registry on its socket, for tests that start the real `assist` program. (Not through
    /// `server::bind`: that one changes the umask for a moment, which other test threads would trip over.)
    pub fn listen(&self) -> &std::path::Path {
        let listener = std::os::unix::net::UnixListener::bind(&self.socket).unwrap();
        let s = self.shared.clone();
        std::thread::spawn(move || server::serve(listener, s));
        &self.socket
    }

    /// The test's own setting up. Where only Georg's own yes counts (a level that allows more, leash.rs) it is
    /// his hand: root at the console, confirmed.
    pub fn set(&self, key: &str, value: impl Into<Json>) {
        let value = value.into();
        match settings::set(self, key, value.clone(), "test") {
            Ok(_) => {}
            Err(e) if e.code == "not_confirmer" => {
                let req = Json::obj().with("op", "set").with("key", key).with("value", value).with("source", "test").with("confirmed", true);
                let a = self.request_as(&Peer::program(0, "/bin/settings"), &req);
                assert_eq!(a.get("ok").and_then(Json::as_bool), Some(true), "set {key} as Georg: {}", a.to_text());
            }
            Err(e) => panic!("set {key}: {e}"),
        }
    }

    pub fn value(&self, key: &str) -> Json {
        settings::get(self, key).unwrap_or_else(|e| panic!("get {key}: {e}"))
    }

    /// Adds a provider the way `assist --add` and `settings set` do (settingsd makes the keys from the list).
    pub fn provider(&self, id: &str, kind: &str, address: &str, place: &str, key: Option<&str>) {
        let mut ids = config::parse_ids(self.value("ai.providers").as_str().unwrap_or(""));
        ids.push(id.to_string());
        self.set("ai.providers", ids.join(", "));
        self.set(&format!("ai.provider.{id}.kind"), kind);
        self.set(&format!("ai.provider.{id}.address"), address);
        self.set(&format!("ai.provider.{id}.model"), "model-rec");
        self.set(&format!("ai.provider.{id}.place"), place);
        if let Some(k) = key {
            self.set(&format!("ai.provider.{id}.key"), k);
        }
    }

    /// A request as settingsd would answer it to another program on its socket.
    pub fn request_as(&self, peer: &Peer, req: &Json) -> Json {
        match proto::handle(&self.shared, peer, &req.to_text()) {
            ProtoReply::Line(l) => json::parse(&l).unwrap(),
            ProtoReply::Watch(_) => panic!("a watch is not a request"),
        }
    }

    pub fn request_text(&self, req: &str) -> String {
        self.request(&json::parse(req).unwrap()).unwrap().to_text()
    }

    /// Everything settingsd wrote down anywhere: its events, its history, its files.
    pub fn everything_settingsd_kept(&self) -> String {
        let mut all = self.events.lock().unwrap().join("\n");
        all.push_str(&self.request(&Json::obj().with("op", "history").with("n", 1000i64)).unwrap().to_text());
        all.push_str(&self.request(&Json::obj().with("op", "list").with("prefix", "")).unwrap().to_text());
        all
    }
}

impl Settings for World {
    fn request(&self, req: &Json) -> Result<Json, Refused> {
        match proto::handle(&self.shared, &self.me, &req.to_text()) {
            ProtoReply::Line(l) => Ok(json::parse(&l).unwrap()),
            ProtoReply::Watch(_) => panic!("a watch is not a request"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Hit {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Json {
        json::parse(&self.body).unwrap()
    }
}

pub enum Reply {
    /// 200, chunked, complete.
    Sse(Vec<u8>),
    /// 200, chunked, the connection dies before the last chunk.
    Cut(Vec<u8>),
    /// 200, no framing: the body ends when the connection closes.
    Close(Vec<u8>),
    Status(u16, Vec<(&'static str, String)>, String),
    /// 200 and these bytes, then silence for as long as the client stays.
    Hang(Vec<u8>),
}

pub struct Fake {
    pub addr: SocketAddr,
    pub hits: Arc<Mutex<Vec<Hit>>>,
    pub connections: Arc<AtomicUsize>,
}

fn serve_one(mut s: TcpStream, hits: &Mutex<Vec<Hit>>, handler: &(dyn Fn(&Hit, usize) -> Reply + Send + Sync)) {
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split(' ').nth(1).unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        let (k, v) = line.trim().split_once(':').unwrap();
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).unwrap();
    let hit = Hit { path, headers, body: String::from_utf8(body).unwrap() };
    let n = {
        let mut h = hits.lock().unwrap();
        h.push(hit.clone());
        h.len()
    };
    let chunked = |s: &mut TcpStream, data: &[u8]| {
        // Small odd-sized chunks: a character or an event is split somewhere in every recording.
        for piece in data.chunks(37) {
            let _ = write!(s, "{:x}\r\n", piece.len());
            let _ = s.write_all(piece);
            let _ = s.write_all(b"\r\n");
            let _ = s.flush();
        }
    };
    const HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n";
    match handler(&hit, n) {
        Reply::Sse(data) => {
            let _ = write!(s, "{HEAD}Transfer-Encoding: chunked\r\n\r\n");
            chunked(&mut s, &data);
            let _ = s.write_all(b"0\r\n\r\n");
        }
        Reply::Cut(data) => {
            let _ = write!(s, "{HEAD}Transfer-Encoding: chunked\r\n\r\n");
            chunked(&mut s, &data);
        }
        Reply::Close(data) => {
            let _ = write!(s, "{HEAD}\r\n");
            let _ = s.write_all(&data);
        }
        Reply::Status(code, extra, body) => {
            let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n", body.len());
            for (k, v) in extra {
                let _ = write!(s, "{k}: {v}\r\n");
            }
            let _ = write!(s, "\r\n{body}");
        }
        Reply::Hang(data) => {
            let _ = write!(s, "{HEAD}Transfer-Encoding: chunked\r\n\r\n");
            chunked(&mut s, &data);
            // Until the client gives up and closes.
            let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
            let mut sink = [0u8; 64];
            while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
        }
    }
}

impl Fake {
    /// `handler` gets each request and its number (1 for the first).
    pub fn start(handler: impl Fn(&Hit, usize) -> Reply + Send + Sync + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let (h, c) = (hits.clone(), connections.clone());
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                c.fetch_add(1, Ordering::SeqCst);
                let (h, handler) = (h.clone(), handler.clone());
                std::thread::spawn(move || serve_one(s, &h, &*handler));
            }
        });
        Fake { addr, hits, connections }
    }

    /// The address a provider setting holds, with the path prefix its kind expects.
    pub fn address(&self, kind: &str) -> String {
        match kind {
            "anthropic" => format!("http://{}", self.addr),
            "gemini" => format!("http://{}/v1beta", self.addr),
            _ => format!("http://{}/v1", self.addr),
        }
    }
    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// An Anthropic stream with one tool call whose input is `input` (raw text), then the stop reason.
pub fn tool_stream(name: &str, input: &str, stop: &str) -> Vec<u8> {
    let quoted = Json::from(input).to_text();
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m\",\"content\":[]}}}}\n\n\
event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_h\",\"name\":\"{name}\",\"input\":{{}}}}}}\n\n\
event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{quoted}}}}}\n\n\
event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\"}}}}\n\n\
event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
    .into_bytes()
}

/// A screen that writes everything down and answers questions as told.
pub struct Rec {
    pub text: String,
    pub notes: Vec<String>,
    pub logs: Vec<String>,
    pub asked: Vec<String>,
    /// Every step of a tool: `name:State:text`.
    pub tools: Vec<String>,
    pub answer: Answer,
    /// Cancel once this much text arrived.
    pub cancel_at: Option<usize>,
    /// Esc a moment after a note with these words was shown (a tool at work).
    pub esc_after_note: Option<(&'static str, Duration)>,
    noted: Option<std::time::Instant>,
}

impl Rec {
    pub fn new(answer: Answer) -> Rec {
        Rec { text: String::new(), notes: Vec::new(), logs: Vec::new(), asked: Vec::new(), tools: Vec::new(), answer, cancel_at: None, esc_after_note: None, noted: None }
    }
    /// Everything Georg or a log reader could see.
    pub fn everything(&self) -> String {
        format!("{}\n{}\n{}\n{}", self.text, self.notes.join("\n"), self.logs.join("\n"), self.asked.join("\n"))
    }
}

impl Ui for Rec {
    fn text(&mut self, delta: &str) {
        self.text.push_str(delta);
    }
    fn note(&mut self, line: &str) {
        if self.noted.is_none() && self.esc_after_note.is_some_and(|(words, _)| line.contains(words)) {
            self.noted = Some(std::time::Instant::now());
        }
        self.notes.push(line.to_string());
    }
    fn log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }
    fn ask(&mut self, question: &str) -> Answer {
        self.asked.push(question.to_string());
        self.answer
    }
    fn tool(&mut self, name: &str, state: assist::engine::ToolState, text: &str) {
        self.tools.push(format!("{name}:{state:?}:{text}"));
        // As a screen without a place for tools shows them (the trait's own way).
        if matches!(state, assist::engine::ToolState::Runs | assist::engine::ToolState::Refused) {
            self.note(text);
        }
    }
    fn cancelled(&mut self) -> bool {
        let esc = matches!((self.noted, self.esc_after_note), (Some(at), Some((_, wait))) if at.elapsed() >= wait);
        esc || self.cancel_at.is_some_and(|n| self.text.len() >= n)
    }
}
