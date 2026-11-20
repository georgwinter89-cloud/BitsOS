//! MC-L host tests: the local tools socket (`mcp::local`) over std's Unix sockets. Who is admitted (the door is
//! the test's own here; the kernel's door over real processes is `mc.local.user_*` in crates/proc), what it
//! lists and calls (only tools marked `local`), and that a client that misbehaves costs only its own
//! connection: the network endpoint beside it keeps answering.

mod common;
use common::*;
use mcp::host::{StdEnv, StdListen};
use mcp::local::{Door, Local, Pass, Peer};
use mcp::{Json, Registry, ServerConfig, ServerHandle, ToolContext, ToolOutput, ToolResult};
use net::host::StdSockets;
use net::{Ipv4, SockAddr};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static STD: StdSockets = StdSockets;

// Local test tools (this binary only).
fn l_info(_: &ToolContext, a: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(format!("info {}", a.get("text").and_then(Json::as_str).unwrap_or("")))])
}
mcp::mcp_tool!(name: "l_info", owner: "MC", safety: Read, local: true, description: "Test: local information.", schema: r#"{"type":"object","properties":{"text":{"type":"string"}}}"#, handler: l_info);

static SLOW_STOPPED: AtomicBool = AtomicBool::new(false);
fn l_slow(ctx: &ToolContext, a: &Json) -> ToolResult {
    let ms = a.get("ms").and_then(Json::as_u64).unwrap_or(1000);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        if ctx.stop_requested() {
            SLOW_STOPPED.store(true, Ordering::Release);
            return Ok(vec![ToolOutput::Text("stopped".into())]);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(vec![ToolOutput::Text(format!("slept {ms}"))])
}
mcp::mcp_tool!(name: "l_slow", owner: "MC", safety: Read, local: true, description: "Test: slow.", schema: r#"{"type":"object","properties":{"ms":{"type":"integer"}}}"#, handler: l_slow);

fn l_big(_: &ToolContext, a: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text("x".repeat(a.get("bytes").and_then(Json::as_u64).unwrap_or(1) as usize))])
}
mcp::mcp_tool!(name: "l_big", owner: "MC", safety: Read, local: true, description: "Test: a long answer.", schema: r#"{"type":"object","properties":{"bytes":{"type":"integer"}}}"#, handler: l_big);

static AFTER: AtomicBool = AtomicBool::new(false);
fn l_after(ctx: &ToolContext, _: &Json) -> ToolResult {
    ctx.after_answer(Box::new(|| AFTER.store(true, Ordering::Release)));
    Ok(vec![ToolOutput::Text("asked".into())])
}
mcp::mcp_tool!(name: "l_after", owner: "MC", safety: Change, local: true, description: "Test: asks for an action after its answer.", schema: r#"{"type":"object","properties":{}}"#, handler: l_after);

/// Does not look at `stop`: runs until the test lets it go.
static DEAF_RELEASE: AtomicBool = AtomicBool::new(false);
fn l_deaf(_: &ToolContext, _: &Json) -> ToolResult {
    let t0 = Instant::now();
    while !DEAF_RELEASE.load(Ordering::Acquire) && t0.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(vec![ToolOutput::Text("let go".into())])
}
mcp::mcp_tool!(name: "l_deaf", owner: "MC", safety: Read, local: true, description: "Test: ignores stop.", schema: r#"{"type":"object","properties":{}}"#, handler: l_deaf);

// Stands for klogd's tool of this name (not linked into this test): since/until reach the journal there.
fn metrics(_: &ToolContext, a: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(format!("metrics {}", a.to_string()))])
}
mcp::mcp_tool!(name: "metrics", owner: "RB", safety: Read, local: true, description: "Test: metrics.", schema: r#"{"type":"object","properties":{"since":{"type":"string"},"until":{"type":"string"},"name":{"type":"string"}}}"#, handler: metrics);

/// The test's door: admits the pids in `assist` and asks again before every request.
struct TestDoor {
    assist: Arc<Mutex<HashSet<u32>>>,
}
struct TestPass {
    assist: Arc<Mutex<HashSet<u32>>>,
    peer: Peer,
}
fn judge(assist: &Mutex<HashSet<u32>>, peer: Peer) -> Result<(), String> {
    if assist.lock().unwrap().contains(&peer.pid) { Ok(()) } else { Err(format!("pid {} does not run the boot's /bin/assist", peer.pid)) }
}
impl Door for TestDoor {
    fn admit(&self, peer: Peer, _: mcp::svc::By) -> Result<Box<dyn Pass>, String> {
        judge(&self.assist, peer)?;
        Ok(Box::new(TestPass { assist: self.assist.clone(), peer }))
    }
}
impl Pass for TestPass {
    fn still(&self) -> Result<(), String> {
        judge(&self.assist, self.peer)
    }
    fn subject(&self) -> policy::Subject {
        policy::Subject { program: policy::ProgramId::SYSTEM, pid: self.peer.pid, uid: self.peer.uid, tree: 0 }
    }
}

/// Pids of this test binary's make-believe programs: every test takes its own, so their events tell apart.
fn pids() -> (u32, u32) {
    static NEXT: AtomicU32 = AtomicU32::new(50_000);
    let p = NEXT.fetch_add(2, Ordering::Relaxed);
    (p, p + 1)
}

enum Got {
    Line(String),
    Eof,
    Timeout,
}

/// A program at the local socket.
struct Prog {
    w: UnixStream,
    r: BufReader<UnixStream>,
}

impl Prog {
    fn send(&mut self, text: &str) -> bool {
        self.w.write_all(text.as_bytes()).is_ok()
    }
    fn got(&mut self, ms: u64) -> Got {
        self.r.get_ref().set_read_timeout(Some(Duration::from_millis(ms))).unwrap();
        let mut line = String::new();
        match self.r.read_line(&mut line) {
            Ok(0) => Got::Eof,
            Ok(_) => Got::Line(line.trim_end().to_string()),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Got::Timeout,
            Err(_) => Got::Eof,
        }
    }
    fn line(&mut self) -> Json {
        match self.got(10_000) {
            Got::Line(l) => Json::parse(&l).unwrap_or_else(|_| panic!("no JSON: {l}")),
            Got::Eof => panic!("the door closed the connection"),
            Got::Timeout => panic!("no answer within 10 s"),
        }
    }
    fn ask(&mut self, line: &str) -> Json {
        assert!(self.send(line) && self.send("\n"), "write");
        self.line()
    }
    fn call(&mut self, id: u64, name: &str, args: &str) -> Json {
        self.ask(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#))
    }
    fn names(&mut self) -> Vec<String> {
        let a = self.ask(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#);
        let tools = a.get("result").and_then(|r| r.get("tools")).and_then(Json::as_array).unwrap_or_else(|| panic!("no tools: {}", a.to_string())).to_vec();
        tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str).map(String::from)).collect()
    }
    fn ended(&mut self, ms: u64) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            match self.got(50) {
                Got::Eof => return true,
                Got::Line(_) | Got::Timeout => {}
            }
        }
        false
    }
}

fn text_of(a: &Json) -> String {
    let c = a.get("result").and_then(|r| r.get("content")).and_then(Json::as_array).unwrap_or_else(|| panic!("no content: {}", a.to_string())).to_vec();
    c.iter().filter_map(|i| i.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n")
}
fn failed(a: &Json) -> bool {
    a.get("result").and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true)
}
fn error_of(a: &Json) -> (i64, String) {
    let e = a.get("error").unwrap_or_else(|| panic!("no error: {}", a.to_string()));
    (e.get("code").and_then(Json::as_f64).unwrap_or(0.0) as i64, e.get("message").and_then(Json::as_str).unwrap_or("").to_string())
}

/// The network server with a services registry, and the local door beside it.
struct Both {
    port: u16,
    srv: ServerHandle,
    reg: Arc<Registry>,
    svc_path: PathBuf,
    svc_lis: Arc<StdListen>,
    local: Arc<Local>,
    path: PathBuf,
    lis: Arc<StdListen>,
    assist: Arc<Mutex<HashSet<u32>>>,
    /// An admitted pid and one that is not.
    me: u32,
    stranger: u32,
}

impl Both {
    fn start() -> Both {
        init();
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let svc_path = dir.join(format!("mcl-svc-{}-{n}.sock", std::process::id()));
        let path = dir.join(format!("mcl-tools-{}-{n}.sock", std::process::id()));
        let reg = Registry::new(Arc::new(StdEnv), mcp::TOOLS.entries());
        let svc_lis = Arc::new(StdListen::bind(&svc_path).expect("bind"));
        reg.listen(svc_lis.clone()).expect("listen");
        let (me, stranger) = pids();
        let assist = Arc::new(Mutex::new(HashSet::from([me])));
        let local = Local::new(Arc::new(StdEnv), mcp::TOOLS.entries(), Some(reg.clone()), Arc::new(TestDoor { assist: assist.clone() }));
        let lis = Arc::new(StdListen::bind(&path).expect("bind"));
        lis.set_peer_uid(1000);
        local.listen(lis.clone()).expect("listen");
        for _ in 0..50 {
            let port = free_port();
            let mut cfg = ServerConfig::new(TOKEN);
            cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port };
            match mcp::start_with(&STD, Arc::new(StdEnv), cfg, mcp::TOOLS.entries(), Some(reg.clone())) {
                Ok(srv) => return Both { port, srv, reg, svc_path, svc_lis, local, path, lis, assist, me, stranger },
                Err(mcp::StartError::Listen(net::NetError::AddrInUse)) => continue,
                Err(e) => panic!("server start: {e:?}"),
            }
        }
        panic!("no free port");
    }
    /// Connects as the program with `pid`. The listener stamps the pid at accept, so the caller reads the
    /// door's first line (an answer or the refusal) before the next program connects.
    fn raw(&self, pid: u32) -> Prog {
        self.lis.set_peer_pid(pid);
        let s = UnixStream::connect(&self.path).expect("connect");
        Prog { w: s.try_clone().unwrap(), r: BufReader::new(s) }
    }
    /// Connects as an admitted program and makes sure it is in.
    fn prog(&self, pid: u32) -> Prog {
        let mut p = self.raw(pid);
        let a = p.ask(r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#);
        assert_eq!(a.get("result").map(Json::to_string).as_deref(), Some("{}"), "{}", a.to_string());
        p
    }
    fn net_ok(&self) -> bool {
        let t0 = Instant::now();
        let a = ask(self.port, &tool_call(1, "t_echo", r#"{"text":"net"}"#), 5000).expect("the network side answers").0;
        texts(&a) == "net" && t0.elapsed() < Duration::from_secs(2)
    }
    fn net_names(&self) -> Vec<String> {
        let (a, _) = ask(self.port, &rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#), 5000).expect("tools/list");
        let tools = result_of(&a).get("tools").and_then(Json::as_array).expect("tools").to_vec();
        tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str).map(String::from)).collect()
    }
    fn end(self) {
        self.local.stop();
        assert!(self.local.wait_stopped(5_000_000_000), "the door's threads did not end");
        assert!(!self.path.exists(), "the tools socket's file is left");
        stop(self.srv);
        self.reg.stop();
        assert!(self.reg.wait_stopped(5_000_000_000), "registry threads did not end");
        let _ = (&self.svc_lis, &self.svc_path);
    }
}

fn has(events: &[String], parts: &[&str]) -> bool {
    events.iter().any(|e| parts.iter().all(|p| e.contains(p)))
}

const LOCAL_BUILT_IN: [&str; 8] = ["cpus", "l_after", "l_big", "l_deaf", "l_info", "l_slow", "metrics", "version"];

/// A refusal of `pid` at level Error: what the log keeps at the Mini-PC's usual level ("errors only").
fn refused_as_error(pid: u32) -> bool {
    let pid = format!("pid={pid} ");
    klog::tail(100_000).into_iter().any(|r| r.subsystem == "mcp" && r.name == "local.refused" && r.level == klog::Level::Error && r.text.contains(&pid))
}

#[test]
fn assist_is_admitted_a_stranger_is_refused() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    assert!(text_of(&me.call(1, "version", "{}")).starts_with("BitsOS V2 kernel"));
    // A stranger: one refusal line, then the end. Nothing it could send is read.
    let mut s = b.raw(b.stranger);
    let (code, msg) = error_of(&s.line());
    assert_eq!(code, -32001);
    assert!(msg.starts_with("Refused: ") && msg.contains("/bin/assist"), "{msg}");
    assert!(s.ended(2000), "a refused program's connection stays open");
    assert!(has(&events("local.refused"), &[&format!("pid={}", b.stranger)]), "the refusal is not in the log: {:?}", events("local.refused"));
    assert!(refused_as_error(b.stranger), "a stranger's refusal is not an Error event: {:?}", events("local.refused"));
    // The admitted one is not disturbed.
    assert_eq!(text_of(&me.call(2, "l_info", r#"{"text":"a"}"#)), "info a");
    b.end();
}

#[test]
fn a_stranger_that_claims_to_be_assist_is_refused() {
    let b = Both::start();
    let mut s = b.raw(b.stranger);
    // Whatever it says about itself: the door never reads a claim.
    let claim = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"clientInfo\":{{\"name\":\"/bin/assist\"}},\"program\":\"/bin/assist\",\"exe\":\"/bin/assist\",\"pid\":{}}}}}\n{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"l_info\",\"arguments\":{{}}}}}}\n",
        b.me
    );
    let _ = s.send(&claim);
    let (code, msg) = error_of(&s.line());
    assert!(code == -32001 && msg.starts_with("Refused: "), "{code} {msg}");
    assert!(s.ended(2000));
    assert!(!has(&events("local.call"), &[&format!("pid={}", b.stranger)]), "a stranger's call was taken");
    b.end();
}

#[test]
fn a_program_that_is_no_longer_assist_loses_its_connection() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    let mut idle = b.prog(b.me);
    // The pid now belongs to another program (it ended and the pid was given again, or it called execve).
    b.assist.lock().unwrap().remove(&b.me);
    assert!(me.send("{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"l_info\",\"arguments\":{}}}\n"));
    match me.got(5000) {
        Got::Line(l) => assert!(l.contains("Refused: "), "{l}"),
        // The watchdog may have closed it first.
        Got::Eof => {}
        Got::Timeout => panic!("no refusal"),
    }
    assert!(me.ended(3000));
    // One that sends nothing is closed by the watchdog.
    assert!(idle.ended(5000), "the idle connection of a program that is gone stays");
    assert!(until(2000, || b.local.connections() == 0));
    b.end();
}

#[test]
fn the_list_has_only_local_tools() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    let a = me.ask(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tools = a.get("result").and_then(|r| r.get("tools")).and_then(Json::as_array).expect("tools").to_vec();
    let names: Vec<&str> = tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str)).collect();
    assert_eq!(names, LOCAL_BUILT_IN, "the local list");
    // Each with its B5 class.
    let class = |n: &str| tools.iter().find(|t| t.get("name").and_then(Json::as_str) == Some(n)).and_then(|t| t.get("annotations")).and_then(|a| a.get("safety")).and_then(Json::as_str).map(String::from);
    assert_eq!(class("version").as_deref(), Some("read"));
    assert_eq!(class("l_after").as_deref(), Some("change"));
    // The network side lists everything, as before, and says nothing about `local`.
    let net = b.net_names();
    for n in ["t_echo", "read_file", "hw_info", "net_info", "reboot", "output", "stop", "screen", "send_keys", "ktest"] {
        assert!(net.iter().any(|x| x == n), "{n} is gone from the network list");
        assert!(!names.contains(&n), "{n} is in the local list");
    }
    for n in LOCAL_BUILT_IN {
        assert!(net.iter().any(|x| x == n), "{n} is not in the network list");
    }
    b.end();
}

#[test]
fn a_local_tool_answers_and_any_other_is_unknown() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    assert_eq!(text_of(&me.call(1, "l_info", r#"{"text":"ä"}"#)), "info ä");
    // A tool of the network side and a name nobody has get the same answer, but for the name.
    let (code, msg) = error_of(&me.call(2, "read_file", r#"{"path":"/etc/passwd"}"#));
    let (code2, msg2) = error_of(&me.call(3, "no_such_tool", "{}"));
    assert_eq!((code, code2), (-32602, -32602));
    assert!(msg.starts_with("Unknown tool: read_file"), "{msg}");
    assert_eq!(msg.replace("read_file", "X"), msg2.replace("no_such_tool", "X"));
    // The server's own tools are not local either: a call is never left running behind its caller.
    for n in ["output", "stop", "t_echo", "run", "reboot", "hw_info", "net_info"] {
        assert!(error_of(&me.call(4, n, "{}")).1.starts_with("Unknown tool"), "{n}");
    }
    // A bad argument is said as on the network side.
    assert_eq!(error_of(&me.call(5, "l_info", r#"{"text":5}"#)).0, -32602);
    // An argument the tool does not declare is refused here (the network side hands it to the handler): a
    // local tool that passes its arguments on cannot be made to carry more.
    let (code, msg) = error_of(&me.call(6, "l_info", r#"{"text":"a","kind":"events"}"#));
    assert!(code == -32602 && msg.contains("kind is not an argument of l_info"), "{msg}");
    assert_eq!(text_of(&me.call(7, "l_info", r#"{"text":"a","timeout_ms":5000}"#)), "info a");
    let net = ask(b.port, &tool_call(8, "t_echo", r#"{"text":"n","kind":"events"}"#), 5000).expect("an answer").0;
    assert_eq!(texts(&net), "n", "the network side changed its rule for undeclared arguments");
    let ev = events("local.call");
    let pid = format!("pid={}", b.me);
    assert!(has(&ev, &[&pid, "tool=l_info", "outcome=done"]), "{ev:?}");
    assert!(has(&ev, &[&pid, "tool=read_file", "outcome=refused", "not a local tool"]), "{ev:?}");
    assert!(has(&ev, &[&pid, "tool=no_such_tool", "outcome=refused", "no such tool"]), "{ev:?}");
    assert!(has(&ev, &[&pid, "tool=l_info", "outcome=refused", "bad arguments"]), "{ev:?}");
    // The other methods of the network side.
    let i = me.ask(r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{}}"#);
    assert_eq!(i.get("result").and_then(|r| r.get("serverInfo")).and_then(|s| s.get("name")).and_then(Json::as_str), Some("BitsOS"));
    assert_eq!(error_of(&me.ask(r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#)).0, -32601);
    // A notification gets no line: the next answer is the ping's.
    assert!(me.send("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"));
    assert_eq!(me.ask(r#"{"jsonrpc":"2.0","id":11,"method":"ping"}"#).get("id").and_then(Json::as_u64), Some(11));
    b.end();
}

fn svc_tool(name: &str, safety: &str, local: &str) -> String {
    format!(r#"{{"name":"{name}","description":"test {name}","inputSchema":{{"type":"object","properties":{{"n":{{"type":"integer"}}}}}},"safety":"{safety}"{local}}}"#)
}

/// A service at the registry: registers `tools`, answers every call with "svc <arguments>" and, when the
/// kernel named a caller, " caller <its JSON>".
fn service(b: &Both, uid: u32, name: &str, tools: &[String]) -> (UnixStream, Json) {
    b.svc_lis.set_peer_uid(uid);
    let s = UnixStream::connect(&b.svc_path).expect("connect");
    let mut w = s.try_clone().unwrap();
    writeln!(w, r#"{{"jsonrpc":"2.0","id":1,"method":"register","params":{{"service":"{name}","tools":[{}]}}}}"#, tools.join(",")).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut first = String::new();
    r.read_line(&mut first).expect("the registry's answer");
    std::thread::spawn(move || {
        for line in r.lines() {
            let Ok(line) = line else { break };
            let Ok(m) = Json::parse(&line) else { continue };
            if m.get("method").and_then(Json::as_str) != Some("tools/call") {
                continue;
            }
            let id = m.get("id").and_then(Json::as_u64).unwrap();
            let args = m.get("params").and_then(|p| p.get("arguments")).unwrap().to_string();
            // Who the kernel says called (MC-L: only a call through the local tools socket names one).
            let who = m.get("params").and_then(|p| p.get("_meta")).and_then(|x| x.get("caller")).map(|c| format!(" caller {}", c.to_string())).unwrap_or_default();
            let mut t = String::new();
            mcp::json::escape_into(&format!("svc {args}{who}"), &mut t);
            let _ = writeln!(w, r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":{t}}}]}}}}"#);
        }
    });
    (s, Json::parse(first.trim_end()).expect("JSON"))
}

#[test]
fn a_service_tool_is_local_only_when_a_root_service_marks_it() {
    let b = Both::start();
    let (_keep, a) = service(&b, 0, "lsvc", &[svc_tool("s_loc", "read", r#","local":true"#), svc_tool("s_net", "read", ""), svc_tool("s_no", "read", r#","local":false"#)]);
    assert!(a.get("result").is_some(), "{}", a.to_string());
    let mut me = b.prog(b.me);
    let names = me.names();
    assert!(names.iter().any(|n| n == "s_loc") && !names.iter().any(|n| n == "s_net" || n == "s_no"), "{names:?}");
    // The service is told which local program called; a call over the network side names none.
    assert_eq!(text_of(&me.call(1, "s_loc", r#"{"n":4}"#)), format!(r#"svc {{"n":4}} caller {{"pid":{},"uid":1000}}"#, b.me));
    let net = ask(b.port, &tool_call(9, "s_loc", r#"{"n":5}"#), 5000).expect("an answer").0;
    assert_eq!(texts(&net), r#"svc {"n":5}"#);
    assert!(error_of(&me.call(2, "s_net", "{}")).1.starts_with("Unknown tool"));
    // The network side has all three.
    let net = b.net_names();
    assert!(["s_loc", "s_net", "s_no"].iter().all(|n| net.iter().any(|x| x == n)), "{net:?}");
    // A service of another uid may not mark a tool local, and local must be a boolean.
    let (_k2, a) = service(&b, 1000, "usvc", &[svc_tool("u_loc", "read", r#","local":true"#)]);
    assert!(error_of(&a).1.contains("local") && error_of(&a).1.contains("uid 0"), "{}", a.to_string());
    let (_k3, a) = service(&b, 0, "bsvc", &[svc_tool("b_loc", "read", r#","local":"yes""#)]);
    assert!(error_of(&a).1.contains("local must be true or false"), "{}", a.to_string());
    assert!(!me.names().iter().any(|n| n == "u_loc" || n == "b_loc"));
    // The service ends: its tool is unknown at once.
    drop(_keep.shutdown(std::net::Shutdown::Both));
    assert!(until(3000, || !me.names().iter().any(|n| n == "s_loc")), "the tool of an ended service stays in the local list");
    b.end();
}

#[test]
fn garbage_and_half_lines_cost_only_the_connection() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    // Garbage: a parse error each, the connection stays for a client that recovers.
    assert_eq!(error_of(&me.ask("garbage")).0, -32700);
    assert_eq!(error_of(&me.ask("\u{0}\u{1}\u{2}")).0, -32700);
    assert_eq!(error_of(&me.ask("[1,2]")).0, -32600);
    assert_eq!(text_of(&me.call(1, "l_info", "{}")), "info ");
    // A half line and the rest later: answered once it is whole.
    assert!(me.send(r#"{"jsonrpc":"2.0","id":5,"meth"#));
    assert!(matches!(me.got(300), Got::Timeout), "half a line was answered");
    assert_eq!(me.ask(r#"od":"ping"}"#).get("id").and_then(Json::as_u64), Some(5));
    // Too many bad messages in a row: closed.
    for _ in 0..mcp::local::MAX_ERRORS {
        let _ = me.send("nonsense\n");
    }
    assert!(me.ended(5000), "a connection that only sends nonsense stays open");
    assert!(has(&events("local.too_many_errors"), &[&format!("pid={}", b.me)]));
    // A half line, then the program goes: nothing breaks.
    let mut half = b.prog(b.me);
    assert!(half.send(r#"{"jsonrpc":"2.0","id":6,"method":"tools/ca"#));
    drop(half);
    // Both sides still answer.
    assert_eq!(text_of(&b.prog(b.me).call(2, "l_info", r#"{"text":"b"}"#)), "info b");
    assert!(b.net_ok());
    assert!(until(2000, || b.local.connections() == 0), "{} connections left", b.local.connections());
    b.end();
}

#[test]
fn a_line_of_10_mb_is_cut_and_logged() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    let chunk = "y".repeat(64 * 1024);
    let mut sent = 0usize;
    // The door stops reading at its line limit and hangs up: the writer gets an error long before 10 MB.
    while sent < 10 << 20 {
        if !me.send(&chunk) {
            break;
        }
        sent += chunk.len();
    }
    let mut said = false;
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        match me.got(200) {
            Got::Line(l) => said |= l.contains("longer than"),
            Got::Eof => break,
            Got::Timeout => {}
        }
    }
    assert!(said || sent < 10 << 20, "neither an error line nor a closed connection");
    assert!(me.ended(3000), "the connection of a 10 MB line stays open");
    assert!(has(&events("local.line_too_long"), &[&format!("pid={}", b.me)]), "{:?}", events("local.line_too_long"));
    assert_eq!(text_of(&b.prog(b.me).call(1, "l_info", "{}")), "info ");
    assert!(b.net_ok());
    b.end();
}

#[test]
fn a_client_that_never_reads_blocks_nobody() {
    let b = Both::start();
    b.local.set_write_stall_ns(300_000_000);
    let mut hung = b.prog(b.me);
    // Far more answer than a socket buffer takes, and never a read.
    for i in 0..8 {
        if !hung.send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{i},\"method\":\"tools/call\",\"params\":{{\"name\":\"l_big\",\"arguments\":{{\"bytes\":1000000}}}}}}\n")) {
            break;
        }
    }
    // While it hangs: the network side and another local program answer at once.
    assert!(b.net_ok(), "the network side waits for a hung local client");
    let mut other = b.prog(b.me);
    let t0 = Instant::now();
    assert_eq!(text_of(&other.call(1, "l_info", r#"{"text":"c"}"#)), "info c");
    assert!(t0.elapsed() < Duration::from_secs(1), "another local program waited {:?}", t0.elapsed());
    // The hung one is closed after the stall time.
    assert!(until(5000, || has(&events("local.write_stuck"), &[&format!("pid={}", b.me)])), "no local.write_stuck");
    assert!(until(3000, || b.local.connections() == 1), "{} connections", b.local.connections());
    assert!(b.net_ok());
    drop(hung);
    b.end();
}

#[test]
fn a_call_ends_at_its_deadline() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    let t0 = Instant::now();
    let a = me.call(1, "l_slow", r#"{"ms":20000,"timeout_ms":200}"#);
    assert!(failed(&a) && text_of(&a).contains("did not answer within 200 ms"), "{}", a.to_string());
    assert!(!text_of(&a).contains("output"), "the answer names tools that are not local: {}", text_of(&a));
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    // The tool was asked to stop and its worker is free again; the connection goes on.
    assert!(until(3000, || SLOW_STOPPED.load(Ordering::Acquire)), "the tool was not asked to stop");
    assert!(until(3000, || b.local.tools_running() == 0));
    assert_eq!(text_of(&me.call(2, "l_slow", r#"{"ms":10}"#)), "slept 10");
    // A service that does not answer: its call ends at the deadline too.
    b.svc_lis.set_peer_uid(0);
    let quiet = UnixStream::connect(&b.svc_path).unwrap();
    let mut w = quiet.try_clone().unwrap();
    writeln!(w, r#"{{"jsonrpc":"2.0","id":1,"method":"register","params":{{"service":"quiet","tools":[{}]}}}}"#, svc_tool("q_hang", "read", r#","local":true"#)).unwrap();
    assert!(until(3000, || me.names().iter().any(|n| n == "q_hang")));
    let t0 = Instant::now();
    let a = me.call(3, "q_hang", r#"{"timeout_ms":300}"#);
    assert!(failed(&a) && text_of(&a).contains("did not answer"), "{}", a.to_string());
    assert!(t0.elapsed() < Duration::from_secs(3));
    // An action after the answer is never run from this door.
    assert_eq!(text_of(&me.call(4, "l_after", "{}")), "asked");
    sleep(700);
    assert!(!AFTER.load(Ordering::Acquire), "an action after a local answer ran");
    // A tool that changes something has a log record of its own kind.
    assert!(has(&events("local.change"), &[&format!("pid={}", b.me), "tool=l_after", "outcome=done"]), "{:?}", events("local.change"));
    assert!(!has(&events("local.change"), &["tool=l_slow"]), "a read call is logged as a change");
    assert!(b.net_ok());
    b.end();
}

#[test]
fn one_process_holds_a_few_connections_at_most() {
    let b = Both::start();
    let mut held: Vec<Prog> = (0..mcp::local::PER_PEER).map(|_| b.prog(b.me)).collect();
    let mut more = b.raw(b.me);
    let (code, msg) = error_of(&more.line());
    assert!(code == -32001 && msg.contains("too many connections"), "{msg}");
    assert!(more.ended(2000));
    // One goes, one more may come.
    drop(held.pop());
    assert!(until(3000, || b.local.connections() == mcp::local::PER_PEER - 1));
    assert_eq!(text_of(&b.prog(b.me).call(1, "l_info", "{}")), "info ");
    b.end();
}

#[test]
fn timeout_ms_is_capped_at_this_door() {
    let b = Both::start();
    b.local.set_call_max_ns(300_000_000);
    let mut me = b.prog(b.me);
    let t0 = Instant::now();
    // A day, as the network side allows: here the call ends at the door's own bound.
    let a = me.call(1, "l_slow", r#"{"ms":20000,"timeout_ms":86400000}"#);
    assert!(failed(&a) && text_of(&a).contains("did not answer within 300 ms"), "{}", a.to_string());
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    assert!(until(3000, || b.local.tools_running() == 0));
    b.end();
}

#[test]
fn a_call_ends_when_its_program_hangs_up() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    // A long call, and the program goes away without waiting for the answer.
    assert!(me.send("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"l_slow\",\"arguments\":{\"ms\":50000,\"timeout_ms\":50000}}}\n"));
    assert!(until(3000, || b.local.tools_running() == 1), "the call did not start");
    drop(me);
    // Its place and its worker are free long before the deadline.
    assert!(until(3000, || b.local.connections() == 0), "{} connections left", b.local.connections());
    assert!(until(3000, || b.local.tools_running() == 0), "the tool still runs");
    assert!(has(&events("local.hung_up"), &[&format!("pid={}", b.me), "tool=l_slow"]), "{:?}", events("local.hung_up"));
    assert_eq!(text_of(&b.prog(b.me).call(2, "l_info", "{}")), "info ");
    b.end();
}

#[test]
fn a_tool_that_does_not_end_is_not_called_again() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    let a = me.call(1, "l_deaf", r#"{"timeout_ms":200}"#);
    assert!(failed(&a) && text_of(&a).contains("did not answer within 200 ms"), "{}", a.to_string());
    // It ignores stop and still runs: further calls are refused and leave no thread behind.
    for i in 0..5 {
        let a = me.call(2 + i, "l_deaf", r#"{"timeout_ms":200}"#);
        assert!(failed(&a) && text_of(&a).contains("still busy with an earlier call"), "{}", a.to_string());
    }
    assert_eq!(b.local.tools_running(), 1, "calls piled up behind a tool that hangs");
    assert!(has(&events("local.tool_stuck"), &["tool=l_deaf", &format!("pid={}", b.me)]));
    // Other tools and the network side answer meanwhile.
    assert_eq!(text_of(&me.call(8, "l_info", r#"{"text":"d"}"#)), "info d");
    assert!(b.net_ok());
    // Once it returned it can be called again.
    DEAF_RELEASE.store(true, Ordering::Release);
    assert!(until(3000, || b.local.tools_running() == 0));
    assert_eq!(text_of(&me.call(9, "l_deaf", "{}")), "let go");
    b.end();
}

#[test]
fn the_journal_arguments_of_metrics_are_not_offered() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    assert_eq!(text_of(&me.call(1, "metrics", r#"{"name":"cpu"}"#)), r#"metrics {"name":"cpu"}"#);
    for arg in ["since", "until"] {
        let (code, msg) = error_of(&me.call(2, "metrics", &format!(r#"{{"name":"proc","{arg}":"2026-10-20 00:00"}}"#)));
        assert!(code == -32602 && msg.contains(&format!("{arg} is not an argument of metrics at the local tools socket")), "{code} {msg}");
    }
    assert!(has(&events("local.call"), &[&format!("pid={}", b.me), "tool=metrics", "outcome=refused"]));
    // The network side keeps them.
    let net = ask(b.port, &tool_call(3, "metrics", r#"{"since":"12:00"}"#), 5000).expect("an answer").0;
    assert_eq!(texts(&net), r#"metrics {"since":"12:00"}"#);
    b.end();
}

#[test]
fn a_program_that_floods_is_paced_not_cut_off() {
    let b = Both::start();
    let mut me = b.prog(b.me);
    let t0 = Instant::now();
    let n = 32;
    let mut all = String::new();
    for i in 0..n {
        all.push_str(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"tools/call\",\"params\":{{\"name\":\"l_info\",\"arguments\":{{}}}}}}\n", 100 + i));
    }
    assert!(me.send(&all));
    for i in 0..n {
        assert_eq!(me.line().get("id").and_then(Json::as_u64), Some(100 + i), "answers in order, none lost");
    }
    // 20 at once (the ping of `prog` was one), the rest at 20 a second.
    assert!(t0.elapsed() >= Duration::from_millis(500), "32 calls took {:?}: not paced", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    b.end();
}

/// A listener whose accept fails every time (out of memory), until it is closed (then: not listening).
struct BadListen {
    calls: AtomicUsize,
    dead: AtomicBool,
}
impl mcp::svc::Listen for BadListen {
    fn accept(&self) -> Result<Box<dyn mcp::svc::Link>, i64> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.dead.load(Ordering::Acquire) { Err(-22) } else { Err(-12) }
    }
    fn close(&self) {
        self.dead.store(true, Ordering::Release);
    }
}

#[test]
fn a_failing_accept_is_retried_slowly_and_a_dead_listener_ends_the_loop() {
    init();
    let door = Arc::new(TestDoor { assist: Arc::new(Mutex::new(HashSet::new())) });
    let local = Local::new(Arc::new(StdEnv), mcp::TOOLS.entries(), None, door);
    let lis = Arc::new(BadListen { calls: AtomicUsize::new(0), dead: AtomicBool::new(false) });
    let said_before = events("local.accept_failed").len();
    local.listen(lis.clone()).expect("listen");
    sleep(1500);
    // 10 ms, doubling to 1 s: a handful of tries, not 150; said once, not every time.
    let calls = lis.calls.load(Ordering::Acquire);
    assert!((2..=12).contains(&calls), "{calls} accepts in 1.5 s");
    assert_eq!(events("local.accept_failed").len() - said_before, 1, "{:?}", events("local.accept_failed"));
    // The listener is gone for good (not by the door's stop): the loop ends instead of spinning.
    lis.dead.store(true, Ordering::Release);
    assert!(until(3000, || !events("local.listener_dead").is_empty()), "no local.listener_dead");
    let after = lis.calls.load(Ordering::Acquire);
    sleep(300);
    assert_eq!(lis.calls.load(Ordering::Acquire), after, "accept is still called on a dead listener");
    local.stop();
    assert!(local.wait_stopped(5_000_000_000));
}
