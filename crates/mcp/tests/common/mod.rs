//! Shared pieces of the host tests: a server on 127.0.0.1 in the MC port range (25000-25499), a raw HTTP
//! client like V1's `client()` in mcp-http-test.mjs, and test tools.
#![allow(dead_code)]

use mcp::host::StdEnv;
use mcp::{Json, ServerConfig, ServerHandle, ToolContext, ToolOutput, ToolResult};
use net::host::StdSockets;
use net::{Ipv4, SockAddr};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

pub const TOKEN: &str = "test-token-1234";
pub const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
static STD: StdSockets = StdSockets;

pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        klog::heap_ready();
        klog::set_serial(false);
    });
}

/// A port nobody listens on now (asked from the OS): fixed ports collided when several worktrees ran their
/// gates at once (`Listen(AddrInUse)`).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).expect("a free port")
}

pub fn start(port: u16, tweak: impl FnOnce(&mut ServerConfig)) -> ServerHandle {
    init();
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port };
    tweak(&mut cfg);
    mcp::start(&STD, Arc::new(StdEnv), cfg, mcp::TOOLS.entries()).expect("server start")
}

/// A server on a port the OS called free: (port, server). `free_port` alone leaves a window between asking and
/// binding in which another gate's test (the desk's checkout and the lane worktrees run the same tests) can take
/// the port; a taken port is simply asked for again.
pub fn serve(tweak: impl Fn(&mut ServerConfig)) -> (u16, ServerHandle) {
    init();
    for _ in 0..50 {
        let port = free_port();
        let mut cfg = ServerConfig::new(TOKEN);
        cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port };
        tweak(&mut cfg);
        match mcp::start(&STD, Arc::new(StdEnv), cfg, mcp::TOOLS.entries()) {
            Ok(h) => return (port, h),
            Err(mcp::StartError::Listen(net::NetError::AddrInUse)) => continue,
            Err(e) => panic!("server start: {e:?}"),
        }
    }
    panic!("no free port in 50 tries");
}

pub fn stop(s: ServerHandle) {
    s.stop();
    assert!(s.wait_stopped(5_000_000_000), "server threads did not end");
}

pub struct Req<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub token: Option<&'a str>,
    pub body: Option<&'a str>,
    pub headers: Vec<String>,
    pub version: &'a str,
    pub eol: &'a str,
}

impl Default for Req<'_> {
    fn default() -> Self {
        Req { method: "POST", path: "/mcp", token: Some(TOKEN), body: Some(PING), headers: vec![], version: "1.1", eol: "\r\n" }
    }
}

impl Req<'_> {
    pub fn text(&self) -> String {
        let mut lines = vec![format!("{} {} HTTP/{}", self.method, self.path, self.version), "Host: bitsos".into()];
        if let Some(t) = self.token {
            lines.push(format!("Authorization: Bearer {t}"));
        }
        if let Some(b) = self.body {
            lines.push("Content-Type: application/json".into());
            lines.push(format!("Content-Length: {}", b.len()));
        }
        lines.extend(self.headers.iter().cloned());
        format!("{}{}{}{}", lines.join(self.eol), self.eol, self.eol, self.body.unwrap_or(""))
    }
}

pub fn req() -> String {
    Req::default().text()
}

pub fn rpc(body: &str) -> String {
    Req { body: Some(body), ..Default::default() }.text()
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub head: String,
    pub body: String,
    pub at: Instant,
}

impl Answer {
    pub fn json(&self) -> Json {
        Json::parse(&self.body).unwrap_or_else(|_| panic!("no JSON: {}", self.body))
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.split("\r\n").find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()))
    }
    /// V1's `mcpOk`: 200, keep-alive, JSON content type when there is a body.
    pub fn mcp_ok(&self) -> bool {
        self.status == 200 && self.head.to_ascii_lowercase().ends_with("\r\nconnection: keep-alive") && (self.body.is_empty() || self.header("content-type").as_deref() == Some("application/json"))
    }
    pub fn closes(&self) -> bool {
        self.head.to_ascii_lowercase().ends_with("\r\nconnection: close")
    }
}

/// A raw client on its own socket; a reader thread collects what comes, `answer` parses it in order.
pub struct Client {
    s: TcpStream,
    rx: Arc<Mutex<(Vec<u8>, bool)>>,
}

impl Client {
    pub fn new(port: u16) -> Client {
        let s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.set_nodelay(true).ok();
        let rx = Arc::new(Mutex::new((Vec::new(), false)));
        let (mut r, rx2) = (s.try_clone().unwrap(), rx.clone());
        std::thread::spawn(move || {
            let mut b = [0u8; 65536];
            loop {
                match r.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => rx2.lock().unwrap().0.extend_from_slice(&b[..n]),
                }
            }
            rx2.lock().unwrap().1 = true;
        });
        Client { s, rx }
    }
    pub fn write(&mut self, d: impl AsRef<[u8]>) {
        let _ = self.s.write_all(d.as_ref());
    }
    pub fn ended(&self) -> bool {
        self.rx.lock().unwrap().1
    }
    pub fn wait_ended(&self, ms: u64) -> bool {
        until(ms, || self.ended())
    }
    fn parse(&self) -> Option<Answer> {
        let mut g = self.rx.lock().unwrap();
        let buf = &g.0;
        let i = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&buf[..i]).into_owned();
        let n = head.split("\r\n").find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
        if buf.len() < i + 4 + n {
            return None;
        }
        let body = String::from_utf8_lossy(&buf[i + 4..i + 4 + n]).into_owned();
        g.0.drain(..i + 4 + n);
        let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Some(Answer { status, head, body, at: Instant::now() })
    }
    /// The next whole answer, or None after `ms`.
    pub fn answer(&self, ms: u64) -> Option<Answer> {
        let end = Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(a) = self.parse() {
                return Some(a);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    pub fn close(self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
    }
}

pub fn until(ms: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn sleep(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// One request on a connection of its own (V1's `ask`): the answer and whether the server closed after it.
pub fn ask(port: u16, data: &str, ms: u64) -> Option<(Answer, bool)> {
    let mut c = Client::new(port);
    c.write(data);
    let a = c.answer(ms)?;
    let closed = a.closes() && c.wait_ended(5000);
    c.close();
    Some((a, closed))
}

// Test tools (registered only in the test binaries).
fn t_echo(_: &ToolContext, a: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(a.get("text").and_then(Json::as_str).unwrap_or("").to_string())])
}
mcp::mcp_tool!(name: "t_echo", owner: "MC", safety: Read, description: "Test: echoes text.", schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#, handler: t_echo);

fn t_sleep(ctx: &ToolContext, a: &Json) -> ToolResult {
    let ms = a.get("ms").and_then(Json::as_u64).unwrap_or(1000);
    std::thread::sleep(Duration::from_millis(ms));
    Ok(vec![ToolOutput::Text(format!("slept {ms} ms, deadline {}", ctx.deadline_ns))])
}
mcp::mcp_tool!(name: "t_sleep", owner: "MC", safety: Read, description: "Test: sleeps.", schema: r#"{"type":"object","properties":{"ms":{"type":"integer"}}}"#, handler: t_sleep);

fn t_panic(_: &ToolContext, _: &Json) -> ToolResult {
    panic!("t_panic panics on purpose");
}
mcp::mcp_tool!(name: "t_panic", owner: "MC", safety: Read, description: "Test: panics.", schema: r#"{"type":"object","properties":{}}"#, handler: t_panic);

/// The PNG bytes `t_png` answers with: `n` bytes of a fixed pattern.
pub fn png_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i ^ (i >> 8) ^ (i >> 16)) as u8).collect()
}
fn t_png(_: &ToolContext, a: &Json) -> ToolResult {
    let n = a.get("bytes").and_then(Json::as_u64).unwrap_or(16) as usize;
    Ok(vec![ToolOutput::Text("picture".into()), ToolOutput::Png(png_bytes(n))])
}
mcp::mcp_tool!(name: "t_png", owner: "DP", safety: Read, description: "Test: a PNG of n bytes.", schema: r#"{"type":"object","properties":{"bytes":{"type":"integer"}}}"#, handler: t_png);

/// Standard base64 decoder for checking image content.
pub fn b64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("not base64: {c}"),
    } as u32;
    let mut out = Vec::new();
    for ch in s.as_bytes().chunks(4) {
        let pad = ch.iter().filter(|&&c| c == b'=').count();
        let n = ch.iter().filter(|&&c| c != b'=').fold(0u32, |acc, &c| acc << 6 | val(c)) << (6 * pad);
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    out
}

/// Log events `mcp http.request` whose text contains `needle`.
pub fn request_events(needle: &str) -> Vec<String> {
    klog::tail(100_000).into_iter().filter(|r| r.subsystem == "mcp" && r.name == "http.request" && r.text.contains(needle)).map(|r| r.text).collect()
}
pub fn events(name: &str) -> Vec<String> {
    klog::tail(100_000).into_iter().filter(|r| r.subsystem == "mcp" && r.name == name).map(|r| r.text).collect()
}

/// Loops for `ms` (default 10 s), writing "tick n" to its output every 50 ms; ends at the next look once stop is
/// asked, unless `ignore_stop`.
fn t_spin(ctx: &ToolContext, a: &Json) -> ToolResult {
    let ms = a.get("ms").and_then(Json::as_u64).unwrap_or(10_000);
    let ignore = a.get("ignore_stop").and_then(Json::as_bool).unwrap_or(false);
    // MC-5b: as if it ran program `pid` (output/stop by pid; the grace it was given shows in the result).
    let job = a.get("pid").and_then(Json::as_u64);
    if let Some(pid) = job {
        ctx.set_job_pid(pid as u32);
    }
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_millis(ms) {
        if !ignore && ctx.stop_requested() {
            let grace = if job.is_some() { format!(", grace {} ms", ctx.stop_grace_ns() / 1_000_000) } else { String::new() };
            return Ok(vec![ToolOutput::Text(format!("spun {n}, stopped{grace}"))]);
        }
        ctx.output(&format!("tick {n}\n"));
        n += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(vec![ToolOutput::Text(format!("spun {n}, done"))])
}
mcp::mcp_tool!(name: "t_spin", owner: "MC", safety: Read, description: "Test: loops, writes output, stops when asked.", schema: r#"{"type":"object","properties":{"ms":{"type":"integer"},"ignore_stop":{"type":"boolean"},"pid":{"type":"integer"}}}"#, handler: t_spin);

/// When the action after an answer ran (tag, instant).
pub static AFTER_RAN: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());

/// Registers an action after its answer (tagged), then sleeps `ms` (default 0) before returning.
fn t_after(ctx: &ToolContext, a: &Json) -> ToolResult {
    let tag = a.get("tag").and_then(Json::as_str).unwrap_or("").to_string();
    let t2 = tag.clone();
    let ok = ctx.after_answer(Box::new(move || AFTER_RAN.lock().unwrap().push((t2, Instant::now()))));
    std::thread::sleep(Duration::from_millis(a.get("ms").and_then(Json::as_u64).unwrap_or(0)));
    Ok(vec![ToolOutput::Text(format!("after {tag} registered={ok}"))])
}
mcp::mcp_tool!(name: "t_after", owner: "MC", safety: Change, description: "Test: an action after the answer.", schema: r#"{"type":"object","properties":{"tag":{"type":"string"},"ms":{"type":"integer"}}}"#, handler: t_after);

pub fn after_ran(tag: &str) -> Option<Instant> {
    AFTER_RAN.lock().unwrap().iter().find(|(t, _)| t == tag).map(|(_, i)| *i)
}

/// A tools/call request body.
pub fn tool_call(id: u64, name: &str, args: &str) -> String {
    rpc(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#))
}

/// The tools/call result object of an answer.
pub fn result_of(a: &Answer) -> Json {
    assert_eq!(a.status, 200, "{a:?}");
    a.json().get("result").cloned().unwrap_or_else(|| panic!("no result: {}", a.body))
}

/// Every text content item of a result, joined by newlines.
pub fn texts(a: &Answer) -> String {
    let r = result_of(a);
    let items = r.get("content").and_then(Json::as_array).map(|c| c.to_vec()).unwrap_or_default();
    items.iter().filter_map(|c| c.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n")
}

pub fn is_error(a: &Answer) -> bool {
    result_of(a).get("isError").and_then(Json::as_bool) == Some(true)
}

/// The call number in a "Still running: ... (call n)" answer.
pub fn call_number(a: &Answer) -> u64 {
    let t = texts(a);
    let i = t.find("(call ").unwrap_or_else(|| panic!("no call number: {t}"));
    t[i + 6..].split(')').next().unwrap().parse().unwrap()
}

/// `bytes` bytes of text (`unit` repeated, default "x").
fn t_text(_: &ToolContext, a: &Json) -> ToolResult {
    let n = a.get("bytes").and_then(Json::as_u64).unwrap_or(16) as usize;
    let unit = a.get("unit").and_then(Json::as_str).unwrap_or("x");
    Ok(vec![ToolOutput::Text(unit.repeat(n / unit.len().max(1)))])
}
mcp::mcp_tool!(name: "t_text", owner: "MC", safety: Read, description: "Test: n bytes of text.", schema: r#"{"type":"object","properties":{"bytes":{"type":"integer"},"unit":{"type":"string"}}}"#, handler: t_text);
