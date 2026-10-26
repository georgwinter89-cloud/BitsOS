//! mcpreg: a Rust service registers MCP tools with the BitsOS kernel (MC-6) and answers their calls. The kernel's
//! MCP server lists them next to its own tools (owner = the service's name) and forwards each call with a
//! deadline; the tools vanish when the service's connection ends.
//!
//! ```no_run
//! use mcpreg::{Reply, Safety, Service, Tool};
//! let mut s = Service::connect("settingsd")?;
//! s.register(&[Tool::new("settings_get", "Value of a setting.", r#"{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}"#, Safety::Read)?])?;
//! s.serve(|call, _| match call.arguments.get("key").and_then(|k| k.as_str()) {
//!     Some(k) => Reply::text(format!("{k} = 1")),
//!     None => Reply::bad_argument("key"),
//! })?;
//! # Ok::<(), mcpreg::Error>(())
//! ```
//!
//! The wire protocol (JSON-RPC 2.0, one message per line) is described in the kernel's `crates/mcp/src/svc/proto.rs`.
//! A call comes with `timeout_ms`, the time its caller waits; an answer after it is dropped (the kernel sends
//! `Event::Cancelled` then). `serve` handles one call at a time; a service with slow tools takes `next` and
//! answers from threads of its own through a `Replier`.
//!
//! Rules the kernel applies (until PM decides who may register what): connect and `register` within 10 s, or the
//! connection is closed; service and tool names starting with `settings` are for services
//! running as uid 0; a service of another uid is listed with cautious hints (`readOnlyHint` false,
//! `destructiveHint` true, `verified` false) whatever safety it declares. The socket is opened close-on-exec
//! (std); a service that forks keeps working, but the kernel drops its tools once the registering process
//! has ended, even while a child still holds the socket.

pub mod json;

pub use json::Json;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

/// Where the kernel listens; `BITSOS_MCP_SOCK` names another socket (tests).
pub const DEFAULT_SOCKET: &str = "/run/bitsos/mcp.sock";
/// Longest line taken from the kernel (its calls carry the client's arguments, at most a few MB).
const MAX_LINE: usize = 16 << 20;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The kernel refused a request (its message names the reason, e.g. the first bad tool).
    Refused(String),
    /// The kernel sent something this library does not understand, or closed the connection.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Refused(m) => write!(f, "refused: {m}"),
            Error::Protocol(m) => write!(f, "protocol: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

/// B5 safety class: `read` looks only, `change` changes state, `delete_overwrite` destroys or replaces data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    Read,
    Change,
    DeleteOverwrite,
}

impl Safety {
    pub fn as_str(self) -> &'static str {
        match self {
            Safety::Read => "read",
            Safety::Change => "change",
            Safety::DeleteOverwrite => "delete_overwrite",
        }
    }
}

/// A tool to register.
#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Json,
    pub safety: Safety,
}

impl Tool {
    /// `schema`: the JSON schema of the arguments as text (an object of type "object").
    pub fn new(name: &str, description: &str, schema: &str, safety: Safety) -> Result<Tool, Error> {
        let input_schema = Json::parse(schema).map_err(|e| Error::Refused(format!("tool {name}: schema is not JSON: {e}")))?;
        Ok(Tool { name: name.to_string(), description: description.to_string(), input_schema, safety })
    }
    fn to_json(&self) -> Json {
        Json::obj(vec![("name", Json::str(&self.name)), ("description", Json::str(&self.description)), ("inputSchema", self.input_schema.clone()), ("safety", Json::str(self.safety.as_str()))])
    }
}

/// A call the kernel forwards.
#[derive(Debug, Clone)]
pub struct Call {
    pub id: u64,
    pub name: String,
    pub arguments: Json,
    /// How long the caller waits (ms, from when the kernel sent it).
    pub timeout_ms: u64,
}

/// What comes from the kernel.
#[derive(Debug, Clone)]
pub enum Event {
    Call(Call),
    /// The caller's deadline passed or `stop` was asked: an answer to `id` is dropped.
    Cancelled { id: u64, reason: String },
}

/// One item of an answer.
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    /// PNG bytes.
    Png(Vec<u8>),
}

/// The answer to a call.
#[derive(Debug, Clone)]
pub enum Reply {
    Ok(Vec<Content>),
    /// A failed call (`isError`; the caller sees "ERROR: text").
    Error(String),
    /// A bad argument (JSON-RPC -32602, "Bad argument: text").
    BadArgument(String),
}

impl Reply {
    pub fn text(t: impl Into<String>) -> Reply {
        Reply::Ok(vec![Content::Text(t.into())])
    }
    pub fn error(t: impl Into<String>) -> Reply {
        Reply::Error(t.into())
    }
    pub fn bad_argument(t: impl Into<String>) -> Reply {
        Reply::BadArgument(t.into())
    }
}

/// Sends answers and progress from any thread.
#[derive(Clone)]
pub struct Replier {
    w: Arc<Mutex<UnixStream>>,
}

impl Replier {
    fn send(&self, line: &str) -> Result<(), Error> {
        let mut w = self.w.lock().map_err(|_| Error::Protocol("writer poisoned".into()))?;
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
        Ok(())
    }

    pub fn reply(&self, id: u64, r: Reply) -> Result<(), Error> {
        let mut s = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},");
        match r {
            Reply::Ok(items) => {
                s.push_str("\"result\":{\"content\":[");
                for (i, c) in items.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    match c {
                        Content::Text(t) => {
                            s.push_str("{\"type\":\"text\",\"text\":");
                            json::escape_into(t, &mut s);
                            s.push('}');
                        }
                        Content::Png(p) => {
                            s.push_str("{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"");
                            base64_into(p, &mut s);
                            s.push_str("\"}");
                        }
                    }
                }
                s.push_str("],\"isError\":false}}");
            }
            Reply::Error(t) => {
                s.push_str("\"result\":{\"content\":[{\"type\":\"text\",\"text\":");
                json::escape_into(&t, &mut s);
                s.push_str("}],\"isError\":true}}");
            }
            Reply::BadArgument(t) => {
                s.push_str("\"error\":{\"code\":-32602,\"message\":");
                json::escape_into(&t, &mut s);
                s.push_str("}}");
            }
        }
        self.send(&s)
    }

    /// Progress text for the kernel's `output` tool (shown while the call runs).
    pub fn progress(&self, id: u64, text: &str) -> Result<(), Error> {
        let mut s = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"progressToken\":{id},\"progress\":0,\"message\":");
        json::escape_into(text, &mut s);
        s.push_str("}}");
        self.send(&s)
    }
}

/// A connection to the kernel's registry.
pub struct Service {
    name: String,
    r: BufReader<UnixStream>,
    w: Replier,
    next_id: u64,
    /// Events that came while `register`/`unregister` waited for their answer.
    queued: VecDeque<Event>,
}

impl Service {
    /// Connects to `BITSOS_MCP_SOCK` or `/run/bitsos/mcp.sock`. `name`: the service's name (a-z, 0-9, _ and -,
    /// starting with a letter; the owner of its tools).
    pub fn connect(name: &str) -> Result<Service, Error> {
        let path = std::env::var("BITSOS_MCP_SOCK").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
        Service::connect_at(&path, name)
    }

    pub fn connect_at(path: &str, name: &str) -> Result<Service, Error> {
        let s = UnixStream::connect(path)?;
        let w = s.try_clone()?;
        Ok(Service { name: name.to_string(), r: BufReader::new(s), w: Replier { w: Arc::new(Mutex::new(w)) }, next_id: 1, queued: VecDeque::new() })
    }

    pub fn replier(&self) -> Replier {
        self.w.clone()
    }

    /// Registers tools (all or none); the kernel answers with their names.
    pub fn register(&mut self, tools: &[Tool]) -> Result<Vec<String>, Error> {
        let params = Json::obj(vec![("service", Json::str(&self.name)), ("tools", Json::Arr(tools.iter().map(Tool::to_json).collect()))]);
        let r = self.request("register", params)?;
        names(&r, "registered")
    }

    pub fn unregister(&mut self, tools: &[&str]) -> Result<Vec<String>, Error> {
        let params = Json::obj(vec![("tools", Json::Arr(tools.iter().map(|t| Json::str(t)).collect()))]);
        let r = self.request("unregister", params)?;
        names(&r, "unregistered")
    }

    fn request(&mut self, method: &str, params: Json) -> Result<Json, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = Json::obj(vec![("jsonrpc", Json::str("2.0")), ("id", Json::Num(id as f64)), ("method", Json::str(method)), ("params", params)]);
        self.w.send(&msg.to_string())?;
        loop {
            let m = self.read_msg()?.ok_or_else(|| Error::Protocol("the kernel closed the connection".into()))?;
            if m.get("method").is_none() && m.get("id").and_then(Json::as_u64) == Some(id) {
                if let Some(e) = m.get("error") {
                    return Err(Error::Refused(e.get("message").and_then(Json::as_str).unwrap_or("no message").to_string()));
                }
                return m.get("result").cloned().ok_or_else(|| Error::Protocol("an answer without result".into()));
            }
            if let Some(ev) = event_of(&m) {
                self.queued.push_back(ev);
            }
        }
    }

    fn read_msg(&mut self) -> Result<Option<Json>, Error> {
        loop {
            let mut line = Vec::new();
            let n = (&mut self.r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
            if n == 0 {
                return Ok(None);
            }
            if line.len() > MAX_LINE {
                return Err(Error::Protocol("a line longer than 16 MiB".into()));
            }
            let t = String::from_utf8_lossy(&line);
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            return Json::parse(t).map(Some).map_err(|e| Error::Protocol(format!("not JSON: {e}")));
        }
    }

    /// The next call or cancellation; None when the kernel closed the connection (the registry stopped).
    pub fn next(&mut self) -> Result<Option<Event>, Error> {
        if let Some(e) = self.queued.pop_front() {
            return Ok(Some(e));
        }
        loop {
            let Some(m) = self.read_msg()? else { return Ok(None) };
            if let Some(ev) = event_of(&m) {
                return Ok(Some(ev));
            }
        }
    }

    /// Answers calls with `handler`, one at a time, until the kernel closes the connection.
    pub fn serve(&mut self, mut handler: impl FnMut(&Call, &Replier) -> Reply) -> Result<(), Error> {
        while let Some(ev) = self.next()? {
            if let Event::Call(c) = ev {
                let r = handler(&c, &self.w);
                self.w.reply(c.id, r)?;
            }
        }
        Ok(())
    }
}

fn names(r: &Json, key: &str) -> Result<Vec<String>, Error> {
    let a = r.get(key).and_then(Json::as_array).ok_or_else(|| Error::Protocol(format!("no {key} list")))?;
    Ok(a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
}

fn event_of(m: &Json) -> Option<Event> {
    let p = m.get("params");
    match m.get("method").and_then(Json::as_str)? {
        "tools/call" => Some(Event::Call(Call {
            id: m.get("id").and_then(Json::as_u64)?,
            name: p.and_then(|p| p.get("name")).and_then(Json::as_str).unwrap_or("").to_string(),
            arguments: p.and_then(|p| p.get("arguments")).cloned().unwrap_or(Json::Obj(Vec::new())),
            timeout_ms: p.and_then(|p| p.get("_meta")).and_then(|m| m.get("timeout_ms")).and_then(Json::as_u64).unwrap_or(0),
        })),
        "notifications/cancelled" => Some(Event::Cancelled {
            id: p.and_then(|p| p.get("requestId")).and_then(Json::as_u64)?,
            reason: p.and_then(|p| p.get("reason")).and_then(Json::as_str).unwrap_or("").to_string(),
        }),
        _ => None,
    }
}

fn base64_into(data: &[u8], out: &mut String) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A fake kernel: answers register, sends one call and a cancel, and checks the answer.
    #[test]
    fn register_serve_and_reply() {
        let dir = std::env::temp_dir().join(format!("mcpreg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("k.sock");
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).unwrap();
        let k = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let m = Json::parse(line.trim()).unwrap();
            assert_eq!(m.get("method").and_then(Json::as_str), Some("register"));
            let p = m.get("params").unwrap();
            assert_eq!(p.get("service").and_then(Json::as_str), Some("libtest"));
            let t = &p.get("tools").unwrap().as_array().unwrap()[0];
            assert_eq!(t.get("safety").and_then(Json::as_str), Some("change"));
            // A call before the answer is queued, not lost.
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"t\",\"arguments\":{\"x\":\"a\\\"b\"},\"_meta\":{\"progressToken\":5,\"timeout_ms\":900}}}\n").unwrap();
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"registered\":[\"t\"]}}\n").unwrap();
            let mut a = String::new();
            r.read_line(&mut a).unwrap();
            let pr = Json::parse(a.trim()).unwrap();
            assert_eq!(pr.get("method").and_then(Json::as_str), Some("notifications/progress"));
            a.clear();
            r.read_line(&mut a).unwrap();
            let ans = Json::parse(a.trim()).unwrap();
            assert_eq!(ans.get("id").and_then(Json::as_u64), Some(5));
            let c = &ans.get("result").unwrap().get("content").unwrap().as_array().unwrap()[0];
            assert_eq!(c.get("text").and_then(Json::as_str), Some("got a\"b in 900"));
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":5,\"reason\":\"timeout\"}}\n").unwrap();
        });
        let mut s = Service::connect_at(path.to_str().unwrap(), "libtest").unwrap();
        let names = s.register(&[Tool::new("t", "test", r#"{"type":"object"}"#, Safety::Change).unwrap()]).unwrap();
        assert_eq!(names, vec!["t".to_string()]);
        let Some(Event::Call(c)) = s.next().unwrap() else { panic!("no call") };
        s.replier().progress(c.id, "half").unwrap();
        let x = c.arguments.get("x").and_then(Json::as_str).unwrap().to_string();
        s.replier().reply(c.id, Reply::text(format!("got {x} in {}", c.timeout_ms))).unwrap();
        let Some(Event::Cancelled { id, reason }) = s.next().unwrap() else { panic!("no cancel") };
        assert_eq!((id, reason.as_str()), (5, "timeout"));
        k.join().unwrap();
        assert!(s.next().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn b64() {
        let mut s = String::new();
        base64_into(b"\x89PNG", &mut s);
        assert_eq!(s, "iVBORw==");
    }
}
