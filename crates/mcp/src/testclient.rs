//! An MCP client over the kernel's socket stack for ktests (MC's own and other lanes' tools, e.g. proc's `run`):
//! a server of the test's own on a loopback port with the ktest token, and `tools/call` on one keep-alive
//! connection. Never a server on the LAN: a ktest run over MCP on the Mini-PC must not open a second server with
//! the fixed ktest token.

use crate::kernel::{KTEST_TOKEN, KernelEnv, SEC, post};
use crate::server::{self, ServerConfig};
use crate::{Json, ToolEntry};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert};
use net::{Ipv4, KSocket, SockAddr, TcpStream};

pub struct Client {
    sock: &'static dyn KSocket,
    to: SockAddr,
    conn: Option<Box<dyn TcpStream>>,
    id: u64,
}

/// What a tools/call answered: isError, every text item joined, whether the server closed afterwards.
pub struct Reply {
    pub error: bool,
    pub text: String,
    pub closed: bool,
    pub rpc_error: Option<i64>,
    /// base64 data of every image item (MC-3's `screen`).
    pub images: Vec<String>,
}

impl Client {
    pub fn call(&mut self, name: &str, args: &str) -> Result<Reply, String> {
        if self.conn.is_none() {
            self.conn = Some(self.sock.tcp_connect(self.to, Some(SEC)).map_err(|e| format!("connect: {e:?}"))?);
        }
        self.id += 1;
        let body = format!(r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#, self.id);
        let c = self.conn.as_deref().ok_or("no connection")?;
        let (st, head, j) = post(c, KTEST_TOKEN, &body)?;
        let closed = head.contains("Connection: close");
        if closed && let Some(c) = self.conn.take() {
            c.close();
        }
        if st != 200 {
            return Err(format!("{name}: HTTP {st}"));
        }
        let rpc_error = j.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64);
        let r = j.get("result");
        let items = r.and_then(|r| r.get("content")).and_then(Json::as_array).unwrap_or(&[]);
        let text = items.iter().filter_map(|i| i.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n");
        let images = items
            .iter()
            .filter(|i| i.get("type").and_then(Json::as_str) == Some("image"))
            .filter_map(|i| i.get("data").and_then(Json::as_str).map(String::from))
            .collect();
        Ok(Reply { error: r.and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true), text, closed, rpc_error, images })
    }
    /// The text of a call that must succeed.
    pub fn ok(&mut self, name: &str, args: &str) -> Result<String, String> {
        let r = self.call(name, args)?;
        if r.error || r.rpc_error.is_some() {
            return Err(format!("{name} {args}: error {:?} {}", r.rpc_error, r.text));
        }
        Ok(r.text)
    }
    /// The server's port (`with_server` may have moved on from the asked one).
    pub fn port(&self) -> u16 {
        self.to.port
    }
    /// A second client to the same server, on a connection of its own.
    pub fn second(&self) -> Client {
        Client { sock: self.sock, to: self.to, conn: None, id: 1000 }
    }
    pub fn close(&mut self) {
        if let Some(s) = self.conn.take() {
            s.close();
        }
    }
}

/// The call number in a "Still running: ... (call n)" answer.
pub fn call_number(text: &str) -> Result<u64, String> {
    text.split("(call ").nth(1).and_then(|s| s.split(')').next()).and_then(|s| s.parse().ok()).ok_or_else(|| format!("no call number in {text}"))
}

/// How many ports from the asked one `with_server` tries: a server an earlier run left bound (its threads did
/// not end in time) or another test's server must not fail the next Safe run in the same boot.
const PORT_TRIES: u16 = 16;

/// Runs `f` with a client of a server on 127.0.0.1:`port` (or the next free one of `PORT_TRIES`) serving
/// `tools`; stops the server (bounded) after, on every way out of `f`.
pub fn with_server(port: u16, tools: &'static [ToolEntry], f: impl FnOnce(&mut Client) -> TestResult) -> TestResult {
    with_server_cfg(port, tools, None, |_| {}, f)
}

/// MC-6: `with_server` with a registry of user-space services on `/run/bitsos/ktest-<port>.sock` (its path is
/// `Services::path`); the registry is stopped after the server.
pub fn with_services(port: u16, tools: &'static [ToolEntry], tweak: impl Fn(&mut ServerConfig), f: impl FnOnce(&mut Client, &Services) -> TestResult) -> TestResult {
    let path = format!("/run/bitsos/ktest-{port}.sock");
    let reg = crate::svc::Registry::new(Arc::new(KernelEnv), tools);
    let l = crate::svc::link::bind_kernel(&path).map_err(|e| format!("bind {path}: errno {e}"))?;
    reg.listen(Arc::new(l)).map_err(|_| "no thread for the listener")?;
    let svcs = Services { path, reg: reg.clone() };
    let r = with_server_cfg(port, tools, Some(reg.clone()), tweak, |c| f(c, &svcs));
    reg.stop();
    let stopped = reg.wait_stopped(5 * SEC);
    sched::reap();
    r?;
    kassert!(stopped, "registry threads did not end");
    Ok(())
}

/// What `with_services` hands its test.
pub struct Services {
    pub path: String,
    pub reg: Arc<crate::svc::Registry>,
}

fn with_server_cfg(port: u16, tools: &'static [ToolEntry], services: Option<Arc<crate::svc::Registry>>, tweak: impl Fn(&mut ServerConfig), f: impl FnOnce(&mut Client) -> TestResult) -> TestResult {
    let sock = net::stack().ok_or("no socket stack")?;
    let mut tried = Vec::new();
    let (srv, port) = loop {
        let at = port.saturating_add(tried.len() as u16);
        let mut cfg = ServerConfig::new(KTEST_TOKEN);
        cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port: at };
        cfg.after_grace_ns = SEC / 5;
        tweak(&mut cfg);
        match server::start_with(sock, Arc::new(KernelEnv), cfg, tools, services.clone()) {
            Ok(s) => break (s, at),
            Err(server::StartError::Listen(e)) if (tried.len() as u16) + 1 < PORT_TRIES => {
                klog::event!(Info, "mcp", "testclient.port_taken", port = at, error = format!("{e:?}"); "trying the next port");
                tried.push(at);
            }
            Err(e) => return Err(format!("start on port {at} (taken before: {tried:?}): {e:?}")),
        }
    };
    let mut c = Client { sock, to: SockAddr { ip: Ipv4::LOCALHOST, port }, conn: None, id: 0 };
    let r = f(&mut c);
    c.close();
    srv.stop();
    let stopped = srv.wait_stopped(5 * SEC);
    sched::reap();
    r?;
    kassert!(stopped, "server threads did not end");
    Ok(())
}
