//! A stand-in for the kernel's local tools socket (MC-L): a unix socket under the target directory that
//! takes one JSON-RPC line per connection and answers as the test says.

use assist::json::{self, Json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum Say {
    /// The `result` of the answer (the envelope with the request's id is added).
    Result(Json),
    /// A JSON-RPC error with the request's id.
    Error(i64, String),
    /// These bytes as they are.
    Bytes(Vec<u8>),
    /// Nothing, for as long as the client stays.
    Hang,
    /// Closes without a word.
    Close,
}

pub struct Door {
    pub path: PathBuf,
    /// Every request line that arrived, parsed.
    pub asked: Arc<Mutex<Vec<Json>>>,
    /// Connections the client closed while the door was silent.
    pub given_up: Arc<AtomicUsize>,
}

/// One entry of `tools/list` as the kernel writes it.
pub fn tool(name: &str, safety: &str, owner: &str, schema: &str) -> Json {
    let verified = owner != "stranger";
    let mut notes = Json::obj().with("readOnlyHint", verified && safety == "read").with("destructiveHint", !verified || safety == "delete_overwrite").with("owner", owner).with("safety", safety);
    if owner != "kernel" {
        notes = notes.with("service_pid", 7i64).with("verified", verified);
    }
    Json::obj().with("name", name).with("description", format!("The tool {name}.")).with("inputSchema", json::parse(schema).unwrap()).with("annotations", notes)
}

pub const NO_ARGS: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;

pub fn listing(tools: Vec<Json>) -> Say {
    Say::Result(Json::obj().with("tools", tools))
}

pub fn text(text: &str, is_error: bool) -> Say {
    Say::Result(Json::obj().with("content", vec![Json::obj().with("type", "text").with("text", text)]).with("isError", is_error))
}

pub fn refusal(why: &str) -> Vec<u8> {
    let error = Json::obj().with("code", -32001i64).with("message", format!("Refused: {why}"));
    let mut line = Json::obj().with("jsonrpc", "2.0").with("id", Json::Null).with("error", error).to_text();
    line.push('\n');
    line.into_bytes()
}

fn serve(mut s: UnixStream, asked: &Mutex<Vec<Json>>, given_up: &AtomicUsize, handler: &(dyn Fn(&Json, usize) -> Say + Send + Sync)) {
    let mut line = String::new();
    if BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let req = json::parse(line.trim()).unwrap();
    let n = {
        let mut a = asked.lock().unwrap();
        a.push(req.clone());
        a.len()
    };
    let id = req.get("id").cloned().unwrap_or(Json::Null);
    let envelope = |field: &str, v: Json| format!("{}\n", Json::obj().with("jsonrpc", "2.0").with("id", id.clone()).with(field, v).to_text()).into_bytes();
    let bytes = match handler(&req, n) {
        Say::Result(r) => envelope("result", r),
        Say::Error(code, message) => envelope("error", Json::obj().with("code", code).with("message", message)),
        Say::Bytes(b) => b,
        Say::Close => return,
        Say::Hang => {
            let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
            let mut sink = [0u8; 64];
            if matches!(s.read(&mut sink), Ok(0)) {
                given_up.fetch_add(1, Ordering::SeqCst);
            }
            return;
        }
    };
    let _ = s.write_all(&bytes);
}

impl Door {
    fn bind(name: &str) -> (UnixListener, Door) {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("door-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        (listener, Door { path, asked: Arc::default(), given_up: Arc::default() })
    }

    /// `handler` gets each request and its number (1 for the first).
    pub fn start(name: &str, handler: impl Fn(&Json, usize) -> Say + Send + Sync + 'static) -> Door {
        let (listener, door) = Door::bind(name);
        let (asked, given_up, handler) = (door.asked.clone(), door.given_up.clone(), Arc::new(handler));
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let (asked, given_up, handler) = (asked.clone(), given_up.clone(), handler.clone());
                std::thread::spawn(move || serve(s, &asked, &given_up, &*handler));
            }
        });
        door
    }

    /// The door for a program it does not admit: the refusal at once, unasked, then the end.
    pub fn refusing(name: &str, why: &'static str) -> Door {
        let (listener, door) = Door::bind(name);
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let _ = s.write_all(&refusal(why));
            }
        });
        door
    }

    /// The tools that were called, in order.
    pub fn called(&self) -> Vec<String> {
        self.asked.lock().unwrap().iter().filter(|r| r.str("method") == Some("tools/call")).filter_map(|r| r.get("params")?.str("name").map(str::to_string)).collect()
    }

    pub fn lists(&self) -> usize {
        self.asked.lock().unwrap().iter().filter(|r| r.str("method") == Some("tools/list")).count()
    }
}

impl Drop for Door {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
