//! The protocol parts of V1's mcp-test.mjs over HTTP against the host build (V1-only tools such as run_asm
//! replaced by test tools), plus the private-network rule and start errors.
mod common;
use common::*;
use mcp::{Json, ServerConfig, StartError};
use net::host::StdSockets;
use net::{Ipv4, KSocket, NetError, SockAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Instant;

fn result(a: &Answer) -> Json {
    assert_eq!(a.status, 200, "{a:?}");
    a.json().get("result").cloned().unwrap_or_else(|| panic!("no result: {}", a.body))
}
fn call(c: &mut Client, id: u64, name: &str, args: &str) -> Answer {
    c.write(rpc(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#)));
    c.answer(30_000).expect("an answer")
}
fn text(a: &Answer) -> String {
    result(a).get("content").and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string()
}

#[test]
fn lifecycle_and_tools() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    for v in ["2025-06-18", "2025-03-26", "2024-11-05", "1999-01-01", "2099-12-31"] {
        c.write(rpc(&format!(r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{v}","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#)));
        let r = result(&c.answer(5000).unwrap());
        assert_eq!(r.get("protocolVersion").and_then(Json::as_str), Some("2025-06-18"), "{v}");
        assert!(r.get("capabilities").unwrap().get("tools").is_some());
    }
    c.write(rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#));
    let a = c.answer(5000).unwrap();
    assert!(a.status == 202 && a.body.is_empty() && a.header("content-type").is_none() && a.head.ends_with("keep-alive"));
    c.write(req());
    assert_eq!(result(&c.answer(5000).unwrap()).to_string(), "{}");
    c.write(rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#));
    let r = result(&c.answer(5000).unwrap());
    let tools = r.get("tools").and_then(Json::as_array).unwrap().to_vec();
    let names: Vec<&str> = tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str)).collect();
    for n in ["t_echo", "t_png", "t_sleep", "version"] {
        assert!(names.contains(&n), "{names:?}");
    }
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    for t in &tools {
        let s = t.get("inputSchema").unwrap();
        assert_eq!(s.get("type").and_then(Json::as_str), Some("object"));
        let req = s.get("required").and_then(Json::as_array).unwrap_or(&[]).to_vec();
        assert!(req.iter().all(|n| s.get("properties").and_then(|p| p.get(n.as_str().unwrap())).is_some()));
        let ann = t.get("annotations").unwrap();
        assert!(ann.get("owner").and_then(Json::as_str).is_some() && ann.get("safety").and_then(Json::as_str).is_some());
    }
    // Unicode both ways.
    let fancy = "Grüße ÄÖÜ ß ─│┌┐└┘ ═║ 😀 €\n\"\\";
    let mut arg = String::new();
    mcp::json::escape_into(fancy, &mut arg);
    assert_eq!(text(&call(&mut c, 3, "t_echo", &format!(r#"{{"text":{arg}}}"#))), fancy);
    let a = call(&mut c, 4, "version", "{}");
    assert!(text(&a).starts_with("BitsOS V2") && result(&a).get("isError").and_then(Json::as_bool) == Some(false));
    c.close();
    stop(srv);
}

#[test]
fn bad_requests_over_http() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    c.write(rpc(r#"{"jsonrpc":"2.0","id":1,"method":"#));
    let a = c.answer(5000).unwrap();
    let e = a.json();
    assert!(a.status == 400 && e.get("error").unwrap().get("code").and_then(Json::as_i64) == Some(-32700) && e.get("id") == Some(&Json::Null));
    assert!(!a.closes(), "a JSON error keeps the connection (V1)");
    c.write(rpc(&format!("[{PING}]")));
    let a = c.answer(5000).unwrap();
    assert!(a.status == 400 && a.json().get("error").unwrap().get("code").and_then(Json::as_i64) == Some(-32600));
    c.write(rpc(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#));
    let a = c.answer(5000).unwrap();
    assert!(a.status == 200 && a.json().get("error").unwrap().get("code").and_then(Json::as_i64) == Some(-32601) && a.json().get("id").and_then(Json::as_u64) == Some(7));
    for (name, args, why) in [("nope", "{}", "Unknown tool: nope"), ("t_echo", "{}", "Missing argument: text"), ("t_echo", r#"{"text":5}"#, "Wrong type for argument: text")] {
        let a = call(&mut c, 8, name, args);
        let err = a.json().get("error").cloned().unwrap();
        assert!(a.status == 200 && err.get("code").and_then(Json::as_i64) == Some(-32602) && err.get("message").and_then(Json::as_str).unwrap().contains(why), "{name}");
    }
    let a = call(&mut c, 9, "t_echo", r#"{"text":"x","timeout_ms":0}"#);
    assert!(result(&a).get("isError").and_then(Json::as_bool) == Some(true) && text(&a).contains("timeout_ms must be"));
    c.close();
    stop(srv);
}

#[test]
fn keep_alive_and_fresh_connections() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    for i in 0..40u64 {
        let a = if i % 2 == 1 {
            c.write(rpc(&format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"ping"}}"#)));
            c.answer(5000).unwrap()
        } else {
            call(&mut c, i, "t_echo", &format!(r#"{{"text":"n{i}"}}"#))
        };
        assert!(a.mcp_ok() && a.json().get("id").and_then(Json::as_u64) == Some(i));
        if i % 2 == 0 {
            assert_eq!(text(&a), format!("n{i}"));
        }
    }
    assert!(!c.ended());
    c.close();
    for i in 0..10 {
        let body = format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"tools/call","params":{{"name":"t_echo","arguments":{{"text":"f{i}"}}}}}}"#);
        let (a, closed) = ask(p, &Req { body: Some(&body), headers: vec!["Connection: close".into()], ..Default::default() }.text(), 5000).unwrap();
        assert!(a.status == 200 && closed && text(&a) == format!("f{i}"));
    }
    // A ping right after a request that closed its connection (V1: fetch's next connection knocks).
    for i in 0..6 {
        let refused = if i % 2 == 1 { Req { path: "/nope", ..Default::default() }.text() } else { Req { token: Some("wrong-token"), ..Default::default() }.text() };
        ask(p, &refused, 5000).unwrap();
        let t = Instant::now();
        assert!(ask(p, &req(), 5000).unwrap().0.mcp_ok());
        assert!(t.elapsed().as_millis() < 1000);
    }
    stop(srv);
}

#[test]
fn a_ping_while_a_tool_runs() {
    let (p, srv) = serve(|_| {});
    let mut slow = Client::new(p);
    let t0 = Instant::now();
    slow.write(rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t_sleep","arguments":{"ms":1500,"timeout_ms":60000}}}"#));
    sleep(200);
    let (a, _) = ask(p, &req(), 5000).unwrap();
    assert!(a.mcp_ok() && a.at.duration_since(t0).as_millis() < 1400, "answered while the tool runs");
    let a = slow.answer(10_000).unwrap();
    assert!(text(&a).starts_with("slept 1500 ms") && a.at.duration_since(t0).as_millis() >= 1450);
    slow.close();
    stop(srv);
}

fn is_error(a: &Answer) -> bool {
    result(a).get("isError").and_then(Json::as_bool) == Some(true)
}

#[test]
fn tools_run_in_workers_with_a_deadline() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    let t0 = Instant::now();
    let a = call(&mut c, 1, "t_sleep", r#"{"ms":3000,"timeout_ms":500}"#);
    let ms = a.at.duration_since(t0).as_millis();
    assert!(is_error(&a) && text(&a).starts_with("Still running") && (450..2500).contains(&ms), "{ms} ms: {}", a.body);
    assert!(a.mcp_ok() && !c.ended(), "the connection goes on");
    let a = call(&mut c, 2, "t_echo", r#"{"text":"next"}"#);
    assert_eq!(text(&a), "next", "served while the worker is still inside the late tool");
    assert_eq!(srv.tools_running(), 1);
    assert!(until(5000, || srv.tools_running() == 0));
    assert!(until(2000, || events("tool.late").iter().any(|e| e.contains("tool=t_sleep"))));
    assert!(events("tool.still_running").iter().any(|e| e.contains("tool=t_sleep")));
    let workers = srv.workers();
    for i in 0..10 {
        assert_eq!(text(&call(&mut c, 10 + i, "t_echo", r#"{"text":"again"}"#)), "again");
    }
    assert_eq!(srv.workers(), workers, "workers are used again, not one per call");
    // A tool that panics answers isError; the server goes on.
    let a = call(&mut c, 3, "t_panic", "{}");
    assert!(is_error(&a) && text(&a).contains("broke off"), "{}", a.body);
    assert_eq!(text(&call(&mut c, 4, "t_echo", r#"{"text":"after"}"#)), "after");
    // Stop while a tool hangs: the waiting connection answers Still running, the server stops all the same.
    let mut h = Client::new(p);
    h.write(rpc(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"t_sleep","arguments":{"ms":3000,"timeout_ms":60000}}}"#));
    sleep(300);
    let t = Instant::now();
    srv.stop();
    let a = h.answer(3000).expect("an answer before the tool returns");
    assert!(text(&a).starts_with("Still running") && t.elapsed().as_millis() < 1500, "{}", a.body);
    assert!(srv.wait_stopped(3_000_000_000), "stopped although a tool still runs");
    assert_eq!(srv.tools_running(), 1);
    c.close();
    h.close();
}

#[test]
fn pictures_and_the_4_mb_limit() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    let n = 2_500_000;
    let a = call(&mut c, 1, "t_png", &format!(r#"{{"bytes":{n}}}"#));
    let r = result(&a);
    let content = r.get("content").and_then(Json::as_array).unwrap();
    assert_eq!(content[0].get("text").and_then(Json::as_str), Some("picture"));
    assert_eq!(content[1].get("mimeType").and_then(Json::as_str), Some("image/png"));
    assert_eq!(b64_decode(content[1].get("data").and_then(Json::as_str).unwrap()), png_bytes(n), "PNG as base64 image content");
    let a = call(&mut c, 2, "t_png", r#"{"bytes":3500000}"#);
    assert!(result(&a).get("isError").and_then(Json::as_bool) == Some(true) && text(&a).contains("4 MB"), "over 4 MB: an error result");
    assert!(a.mcp_ok() && !c.ended());
    c.close();
    stop(srv);
}

/// A socket stack whose accepted peers all come from `peer` (to test the private-network rule).
struct FakePeer(Ipv4);
struct PeerStream(Box<dyn TcpStream>, Ipv4);
struct PeerListener(Box<dyn TcpListener>, Ipv4);
static STD: StdSockets = StdSockets;
impl TcpStream for PeerStream {
    fn read(&self, b: &mut [u8], t: Option<u64>) -> Result<usize, NetError> {
        self.0.read(b, t)
    }
    fn write_all(&self, b: &[u8], t: Option<u64>) -> Result<(), NetError> {
        self.0.write_all(b, t)
    }
    fn peer(&self) -> SockAddr {
        SockAddr { ip: self.1, port: 40000 }
    }
    fn local(&self) -> SockAddr {
        self.0.local()
    }
    fn close(&self) {
        self.0.close()
    }
    fn readable(&self) -> bool {
        self.0.readable()
    }
}
impl TcpListener for PeerListener {
    fn accept(&self, t: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        self.0.accept(t).map(|s| Box::new(PeerStream(s, self.1)) as Box<dyn TcpStream>)
    }
    fn local(&self) -> SockAddr {
        self.0.local()
    }
}
impl KSocket for FakePeer {
    fn tcp_listen(&self, a: SockAddr, b: usize) -> Result<Box<dyn TcpListener>, NetError> {
        Ok(Box::new(PeerListener(STD.tcp_listen(a, b)?, self.0)))
    }
    fn tcp_connect(&self, a: SockAddr, t: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        STD.tcp_connect(a, t)
    }
    fn udp_bind(&self, a: SockAddr) -> Result<Box<dyn UdpSocket>, NetError> {
        STD.udp_bind(a)
    }
}

fn start_with(sock: &'static dyn KSocket) -> (u16, mcp::ServerHandle) {
    init();
    for _ in 0..50 {
        let port = free_port();
        let mut cfg = ServerConfig::new(TOKEN);
        cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port };
        match mcp::start(sock, Arc::new(mcp::host::StdEnv), cfg, mcp::TOOLS.entries()) {
            Ok(h) => return (port, h),
            Err(StartError::Listen(NetError::AddrInUse)) => continue,
            Err(e) => panic!("start: {e:?}"),
        }
    }
    panic!("no free port");
}

#[test]
fn private_networks_only() {
    static PUBLIC: FakePeer = FakePeer(Ipv4([8, 8, 8, 8]));
    static HOME: FakePeer = FakePeer(Ipv4([192, 168, 100, 20]));
    let (p, srv) = start_with(&PUBLIC);
    let mut c = Client::new(p);
    c.write(req());
    assert!(c.wait_ended(5000) && c.answer(100).is_none(), "a public peer gets no answer");
    assert!(until(2000, || events("conn.refused").iter().any(|e| e.contains("peer=8.8.8.8"))));
    stop(srv);
    let (p, srv) = start_with(&HOME);
    assert!(ask(p, &req(), 5000).unwrap().0.mcp_ok(), "a home network peer is served");
    stop(srv);
}

#[test]
fn start_errors() {
    init();
    let env = || Arc::new(mcp::host::StdEnv);
    for t in ["short", "has a blank", "", &"x".repeat(128)] {
        let mut cfg = ServerConfig::new(t);
        cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port: free_port() };
        assert_eq!(mcp::start(&STD, env(), cfg, mcp::TOOLS.entries()).err(), Some(StartError::BadToken), "{t:?}");
    }
    let (taken, srv) = serve(|_| {});
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port: taken };
    assert_eq!(mcp::start(&STD, env(), cfg, mcp::TOOLS.entries()).err(), Some(StartError::Listen(NetError::AddrInUse)));
    stop(srv);
    // The port from CONFIG.TXT (asked from the OS; asked again if another test took it meanwhile).
    let srv = (0..50)
        .find_map(|_| {
            let cfg_port = free_port();
            let get = |k: &str| match k {
                "token" => Some(TOKEN.to_string()),
                "mcp_port" => Some(cfg_port.to_string()),
                "mcp_conns" => Some("1".to_string()),
                _ => None,
            };
            let cfg = ServerConfig::from_config(&get).unwrap();
            assert_eq!((cfg.addr.port, cfg.token.as_str(), cfg.conns), (cfg_port, TOKEN, 1));
            match mcp::start(&STD, env(), cfg, mcp::TOOLS.entries()) {
                Ok(h) => Some(h),
                Err(StartError::Listen(NetError::AddrInUse)) => None,
                Err(e) => panic!("start: {e:?}"),
            }
        })
        .expect("a free port");
    assert!(events("config.raised").iter().any(|e| e.contains("asked=1") && e.contains("used=3")), "B6: the raise is said");
    stop(srv);
    let cfg = ServerConfig::from_config(&|k| match k {
        "token" => Some(TOKEN.to_string()),
        "mcp_conns" => Some("many".to_string()),
        _ => None,
    })
    .unwrap();
    assert_eq!(cfg.conns, mcp::server::DEFAULT_CONNS);
    assert!(events("config.bad_value").iter().any(|e| e.contains("key=mcp_conns") && e.contains("many")), "B6: a value not applied is said");
    assert_eq!(ServerConfig::from_config(&|_| None).err(), Some(StartError::NoToken));
    assert_eq!(ServerConfig::from_config(&|k| if k == "mcp_port" { Some("0".into()) } else { Some(TOKEN.into()) }).err(), Some(StartError::BadPort));
}
