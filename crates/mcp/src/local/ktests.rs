//! MC-L ktests on real AF_UNIX sockets with a kernel thread as the client (`link::connect_kernel`: SO_PEERCRED
//! pid 0). The doors here are the tests' own on `/run/bitsos/ktest-tools-*.sock`; the checks with real
//! programs (the boot's /bin/assist, a stranger, a copy) are proc's `mc.local.user_*`.

use super::door::{Door, Pass, ProgramDoor};
use super::{Peer, start_at};
use crate::kernel::SEC;
use crate::svc::link::{self, Link};
use crate::{Json, TOOLS};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert};

/// Admits kernel threads only (pid 0): the test itself.
struct KernelDoor;
struct KernelPass;

impl Door for KernelDoor {
    fn admit(&self, peer: Peer, _: crate::svc::By) -> Result<Box<dyn Pass>, String> {
        if peer.pid == 0 { Ok(Box::new(KernelPass)) } else { Err(format!("pid {} is not the test", peer.pid)) }
    }
}
impl Pass for KernelPass {
    fn still(&self) -> Result<(), String> {
        Ok(())
    }
    fn subject(&self) -> policy::Subject {
        policy::Subject::KERNEL
    }
}

/// A client: lines out, lines in.
struct Client {
    l: link::UnixLink,
    buf: Vec<u8>,
}

impl Client {
    fn connect(path: &str) -> Result<Client, String> {
        Ok(Client { l: link::connect_kernel(path).map_err(|e| format!("connect {path}: errno {e}"))?, buf: Vec::new() })
    }
    fn send(&self, line: &str) -> Result<(), String> {
        let mut d = line.as_bytes();
        let end = time::now_ns() + 2 * SEC;
        while !d.is_empty() {
            match self.l.try_write(d) {
                Ok(0) if time::now_ns() < end => sched::sleep_ns(1_000_000),
                Ok(0) => return Err(String::from("socket full")),
                Ok(n) => d = &d[n..],
                Err(e) => return Err(format!("write errno {e}")),
            }
        }
        Ok(())
    }
    /// The next line; None when the door closed the connection.
    fn line(&mut self) -> Option<String> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&self.buf[..i]).into_owned();
                self.buf.drain(..=i);
                return Some(line);
            }
            match self.l.read(&mut chunk) {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }
    fn ask(&mut self, line: &str) -> Result<Json, String> {
        self.send(line)?;
        self.send("\n")?;
        let a = self.line().ok_or("the door closed the connection")?;
        Json::parse(&a).map_err(|_| format!("no JSON: {a}"))
    }
}

fn ended(local: &super::Local) -> TestResult {
    local.stop();
    let stopped = local.wait_stopped(5 * SEC);
    sched::reap();
    kassert!(stopped, "the door's threads did not end");
    Ok(())
}

/// The list has the local tools only, a local tool answers, any other is unknown; the socket's name goes with
/// the door.
fn list_call() -> TestResult {
    const PATH: &str = "/run/bitsos/ktest-tools-a.sock";
    let local = start_at(PATH, TOOLS.entries(), None, Arc::new(KernelDoor)).map_err(|e| format!("start: errno {e}"))?;
    let r = (|| -> TestResult {
        let mut c = Client::connect(PATH)?;
        let a = c.ask(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)?;
        let tools = a.get("result").and_then(|r| r.get("tools")).and_then(Json::as_array).ok_or("no tools")?;
        let names: Vec<&str> = tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str)).collect();
        // Exactly these, whatever is marked in the sources: a tool marked local by mistake turns this red
        // (plan/spec/net.md §11.6 says for each why it tells a program of uid 1000 nothing new).
        kassert!(names == ["cpus", "metrics", "sched", "version"], "local list {names:?}");
        let marked = TOOLS.entries().iter().filter(|t| t.local).count();
        kassert!(marked == names.len(), "{marked} tools are marked local, the list has {}", names.len());
        for n in ["read_file", "write_file", "run", "kill", "reboot", "reload", "screen", "send_keys", "read_phys", "log", "events", "output", "stop", "ktest", "hw_info", "net_info", "snd"] {
            kassert!(!names.contains(&n), "{n} is in the local list");
        }
        let a = c.ask(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"version","arguments":{}}}"#)?;
        let text = a.get("result").and_then(|r| r.get("content")).and_then(Json::as_array).and_then(|c| c.first()).and_then(|i| i.get("text")).and_then(Json::as_str).unwrap_or("");
        kassert!(text.starts_with("BitsOS V2 kernel"), "version answered {}", a.to_string());
        for n in ["read_file", "run", "reboot", "output", "stop"] {
            let a = c.ask(&format!(r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"{n}","arguments":{{}}}}}}"#))?;
            let msg = a.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).unwrap_or("");
            kassert!(msg.starts_with("Unknown tool"), "{n} answered {}", a.to_string());
        }
        // metrics: the journal (since/until) stays behind the network side. The real tool still has both
        // arguments (else `serve::WITHHELD` names nothing), and the door refuses each.
        let props = tools.iter().find(|t| t.get("name").and_then(Json::as_str) == Some("metrics")).and_then(|t| t.get("inputSchema")).and_then(|s| s.get("properties")).ok_or("metrics has no schema")?;
        for arg in ["since", "until"] {
            kassert!(props.get(arg).is_some(), "metrics has no argument {arg} any more: local/serve.rs WITHHELD must name the journal's arguments");
            let a = c.ask(&format!(r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"metrics","arguments":{{"{arg}":"00:00"}}}}}}"#))?;
            let msg = a.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).unwrap_or("");
            kassert!(msg.contains("at the local tools socket"), "metrics with {arg} answered {}", a.to_string());
        }
        // Garbage gets a parse error and the connection goes on.
        let a = c.ask("garbage")?;
        kassert!(a.get("error").is_some(), "garbage answered {}", a.to_string());
        kassert!(c.ask(r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#)?.get("result").is_some(), "ping after garbage");
        Ok(())
    })();
    ended(&local)?;
    r?;
    let ns = vfs::root_ns();
    kassert!(vfs::Ctx::kernel(&ns).lookup(None, PATH.as_bytes(), false).is_err(), "the socket's name is left");
    Ok(())
}
ktest::ktest!("mc.local.list_call", Safe, list_call);

/// The real door (the boot's assistant) refuses a kernel thread: one line, then the end.
fn refuses_pid0() -> TestResult {
    const PATH: &str = "/run/bitsos/ktest-tools-b.sock";
    let local = start_at(PATH, TOOLS.entries(), None, Arc::new(ProgramDoor::kernel())).map_err(|e| format!("start: errno {e}"))?;
    let r = (|| -> TestResult {
        let mut c = Client::connect(PATH)?;
        let _ = c.send("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
        let line = c.line().ok_or("no refusal line")?;
        kassert!(line.contains("\"error\"") && line.contains("Refused: "), "refusal {line}");
        kassert!(c.line().is_none(), "the connection of a refused peer stays open");
        kassert!(local.connections() == 0, "a refused peer counts as a connection");
        Ok(())
    })();
    ended(&local)?;
    r
}
ktest::ktest!("mc.local.refuses_pid0", Safe, refuses_pid0);
