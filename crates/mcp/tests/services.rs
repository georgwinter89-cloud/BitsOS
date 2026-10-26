//! MC-6 host tests: user-space services register tools over a Unix socket (std's here, AF_UNIX of NW in the
//! kernel: `mc.svc.*`), calls are forwarded with deadlines, tools vanish with the service, and a hung or
//! crashing service leaves the kernel's own tools and the lifeline's connections alone.

mod common;
use common::*;
use mcp::host::{StdEnv, StdListen};
use mcp::{Json, Registry, ServerConfig, ServerHandle};
use net::host::StdSockets;
use net::{Ipv4, SockAddr};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static STD: StdSockets = StdSockets;

struct Svc {
    port: u16,
    srv: ServerHandle,
    reg: Arc<Registry>,
    path: PathBuf,
    lis: Arc<StdListen>,
}

impl Svc {
    fn start(tweak: impl Fn(&mut ServerConfig)) -> Svc {
        init();
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mcp-svc-{}-{}.sock", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let reg = Registry::new(Arc::new(StdEnv), mcp::TOOLS.entries());
        let lis = Arc::new(StdListen::bind(&path).expect("bind"));
        reg.listen(lis.clone()).expect("listen");
        for _ in 0..50 {
            let port = free_port();
            let mut cfg = ServerConfig::new(TOKEN);
            cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port };
            tweak(&mut cfg);
            match mcp::start_with(&STD, Arc::new(StdEnv), cfg, mcp::TOOLS.entries(), Some(reg.clone())) {
                Ok(srv) => return Svc { port, srv, reg, path, lis },
                Err(mcp::StartError::Listen(net::NetError::AddrInUse)) => continue,
                Err(e) => panic!("server start: {e:?}"),
            }
        }
        panic!("no free port");
    }
    fn call(&self, id: u64, name: &str, args: &str) -> Answer {
        ask(self.port, &tool_call(id, name, args), 15_000).expect("an answer").0
    }
    fn list(&self) -> Vec<Json> {
        let (a, _) = ask(self.port, &rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#), 5000).expect("tools/list");
        result_of(&a).get("tools").and_then(Json::as_array).expect("tools").to_vec()
    }
    fn names(&self) -> Vec<String> {
        self.list().iter().filter_map(|t| t.get("name").and_then(Json::as_str).map(String::from)).collect()
    }
    fn end(self) {
        stop(self.srv);
        self.reg.stop();
        assert!(self.reg.wait_stopped(5_000_000_000), "registry threads did not end");
        assert!(!self.path.exists(), "socket file left");
    }
}

/// A fake service: a connection and what the kernel sent it.
struct Fake {
    w: Mutex<UnixStream>,
    seen: Arc<Mutex<Vec<Json>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Echo,
    Hang,
    Die,
}

impl Fake {
    fn connect(path: &PathBuf, mode: Mode) -> Arc<Fake> {
        let s = UnixStream::connect(path).expect("connect");
        let f = Arc::new(Fake { w: Mutex::new(s.try_clone().unwrap()), seen: Arc::new(Mutex::new(Vec::new())) });
        let f2 = f.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(s).lines() {
                let Ok(line) = line else { break };
                let m = Json::parse(&line).unwrap_or_else(|_| panic!("kernel sent no JSON: {line}"));
                f2.seen.lock().unwrap().push(m.clone());
                if m.get("method").and_then(Json::as_str) != Some("tools/call") {
                    continue;
                }
                let id = m.get("id").and_then(Json::as_u64).unwrap();
                match mode {
                    Mode::Hang => {}
                    Mode::Die => {
                        let _ = f2.w.lock().unwrap().shutdown(std::net::Shutdown::Both);
                        break;
                    }
                    Mode::Echo => {
                        let args = m.get("params").and_then(|p| p.get("arguments")).unwrap().to_string();
                        let mut t = String::new();
                        mcp::json::escape_into(&format!("echo {args}"), &mut t);
                        f2.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":{t}}}]}}}}"#));
                    }
                }
            }
        });
        f
    }
    fn send(&self, line: &str) {
        let mut w = self.w.lock().unwrap();
        let _ = w.write_all(line.as_bytes()).and_then(|_| w.write_all(b"\n"));
    }
    /// The kernel's answer to this service's request `id` (waits up to 5 s).
    fn answer(&self, id: u64) -> Json {
        let mut got = None;
        until(5000, || {
            got = self.seen.lock().unwrap().iter().find(|m| m.get("id").and_then(Json::as_u64) == Some(id) && m.get("method").is_none()).cloned();
            got.is_some()
        });
        got.unwrap_or_else(|| panic!("no answer to {id}: {:?}", self.seen.lock().unwrap()))
    }
    fn count(&self, method: &str) -> usize {
        self.seen.lock().unwrap().iter().filter(|m| m.get("method").and_then(Json::as_str) == Some(method)).count()
    }
}

fn tool(name: &str, safety: &str) -> String {
    format!(r#"{{"name":"{name}","description":"test {name}","inputSchema":{{"type":"object","properties":{{"n":{{"type":"integer"}}}}}},"safety":"{safety}"}}"#)
}

fn register(id: u64, service: &str, tools: &[String]) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"register","params":{{"service":"{service}","tools":[{}]}}}}"#, tools.join(","))
}

fn error_text(a: &Json) -> String {
    a.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).unwrap_or_else(|| panic!("no error: {}", a.to_string())).to_string()
}

#[test]
fn register_list_call_unregister() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Echo);
    f.send(&register(1, "hostsvc", &[tool("h_echo", "read"), tool("h_set", "change")]));
    assert_eq!(f.answer(1).get("result").map(Json::to_string).as_deref(), Some(r#"{"registered":["h_echo","h_set"]}"#));
    let list = s.list();
    let names: Vec<&str> = list.iter().filter_map(|t| t.get("name").and_then(Json::as_str)).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "tools/list stays sorted");
    let e = list.iter().find(|t| t.get("name").and_then(Json::as_str) == Some("h_set")).expect("h_set listed");
    let ann = e.get("annotations").unwrap();
    assert_eq!(ann.get("owner").and_then(Json::as_str), Some("hostsvc"));
    assert_eq!(ann.get("safety").and_then(Json::as_str), Some("change"));
    assert_eq!(ann.get("service_pid").and_then(Json::as_u64), Some(std::process::id() as u64));
    assert_eq!(e.get("inputSchema").map(Json::to_string).as_deref(), Some(r#"{"type":"object","properties":{"n":{"type":"integer"}}}"#));
    let a = s.call(2, "h_echo", r#"{"n":5}"#);
    assert!(!is_error(&a) && texts(&a) == r#"echo {"n":5}"#, "{}", a.body);
    // The kernel checks the schema; the service never sees the bad call.
    let a = s.call(3, "h_echo", r#"{"n":"five"}"#);
    assert!(a.body.contains("-32602") && a.body.contains("Wrong type for argument: n"), "{}", a.body);
    assert_eq!(f.count("tools/call"), 1);
    // A ping and an unregister.
    f.send(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#);
    f.send(r#"{"jsonrpc":"2.0","id":4,"method":"unregister","params":{"tools":["h_set"]}}"#);
    assert!(f.answer(4).get("result").is_some());
    assert!(!s.names().contains(&"h_set".to_string()) && s.names().contains(&"h_echo".to_string()));
    f.send(r#"{"jsonrpc":"2.0","id":5,"method":"unregister","params":{"tools":["version"]}}"#);
    assert!(error_text(&f.answer(5)).contains("not a tool of this service"));
    assert_eq!(s.reg.services(), vec![("hostsvc".to_string(), std::process::id(), 1)]);
    s.end();
}

#[test]
fn schema_errors_and_clashes_register_nothing() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Echo);
    let cases: Vec<(String, &str)> = vec![
        (r#"{"jsonrpc":"2.0","id":1,"method":"register","params":{"service":"badsvc","tools":[{"name":"x","inputSchema":{"type":"string"},"safety":"read"}]}}"#.into(), "type must be \"object\""),
        (r#"{"jsonrpc":"2.0","id":2,"method":"register","params":{"service":"badsvc","tools":[{"name":"x","inputSchema":{"type":"object","properties":{"a":1}},"safety":"read"}]}}"#.into(), "property \"a\" must be an object"),
        (register(3, "badsvc", &[tool("ok", "read"), tool("stop", "read")]), "stop is a kernel tool"),
        (register(4, "badsvc", &[tool("ok", "sometimes")]), "safety must be"),
        (register(5, "BadSvc", &[tool("ok", "read")]), "service name"),
        (register(6, "badsvc", &[tool("a b", "read")]), "name \"a b\""),
        (register(7, "badsvc", &[tool("dup", "read"), tool("dup", "read")]), "named twice"),
        (r#"{"jsonrpc":"2.0","id":8,"method":"register","params":{"service":"badsvc","tools":"x"}}"#.into(), "tools must be an array"),
        (r#"{"jsonrpc":"2.0","id":9,"method":"nothing"}"#.into(), "Method not found"),
    ];
    for (line, _) in &cases {
        f.send(line);
    }
    for (i, (_, want)) in cases.iter().enumerate() {
        let e = error_text(&f.answer(i as u64 + 1));
        assert!(e.contains(want), "case {}: {e}", i + 1);
    }
    f.send("{not json");
    until(5000, || f.seen.lock().unwrap().iter().any(|m| m.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64) == Some(-32700)));
    assert_eq!(s.reg.tool_count(), 0, "a refused register adds nothing");
    // The connection survives bad messages.
    f.send(&register(10, "badsvc", &[tool("b_ok", "read")]));
    assert!(f.answer(10).get("result").is_some());
    // Another service may take neither the name nor the tool, and this one keeps its name.
    let g = Fake::connect(&s.path, Mode::Echo);
    g.send(&register(1, "badsvc", &[tool("g_x", "read")]));
    assert!(error_text(&g.answer(1)).contains("in use"));
    g.send(&register(2, "othersvc", &[tool("b_ok", "read")]));
    assert!(error_text(&g.answer(2)).contains("registered already by badsvc"));
    f.send(&register(11, "renamed", &[tool("b_two", "read")]));
    assert!(error_text(&f.answer(11)).contains("registered as badsvc"));
    s.end();
}

#[test]
fn hung_service_never_blocks_the_lifeline() {
    let s = Svc::start(|c| c.conns = 3);
    let f = Fake::connect(&s.path, Mode::Hang);
    f.send(&register(1, "hangsvc", &[tool("h_hang", "read")]));
    f.answer(1);
    // A call ends at its deadline with an error (no "Still running") and the service is told to cancel.
    let t0 = Instant::now();
    let a = s.call(2, "h_hang", r#"{"timeout_ms":300}"#);
    assert!(is_error(&a) && texts(&a).contains("did not answer h_hang within") && t0.elapsed() < Duration::from_secs(3), "{}", a.body);
    until(3000, || f.count("notifications/cancelled") == 1);
    assert_eq!(f.count("notifications/cancelled"), 1);
    // mcp_conns calls wait on the hung service at once ...
    let port = s.port;
    let waiting: Vec<_> = (0..3).map(|i| std::thread::spawn(move || ask(port, &tool_call(10 + i, "h_hang", r#"{"timeout_ms":2500}"#), 10_000).unwrap().0)).collect();
    until(3000, || f.count("tools/call") == 4);
    // ... a fourth is refused at once; the lifeline answers on further connections, also output and stop.
    let t1 = Instant::now();
    let busy = s.call(20, "h_hang", r#"{"timeout_ms":2500}"#);
    assert!(is_error(&busy) && texts(&busy).contains("waiting already") && t1.elapsed() < Duration::from_secs(1), "{}", busy.body);
    let v = s.call(21, "version", "{}");
    assert!(texts(&v).starts_with("BitsOS V2"));
    let o = s.call(22, "output", r#"{"call":999999}"#);
    assert_eq!(o.status, 200);
    for w in waiting {
        let a = w.join().unwrap();
        assert!(is_error(&a) && texts(&a).contains("did not answer"), "{}", a.body);
    }
    assert_eq!(f.count("notifications/cancelled"), 4);
    assert_eq!(s.reg.tool_count(), 1, "a hung (not dead) service keeps its tools");
    s.end();
}

#[test]
fn stop_cancels_a_forwarded_call() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Hang);
    f.send(&register(1, "stopsvc", &[tool("s_wait", "read")]));
    f.answer(1);
    let port = s.port;
    let w = std::thread::spawn(move || ask(port, &tool_call(2, "s_wait", r#"{"timeout_ms":20000}"#), 30_000).unwrap().0);
    until(3000, || f.count("tools/call") == 1);
    let t0 = Instant::now();
    // The forwarded call is call 1 of this fresh server (only worker calls get numbers).
    let st = s.call(3, "stop", r#"{"call":1}"#);
    let a = w.join().unwrap();
    assert!(is_error(&a) && texts(&a).contains("stopped; stopsvc was told to cancel"), "{} / stop said {}", a.body, st.body);
    assert!(t0.elapsed() < Duration::from_secs(5));
    until(2000, || f.count("notifications/cancelled") == 1);
    let c = f.seen.lock().unwrap().iter().find(|m| m.get("method").and_then(Json::as_str) == Some("notifications/cancelled")).cloned().unwrap();
    assert_eq!(c.get("params").and_then(|p| p.get("reason")).and_then(Json::as_str), Some("stop"));
    s.end();
}

#[test]
fn crashing_service_takes_its_tools_along() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Die);
    f.send(&register(1, "diesvc", &[tool("d_one", "change"), tool("d_two", "read")]));
    f.answer(1);
    assert!(s.names().contains(&"d_two".to_string()));
    let a = s.call(2, "d_one", r#"{"timeout_ms":10000}"#);
    assert!(is_error(&a) && texts(&a).contains("ended before it answered"), "{}", a.body);
    until(3000, || s.reg.tool_count() == 0);
    assert!(!s.names().iter().any(|n| n.starts_with("d_")));
    let a = s.call(3, "d_two", "{}");
    assert!(a.body.contains("Unknown tool: d_two"), "{}", a.body);
    assert!(s.reg.services().is_empty());
    // The same name can register again once the old service is gone.
    let g = Fake::connect(&s.path, Mode::Echo);
    g.send(&register(1, "diesvc", &[tool("d_one", "change")]));
    assert!(g.answer(1).get("result").is_some());
    assert!(texts(&s.call(4, "d_one", "{}")).starts_with("echo"));
    s.end();
}

#[test]
fn a_hundred_tools_and_the_list_budget() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Echo);
    let tools: Vec<String> = (0..100).map(|i| tool(&format!("t{i:03}"), "read")).collect();
    f.send(&register(1, "manysvc", &tools));
    f.answer(1);
    let names = s.names();
    assert_eq!(names.iter().filter(|n| n.starts_with('t') && n.len() == 4).count(), 100);
    assert!(names.contains(&"version".to_string()));
    assert!(texts(&s.call(2, "t099", r#"{"n":1}"#)).starts_with("echo"));
    // Big descriptions until the services' share of tools/list is used up: refused, the list still answers.
    let big = "d".repeat(15_000);
    let mut refused = None;
    for i in 0..200u64 {
        f.send(&format!(r#"{{"jsonrpc":"2.0","id":{},"method":"register","params":{{"tools":[{{"name":"big{i}","description":"{big}","inputSchema":{{"type":"object"}},"safety":"read"}}]}}}}"#, 100 + i));
        let a = f.answer(100 + i);
        if a.get("error").is_some() {
            refused = Some(error_text(&a));
            break;
        }
    }
    assert!(refused.as_deref().is_some_and(|e| e.contains("would take")), "{refused:?}");
    let (a, _) = ask(s.port, &rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#), 5000).unwrap();
    assert!(a.status == 200 && a.body.len() <= mcp::rpc::ANSWER_MAX && a.body.contains("\"version\""));
    s.end();
}

#[test]
fn an_endless_line_ends_the_service() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Echo);
    f.send(&register(1, "longsvc", &[tool("l_x", "read")]));
    f.answer(1);
    {
        let mut w = f.w.lock().unwrap();
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..9 {
            if w.write_all(&chunk).is_err() {
                break;
            }
        }
    }
    until(5000, || s.reg.tool_count() == 0);
    assert_eq!(s.reg.tool_count(), 0);
    assert!(events("svc.line_too_long").len() >= 1);
    s.end();
}

#[test]
fn registry_stop_fails_waiting_calls_at_once() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Hang);
    f.send(&register(1, "endsvc", &[tool("e_wait", "read")]));
    f.answer(1);
    let port = s.port;
    let w = std::thread::spawn(move || ask(port, &tool_call(2, "e_wait", r#"{"timeout_ms":20000}"#), 30_000).unwrap().0);
    until(3000, || f.count("tools/call") == 1);
    let t0 = Instant::now();
    s.reg.stop();
    let a = w.join().unwrap();
    assert!(is_error(&a) && texts(&a).contains("ended before it answered") && t0.elapsed() < Duration::from_secs(3), "{}", a.body);
    assert!(s.reg.wait_stopped(3_000_000_000));
    // The server goes on with its own tools.
    assert!(texts(&s.call(3, "version", "{}")).starts_with("BitsOS V2"));
    s.end();
}

/// The client library's test program (user/lib/mcpreg, static musl) against the registry on CachyOS: the same
/// bytes the kernel sees from it. Built by `cargo xtask image`/`ktest` (target/user); skipped without it.
#[test]
fn the_client_library_program() {
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/user/x86_64-unknown-linux-musl/release/mcp-fakesvc");
    if !exe.is_file() {
        eprintln!("SKIP: {} not built (cargo xtask ktest builds it)", exe.display());
        return;
    }
    let s = Svc::start(|_| {});
    let spawn = |args: &[&str]| std::process::Command::new(&exe).args(args).env("BITSOS_MCP_SOCK", &s.path).stdout(std::process::Stdio::piped()).spawn().expect("spawn mcp-fakesvc");
    let mut echo = spawn(&["echo", "libsvc"]);
    assert!(until(5000, || s.reg.tool_count() == 2), "registered");
    let a = s.call(1, "libsvc_echo", r#"{"text":"grüße \"x\"\n"}"#);
    assert_eq!(texts(&a), r#"echo {"text":"grüße \"x\"\n"}"#);
    echo.kill().unwrap();
    echo.wait().unwrap();
    assert!(until(5000, || s.reg.tool_count() == 0), "tools gone with the program");
    let mut crash = spawn(&["crash", "libcrash"]);
    assert!(until(5000, || s.reg.tool_count() == 1));
    let a = s.call(2, "libcrash_crash", r#"{"timeout_ms":5000}"#);
    assert!(is_error(&a) && texts(&a).contains("ended before it answered"), "{}", a.body);
    let st = crash.wait().unwrap();
    assert_eq!(std::os::unix::process::ExitStatusExt::signal(&st), Some(6));
    let bad = std::process::Command::new(&exe).args(["bad", "libbad"]).env("BITSOS_MCP_SOCK", &s.path).output().unwrap();
    assert_eq!(bad.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("type must be"), "{}", String::from_utf8_lossy(&bad.stderr));
    s.end();
}

fn tool_of<'a>(list: &'a [Json], name: &str) -> &'a Json {
    list.iter().find(|t| t.get("name").and_then(Json::as_str) == Some(name)).unwrap_or_else(|| panic!("{name} not listed"))
}

fn hint(t: &Json, key: &str) -> Option<bool> {
    t.get("annotations").and_then(|a| a.get(key)).and_then(Json::as_bool)
}

/// A call in a thread of its own (the test answers for the fake service meanwhile).
fn call_in_thread(port: u16, id: u64, name: &'static str, args: &'static str) -> std::thread::JoinHandle<Answer> {
    std::thread::spawn(move || ask(port, &tool_call(id, name, args), 15_000).expect("an answer").0)
}

/// The id of the n-th (from 1) tools/call the fake saw.
fn nth_call(f: &Fake, n: usize) -> u64 {
    let mut id = None;
    until(5000, || {
        id = f.seen.lock().unwrap().iter().filter(|m| m.get("method").and_then(Json::as_str) == Some("tools/call")).nth(n - 1).and_then(|m| m.get("id").and_then(Json::as_u64));
        id.is_some()
    });
    id.unwrap_or_else(|| panic!("no tools/call #{n}"))
}

#[test]
fn required_arguments_are_checked_before_forwarding() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Echo);
    let t = r#"{"name":"r_need","description":"needs x","inputSchema":{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]},"safety":"read"}"#;
    f.send(&register(1, "reqsvc", &[t.to_string()]));
    assert!(f.answer(1).get("result").is_some());
    let a = s.call(2, "r_need", "{}");
    assert!(a.body.contains("-32602") && a.body.contains("Missing argument: x"), "{}", a.body);
    let a = s.call(3, "r_need", r#"{"x":5}"#);
    assert!(a.body.contains("-32602") && a.body.contains("Wrong type for argument: x"), "{}", a.body);
    assert_eq!(f.count("tools/call"), 0, "the service never saw a bad call");
    assert!(texts(&s.call(4, "r_need", r#"{"x":"y"}"#)).starts_with("echo"));
    s.end();
}

#[test]
fn late_unknown_and_garbage_answers() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Hang);
    f.send(&register(1, "garbsvc", &[tool("g_t", "read")]));
    assert!(f.answer(1).get("result").is_some());
    // A result that is no object, then one without content: bounded errors, the service stays.
    let h = call_in_thread(s.port, 2, "g_t", r#"{"timeout_ms":10000}"#);
    let id = nth_call(&f, 1);
    f.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":5}}"#));
    let a = h.join().unwrap();
    assert!(is_error(&a) && texts(&a).contains("no result object"), "{}", a.body);
    let h = call_in_thread(s.port, 3, "g_t", r#"{"timeout_ms":10000}"#);
    let id = nth_call(&f, 2);
    f.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":"x"}}}}"#));
    let a = h.join().unwrap();
    assert!(is_error(&a) && texts(&a).contains("no content array"), "{}", a.body);
    // An answer after the deadline (the cancel went out) and one to a call never made: dropped quietly.
    let a = s.call(4, "g_t", r#"{"timeout_ms":300}"#);
    assert!(is_error(&a) && texts(&a).contains("did not answer"), "{}", a.body);
    until(5000, || f.count("notifications/cancelled") == 1);
    let id = nth_call(&f, 3);
    f.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"late"}}]}}}}"#));
    f.send(r#"{"jsonrpc":"2.0","id":999999,"result":{"content":[]}}"#);
    f.send(r#"{"jsonrpc":"2.0","id":"q","method":"ping"}"#);
    let mut pong = false;
    until(5000, || {
        pong = f.seen.lock().unwrap().iter().any(|m| m.get("id").and_then(Json::as_str) == Some("q") && m.get("result").is_some());
        pong
    });
    assert!(pong, "the service still talks to the kernel");
    assert_eq!(s.reg.tool_count(), 1, "the service keeps its tool");
    s.end();
}

#[test]
fn a_second_register_is_all_or_nothing() {
    let s = Svc::start(|_| {});
    let f = Fake::connect(&s.path, Mode::Echo);
    let g = Fake::connect(&s.path, Mode::Echo);
    f.send(&register(1, "firstsvc", &[tool("f_one", "read")]));
    assert!(f.answer(1).get("result").is_some());
    g.send(&register(1, "secondsvc", &[tool("g_one", "read")]));
    assert!(g.answer(1).get("result").is_some());
    // The same name from a second live connection: refused, the first keeps it.
    let h = Fake::connect(&s.path, Mode::Echo);
    h.send(&register(1, "firstsvc", &[tool("h_one", "read")]));
    assert!(error_text(&h.answer(1)).contains("in use"));
    // A second register with one clash adds nothing, a clean one adds.
    f.send(&register(2, "firstsvc", &[tool("f_two", "read"), tool("g_one", "read")]));
    assert!(error_text(&f.answer(2)).contains("registered already by secondsvc"));
    let names = s.names();
    assert!(names.contains(&"f_one".to_string()) && !names.contains(&"f_two".to_string()) && !names.contains(&"h_one".to_string()));
    f.send(&format!(r#"{{"jsonrpc":"2.0","id":3,"method":"register","params":{{"tools":[{}]}}}}"#, tool("f_two", "change")));
    assert!(f.answer(3).get("result").is_some());
    assert!(s.names().contains(&"f_two".to_string()));
    assert!(texts(&s.call(4, "f_one", "{}")).starts_with("echo"));
    s.end();
}

#[test]
fn a_connection_that_never_registers_is_closed() {
    let s = Svc::start(|_| {});
    s.reg.set_register_deadline_ns(300_000_000);
    let mut idle = UnixStream::connect(&s.path).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let f = Fake::connect(&s.path, Mode::Echo);
    f.send(&register(1, "keepsvc", &[tool("k_one", "read")]));
    assert!(f.answer(1).get("result").is_some());
    let t0 = Instant::now();
    let mut b = [0u8; 16];
    use std::io::Read;
    assert_eq!(idle.read(&mut b).ok(), Some(0), "the kernel closes it (EOF)");
    assert!(t0.elapsed() < Duration::from_secs(4));
    until(3000, || s.reg.services().len() == 1);
    assert_eq!(s.reg.services(), vec![("keepsvc".to_string(), std::process::id(), 1)], "a registered service stays");
    s.end();
}

#[test]
fn unregistered_connections_are_bounded() {
    let s = Svc::start(|_| {});
    let held: Vec<UnixStream> = (0..8).map(|_| UnixStream::connect(&s.path).unwrap()).collect();
    until(5000, || s.reg.services().len() == 8);
    assert_eq!(s.reg.services().len(), 8);
    let mut extra = UnixStream::connect(&s.path).unwrap();
    extra.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    use std::io::Read;
    let mut b = [0u8; 16];
    assert_eq!(extra.read(&mut b).ok(), Some(0), "the ninth waits nowhere: closed at once");
    assert_eq!(s.reg.services().len(), 8);
    // Once one registers there is room again (2 x registered services, at least 8).
    let mut w = held[0].try_clone().unwrap();
    w.write_all(format!("{}\n", register(1, "roomsvc", &[tool("room_one", "read")])).as_bytes()).unwrap();
    until(5000, || s.reg.tool_count() == 1);
    let _ninth = UnixStream::connect(&s.path).unwrap();
    until(5000, || s.reg.services().len() == 9);
    assert_eq!(s.reg.services().len(), 9);
    drop(held);
    s.end();
}

#[test]
fn uid_rules_until_pm_decides() {
    let s = Svc::start(|_| {});
    let root = Fake::connect(&s.path, Mode::Echo);
    root.send(&register(1, "settingsd", &[tool("settings_get", "read")]));
    assert!(root.answer(1).get("result").is_some(), "uid 0 may take settings names");
    s.lis.set_peer_uid(1000);
    let u = Fake::connect(&s.path, Mode::Echo);
    until(5000, || s.reg.services().len() == 2);
    u.send(&register(1, "settingsx", &[tool("u_read", "read")]));
    assert!(error_text(&u.answer(1)).contains("for services of uid 0"));
    u.send(&register(2, "usersvc", &[tool("settings_set", "read")]));
    assert!(error_text(&u.answer(2)).contains("for services of uid 0"));
    u.send(&register(3, "usersvc", &[tool("u_read", "read"), tool("u_del", "delete_overwrite")]));
    assert!(u.answer(3).get("result").is_some());
    let list = s.list();
    let (r, ur) = (tool_of(&list, "settings_get"), tool_of(&list, "u_read"));
    assert_eq!((hint(r, "readOnlyHint"), hint(r, "destructiveHint"), hint(r, "verified")), (Some(true), Some(false), Some(true)));
    assert_eq!((hint(ur, "readOnlyHint"), hint(ur, "destructiveHint"), hint(ur, "verified")), (Some(false), Some(true), Some(false)), "a claim, not vouched for");
    assert_eq!(ur.get("annotations").and_then(|a| a.get("safety")).and_then(Json::as_str), Some("read"));
    // Services of other uids together get half the budget; uid 0 keeps the rest.
    let big = "d".repeat(15_000);
    let mut refused = None;
    for i in 0..200u64 {
        u.send(&format!(r#"{{"jsonrpc":"2.0","id":{},"method":"register","params":{{"tools":[{{"name":"ubig{i}","description":"{big}","inputSchema":{{"type":"object"}},"safety":"read"}}]}}}}"#, 100 + i));
        let a = u.answer(100 + i);
        if a.get("error").is_some() {
            refused = Some((i, error_text(&a)));
            break;
        }
    }
    let (n, why) = refused.expect("refused at some point");
    assert!(why.contains("other than 0") && (60..72).contains(&n), "{n}: {why}");
    root.send(&format!(r#"{{"jsonrpc":"2.0","id":2,"method":"register","params":{{"tools":[{{"name":"rbig","description":"{big}","inputSchema":{{"type":"object"}},"safety":"read"}}]}}}}"#));
    assert!(root.answer(2).get("result").is_some(), "uid 0 still has room");
    s.end();
}

#[test]
fn a_service_that_does_not_read_fails_the_call_early() {
    let s = Svc::start(|_| {});
    // Registers, then never reads its socket again.
    let mut deaf = UnixStream::connect(&s.path).unwrap();
    deaf.write_all(format!("{}\n", register(1, "deafsvc", &[tool("d_deaf", "read")])).as_bytes()).unwrap();
    until(5000, || s.reg.tool_count() == 1);
    // Call A's request fills the socket's buffer (or gets stuck itself); call B's cannot be written. Both
    // have 60 s deadlines; the write limit (5 s) ends the stuck one, and closing the service ends the other.
    let pad: &'static str = Box::leak("x".repeat(200_000).into_boxed_str());
    let args: &'static str = Box::leak(format!(r#"{{"timeout_ms":60000,"pad":"{pad}"}}"#).into_boxed_str());
    let t0 = Instant::now();
    let a = call_in_thread(s.port, 10, "d_deaf", args);
    std::thread::sleep(Duration::from_millis(500));
    let (b, _) = ask(s.port, &tool_call(11, "d_deaf", args), 30_000).expect("an answer");
    let a = a.join().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(20), "bounded by the write limit, not the 60 s deadline: {:?}", t0.elapsed());
    assert!(is_error(&a) && is_error(&b), "{} / {}", a.body, b.body);
    assert!(texts(&a).contains("does not read its socket") || texts(&b).contains("does not read its socket"), "{} / {}", texts(&a), texts(&b));
    until(5000, || s.reg.tool_count() == 0);
    assert_eq!(s.reg.tool_count(), 0, "a half-written line closes the service");
    drop(deaf);
    s.end();
}
