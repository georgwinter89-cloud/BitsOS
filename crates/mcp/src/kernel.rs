//! The server in the kernel: threads from `sched` (KernelRt for the server, Normal for tool workers, B4),
//! waiting through `ksync::WaitQueue`, time from `time`, sockets from `net::stack()`. `start_kernel` is what the
//! kernel calls once the network stack is installed.

use crate::server::{self, Env, ServerConfig, ServerHandle, StartError, ThreadKind};
use crate::{Json, TOOLS};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use net::{Ipv4, SockAddr, TcpStream};

pub struct KernelEnv;

impl Env for KernelEnv {
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        sched::sleep_ns(ns)
    }
    fn spawn(&self, name: &str, kind: ThreadKind, f: Box<dyn FnOnce() + Send + 'static>) -> Result<(), crate::server::SpawnFailed> {
        let class = match kind {
            ThreadKind::Server => sched::Class::KernelRt,
            ThreadKind::Worker => sched::Class::Normal,
        };
        sched::spawn(name, class, f).map(|_| ()).map_err(|_| crate::server::SpawnFailed)
    }
    fn wait(&self, wq: &ksync::WaitQueue, cond: &mut dyn FnMut() -> bool, timeout_ns: u64) -> bool {
        wq.wait_until_timeout(cond, timeout_ns)
    }
}

/// Starts the kernel's MCP server from CONFIG.TXT (`token=` required, `mcp_port=`, `mcp_conns=`) on the
/// installed socket stack, serving every registered tool. The server runs until the kernel ends; dropping the
/// handle does not stop it. Tool handlers run in worker threads (class Normal) and should respect
/// `ToolContext::deadline_ns`: past it (+ 0.25 s) the client gets "Still running (call n)", the worker stays
/// inside the tool until it returns, and `output`/`stop` reach the call. Call `crate::set_boot_info` before, so
/// `hw_info` and `reboot` can name the running generation.
pub fn start_kernel(config: &dyn Fn(&str) -> Option<String>) -> Result<ServerHandle, StartError> {
    crate::boot::set_profile(config);
    let cfg = ServerConfig::from_config(config).inspect_err(|e| {
        klog::event!(Error, "mcp", "server.start_failed", reason = format!("{e:?}"); "MCP needs token= (8 to 127 characters) in CONFIG.TXT");
    })?;
    let sock = net::stack().ok_or(StartError::NoStack)?;
    // MC-6: user-space services' tools; without the socket the server still serves its own tools.
    let services = crate::svc::start_kernel(TOOLS.entries());
    server::start_with(sock, Arc::new(KernelEnv), cfg, TOOLS.entries(), services)
}

// ---------------------------------------------------------------------------------------------------------
// ktest: initialize, tools/list and tools/call `version` over the kernel's socket stack (G0: the loopback).

const KTEST_PORT: u16 = 25001;
pub(crate) const KTEST_TOKEN: &str = "ktest-token-mc1";
pub(crate) const SEC: u64 = 1_000_000_000;

/// One answer from `s`: (status, head, body), reading until Content-Length bytes of body arrived.
pub(crate) fn answer(s: &dyn TcpStream) -> Result<(u16, String, String), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let end = time::now_ns() + 10 * SEC;
    loop {
        if let Some(h) = crate::http::head_end(&buf, 0) {
            let head = String::from_utf8_lossy(&buf[..h]).into_owned();
            let len = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            if buf.len() >= h + len {
                let status = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
                return Ok((status, head, String::from_utf8_lossy(&buf[h..h + len]).into_owned()));
            }
        }
        let now = time::now_ns();
        if now >= end {
            return Err(format!("no whole answer after 10 s ({} bytes)", buf.len()));
        }
        match s.read(&mut chunk, Some(end - now)) {
            Ok(0) => return Err(String::from("closed before the answer")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(format!("read: {e:?}")),
        }
    }
}

pub(crate) fn post(s: &dyn TcpStream, token: &str, body: &str) -> Result<(u16, String, Json), String> {
    let req = format!("POST /mcp HTTP/1.1\r\nHost: bitsos\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes(), Some(5 * SEC)).map_err(|e| format!("write: {e:?}"))?;
    let (st, head, b) = answer(s)?;
    let j = if b.is_empty() { Json::Null } else { Json::parse(&b).map_err(|_| format!("answer is no JSON: {b}"))? };
    Ok((st, head, j))
}

fn loopback_protocol() -> ktest::TestResult {
    let sock = net::stack().ok_or("no socket stack")?;
    let mut cfg = ServerConfig::new(KTEST_TOKEN);
    cfg.addr = SockAddr { ip: Ipv4::ANY, port: KTEST_PORT };
    let srv = server::start(sock, Arc::new(KernelEnv), cfg, TOOLS.entries()).map_err(|e| format!("start: {e:?}"))?;
    let r = (|| -> ktest::TestResult {
        let to = SockAddr { ip: Ipv4::LOCALHOST, port: KTEST_PORT };
        let c = sock.tcp_connect(to, Some(SEC)).map_err(|e| format!("connect: {e:?}"))?;
        let (st, head, j) = post(&*c, KTEST_TOKEN, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#)?;
        ktest::kassert!(st == 200 && head.contains("Connection: keep-alive"), "initialize: {st} {head}");
        let pv = j.get("result").and_then(|r| r.get("protocolVersion")).and_then(Json::as_str);
        ktest::kassert!(pv == Some(crate::rpc::PROTOCOL), "protocolVersion {pv:?}");
        let (st, _, _) = post(&*c, KTEST_TOKEN, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)?;
        ktest::kassert!(st == 202, "notifications/initialized: {st}");
        let (st, _, j) = post(&*c, KTEST_TOKEN, r#"{"jsonrpc":"2.0","id":"two","method":"tools/list"}"#)?;
        let tools = j.get("result").and_then(|r| r.get("tools")).and_then(Json::as_array).ok_or("tools/list: no tools")?;
        let v = tools.iter().find(|t| t.get("name").and_then(Json::as_str) == Some("version")).ok_or("tools/list: no version tool")?;
        let ann = v.get("annotations").ok_or("version: no annotations")?;
        ktest::kassert!(st == 200 && ann.get("owner").and_then(Json::as_str) == Some("MC") && ann.get("safety").and_then(Json::as_str) == Some("read"), "annotations {}", ann.to_string());
        let (st, _, j) = post(&*c, KTEST_TOKEN, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"version","arguments":{}}}"#)?;
        let res = j.get("result").ok_or("tools/call: no result")?;
        let text = res.get("content").and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str);
        ktest::kassert!(st == 200 && res.get("isError").and_then(Json::as_bool) == Some(false), "tools/call: {}", j.to_string());
        ktest::kassert!(text.is_some_and(|t| t.starts_with("BitsOS V2")), "version text {text:?}");
        c.close();
        let bad = sock.tcp_connect(to, Some(SEC)).map_err(|e| format!("connect: {e:?}"))?;
        let (st, head, _) = post(&*bad, "wrong-token-123", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)?;
        ktest::kassert!(st == 401 && head.contains("Connection: close"), "wrong token: {st}");
        bad.close();
        Ok(())
    })();
    srv.stop();
    let stopped = srv.wait_stopped(5 * SEC);
    sched::reap();
    r?;
    ktest::kassert!(stopped, "server threads did not end");
    Ok(())
}
ktest::ktest!("mcp.loopback_protocol", Safe, loopback_protocol);

// ---------------------------------------------------------------------------------------------------------
// ktest: three connections side by side over the kernel loopback, and the idlest giving way to a fourth.

const KTEST_CONNS_PORT: u16 = 25002;
const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

fn request(token: &str, body: &str) -> String {
    format!("POST /mcp HTTP/1.1\r\nHost: bitsos\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
}

/// The peer closed `s` within 3 s (a read gives 0 bytes).
pub(crate) fn closed_by_server(s: &dyn TcpStream) -> bool {
    let mut b = [0u8; 256];
    let end = time::now_ns() + 3 * SEC;
    loop {
        let now = time::now_ns();
        if now >= end {
            return false;
        }
        match s.read(&mut b, Some(end - now)) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(net::NetError::TimedOut | net::NetError::WouldBlock) => return false,
            Err(_) => return true,
        }
    }
}

fn loopback_conns() -> ktest::TestResult {
    let sock = net::stack().ok_or("no socket stack")?;
    let mut cfg = ServerConfig::new(KTEST_TOKEN);
    cfg.addr = SockAddr { ip: Ipv4::ANY, port: KTEST_CONNS_PORT };
    cfg.conns = 3;
    cfg.first_ns = SEC / 4;
    let srv = server::start(sock, Arc::new(KernelEnv), cfg, TOOLS.entries()).map_err(|e| format!("start: {e:?}"))?;
    let r = (|| -> ktest::TestResult {
        let to = SockAddr { ip: Ipv4::LOCALHOST, port: KTEST_CONNS_PORT };
        let connect = || sock.tcp_connect(to, Some(SEC)).map_err(|e| format!("connect: {e:?}"));
        // c1 has half a request in; c2 and c3 are served beside it and go idle (c2 first).
        let full = request(KTEST_TOKEN, PING);
        let c1 = connect()?;
        c1.write_all(&full.as_bytes()[..20], Some(SEC)).map_err(|e| format!("write: {e:?}"))?;
        let c2 = connect()?;
        let (st, _, _) = post(&*c2, KTEST_TOKEN, PING)?;
        ktest::kassert!(st == 200, "c2 beside a half request: {st}");
        // c2's thread marks it idle only after the answer is out: without a gap c3 may be marked first.
        sched::sleep_ns(SEC / 20);
        let c3 = connect()?;
        let (st, _, _) = post(&*c3, KTEST_TOKEN, PING)?;
        ktest::kassert!(st == 200, "c3: {st}");
        let end = time::now_ns() + 3 * SEC;
        while srv.connections() < 3 && time::now_ns() < end {
            sched::sleep_ns(SEC / 50);
        }
        ktest::kassert!(srv.connections() == 3, "3 connections open, {} seen", srv.connections());
        sched::sleep_ns(SEC / 2);
        // A fourth knocks: c2, idle longest, gives way; the half request and c3 stay.
        let c4 = connect()?;
        let (st, _, _) = post(&*c4, KTEST_TOKEN, PING)?;
        ktest::kassert!(st == 200, "c4 after an eviction: {st}");
        ktest::kassert!(closed_by_server(&*c2), "c2 (idle longest) was not closed");
        c1.write_all(&full.as_bytes()[20..], Some(SEC)).map_err(|e| format!("write: {e:?}"))?;
        let (st, _, _) = answer(&*c1).map_err(|e| format!("c1: {e}"))?;
        ktest::kassert!(st == 200, "c1 finished its request after the eviction: {st}");
        let (st, _, _) = post(&*c3, KTEST_TOKEN, PING)?;
        ktest::kassert!(st == 200, "c3 kept its place: {st}");
        for c in [c1, c2, c3, c4] {
            c.close();
        }
        Ok(())
    })();
    srv.stop();
    let stopped = srv.wait_stopped(5 * SEC);
    sched::reap();
    r?;
    ktest::kassert!(stopped, "server threads did not end");
    Ok(())
}
ktest::ktest!("mcp.loopback_conns", Safe, loopback_conns);
