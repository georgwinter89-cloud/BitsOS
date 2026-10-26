//! MC-6 ktests: fake services as kernel threads on real AF_UNIX sockets (`link::connect_kernel`), the server and
//! the registry of the test's own (`testclient::with_services`, loopback, ktest token). The same checks with a
//! real user program (`mcp-fakesvc`, the client library) are proc's `mc.svc.user_*`.

use super::link::{self, Link};
use crate::kernel::SEC;
use crate::testclient::{Client, Services, with_services};
use crate::{Json, TOOLS};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ksync::SpinLock;
use ktest::{TestResult, kassert};

/// What a fake service does with a `tools/call`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Answers "echo <name> <arguments>" (and sends one progress line first).
    Echo,
    /// Reads and never answers.
    Hang,
    /// Closes its connection on the first call (a crash as the kernel sees it).
    Die,
}

/// A fake service: what the kernel sent it, one line each.
struct Fake {
    seen: SpinLock<Vec<String>>,
    ended: AtomicBool,
}

impl Fake {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().clone()
    }
    fn saw(&self, part: &str) -> bool {
        self.seen.lock().iter().any(|l| l.contains(part))
    }
}

fn send(l: &dyn Link, line: &str) -> Result<(), String> {
    let mut d = line.as_bytes();
    let end = time::now_ns() + 2 * SEC;
    while !d.is_empty() {
        match l.try_write(d) {
            Ok(0) if time::now_ns() < end => sched::sleep_ns(1_000_000),
            Ok(0) => return Err(String::from("fake: socket full")),
            Ok(n) => d = &d[n..],
            Err(e) => return Err(format!("fake: write errno {e}")),
        }
    }
    Ok(())
}

/// Connects to `path`, sends `first` (register lines), then serves calls by `mode` in a kernel thread until
/// the registry closes the connection.
fn fake(path: &str, first: Vec<String>, mode: Mode) -> Result<Arc<Fake>, String> {
    let l = link::connect_kernel(path).map_err(|e| format!("connect {path}: errno {e}"))?;
    for line in &first {
        send(&l, line)?;
    }
    let f = Arc::new(Fake { seen: SpinLock::named("mc.svc.fake", Vec::new()), ended: AtomicBool::new(false) });
    let f2 = f.clone();
    sched::spawn("mc-svc-fake", sched::Class::Normal, Box::new(move || serve_fake(l, f2, mode))).map_err(|_| "no thread for the fake")?;
    Ok(f)
}

fn serve_fake(l: link::UnixLink, f: Arc<Fake>, mode: Mode) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    'outer: loop {
        let n = match l.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..i]).into_owned();
            buf.drain(..=i);
            f.seen.lock().push(line.clone());
            let Ok(m) = Json::parse(&line) else { continue };
            if m.get("method").and_then(Json::as_str) != Some("tools/call") {
                continue;
            }
            let id = m.get("id").and_then(Json::as_u64).unwrap_or(0);
            match mode {
                Mode::Hang => {}
                Mode::Die => {
                    l.close();
                    break 'outer;
                }
                Mode::Echo => {
                    let p = m.get("params");
                    let name = p.and_then(|p| p.get("name")).and_then(Json::as_str).unwrap_or("");
                    let args = p.and_then(|p| p.get("arguments")).map(Json::to_string).unwrap_or_default();
                    let _ = send(&l, &format!("{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"progressToken\":{id},\"progress\":1,\"message\":\"working\"}}}}\n"));
                    let mut text = format!("echo {name} ");
                    text.push_str(&args);
                    let mut esc = String::new();
                    crate::json::escape_into(&text, &mut esc);
                    let _ = send(&l, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":{esc}}}],\"isError\":false}}}}\n"));
                }
            }
        }
    }
    f.ended.store(true, Ordering::Release);
}

fn tool(name: &str, safety: &str) -> String {
    format!(r#"{{"name":"{name}","description":"test tool {name}","inputSchema":{{"type":"object","properties":{{"n":{{"type":"integer"}}}}}},"safety":"{safety}"}}"#)
}

fn register(id: u64, service: &str, tools: &[String]) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"register\",\"params\":{{\"service\":\"{service}\",\"tools\":[{}]}}}}\n", tools.join(","))
}

/// Waits (bounded) until `cond`.
fn until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) -> TestResult {
    let end = time::now_ns() + secs * SEC;
    while !cond() {
        if time::now_ns() >= end {
            return Err(format!("{what}: not within {secs} s"));
        }
        sched::sleep_ns(10_000_000);
    }
    Ok(())
}

fn listed(c: &mut Client) -> Result<Vec<Json>, String> {
    let body = r#"{"jsonrpc":"2.0","id":99,"method":"tools/list"}"#;
    let to = net::SockAddr { ip: net::Ipv4::LOCALHOST, port: c.port() };
    let sock = net::stack().ok_or("no stack")?;
    let s = sock.tcp_connect(to, Some(SEC)).map_err(|e| format!("connect: {e:?}"))?;
    let r = crate::kernel::post(&*s, crate::kernel::KTEST_TOKEN, body);
    s.close();
    let (_, _, j) = r?;
    Ok(j.get("result").and_then(|r| r.get("tools")).and_then(Json::as_array).ok_or("tools/list: no tools")?.to_vec())
}

fn names(tools: &[Json]) -> Vec<String> {
    tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str).map(String::from)).collect()
}

fn register_call() -> TestResult {
    with_services(25150, TOOLS.entries(), |_| {}, |c, s: &Services| {
        let f = fake(&s.path, alloc::vec![register(1, "ktfake", &[tool("kt_echo", "read"), tool("kt_wipe", "delete_overwrite")])], Mode::Echo)?;
        until("registered", 5, || s.reg.tool_count() == 2)?;
        kassert!(f.saw(r#""registered":["kt_echo","kt_wipe"]"#), "register answer: {:?}", f.seen());
        let list = listed(c)?;
        let n = names(&list);
        let mut sorted = n.clone();
        sorted.sort();
        kassert!(n == sorted, "tools/list not sorted");
        let e = list.iter().find(|t| t.get("name").and_then(Json::as_str) == Some("kt_wipe")).ok_or("kt_wipe not listed")?;
        let ann = e.get("annotations").ok_or("no annotations")?;
        kassert!(ann.get("owner").and_then(Json::as_str) == Some("ktfake") && ann.get("safety").and_then(Json::as_str) == Some("delete_overwrite"), "annotations {}", ann.to_string());
        kassert!(ann.get("destructiveHint").and_then(Json::as_bool) == Some(true) && ann.get("service_pid").is_some(), "annotations {}", ann.to_string());
        let t = c.ok("kt_echo", r#"{"n":7}"#)?;
        kassert!(t.contains("echo kt_echo {\"n\":7}"), "answer {t}");
        // The kernel checks the registered schema before forwarding.
        let r = c.call("kt_echo", r#"{"n":"x"}"#)?;
        kassert!(r.rpc_error == Some(-32602) && f.seen().iter().filter(|l| l.contains("tools/call")).count() == 1, "schema check: {:?} {}", r.rpc_error, r.text);
        kassert!(c.ok("version", "{}")?.starts_with("BitsOS V2"), "version");
        Ok(())
    })
}
ktest::ktest!("mc.svc.register_call", Safe, register_call);

fn schema_errors() -> TestResult {
    with_services(25170, TOOLS.entries(), |_| {}, |_c, s: &Services| {
        let bad = [
            (2, format!(r#"{{"jsonrpc":"2.0","id":2,"method":"register","params":{{"service":"ktbad","tools":[{{"name":"x","inputSchema":{{"type":"string"}},"safety":"read"}}]}}}}"#), "type must be"),
            (3, register(3, "ktbad", &[tool("version", "read")]), "is a kernel tool"),
            (4, format!(r#"{{"jsonrpc":"2.0","id":4,"method":"register","params":{{"service":"ktbad","tools":[{{"name":"y","inputSchema":{{"type":"object"}}}}]}}}}"#), "safety must be"),
            (5, register(5, "Bad Name", &[tool("z", "read")]), "service name"),
            (6, register(6, "ktbad", &[tool("ok_one", "read"), tool("app.x.y", "read")]), "program actions"),
            (7, String::from("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"register\",\"params\":{\"tools\":[]}}"), "needs service"),
            (8, String::from("this is not json"), "Parse error"),
            (9, register(9, "mc", &[tool("q", "read")]), "kernel owner"),
        ];
        let mut lines: Vec<String> = bad.iter().map(|(_, l, _)| { let mut l = l.clone(); if !l.ends_with('\n') { l.push('\n'); } l }).collect();
        lines.push(register(10, "ktbad", &[tool("kt_good", "change")]));
        let f = fake(&s.path, lines, Mode::Echo)?;
        until("answers", 5, || f.seen().len() >= bad.len() + 1)?;
        let seen = f.seen();
        for (id, _, want) in &bad {
            let needle = if *id == 8 { String::from("\"id\":null") } else { format!("\"id\":{id},") };
            let a = seen.iter().find(|l| l.contains(&needle)).ok_or_else(|| format!("no answer to {id}: {seen:?}"))?;
            kassert!(a.contains("\"error\"") && a.contains(want), "answer to {id}: {a}");
        }
        kassert!(seen.iter().any(|l| l.contains("\"id\":10,") && l.contains("kt_good")), "good register: {seen:?}");
        let regd: Vec<String> = s.reg.snapshot().iter().map(|t| t.name.clone()).collect();
        kassert!(regd == alloc::vec![String::from("kt_good")], "registered: {regd:?} (a refused register adds nothing)");
        // A second service may not take the name or the tool.
        let g = fake(&s.path, alloc::vec![register(1, "ktbad", &[tool("kt_other", "read")]), register(2, "ktbad2", &[tool("kt_good", "read")])], Mode::Echo)?;
        until("second answers", 5, || g.seen().len() >= 2)?;
        kassert!(g.seen()[0].contains("in use") && g.seen()[1].contains("registered already"), "clashes: {:?}", g.seen());
        Ok(())
    })
}
ktest::ktest!("mc.svc.schema_errors", Safe, schema_errors);

fn hung() -> TestResult {
    with_services(25190, TOOLS.entries(), |cfg| cfg.conns = 3, |c, s: &Services| {
        let f = fake(&s.path, alloc::vec![register(1, "kthang", &[tool("kt_hang", "read")])], Mode::Hang)?;
        until("registered", 5, || s.reg.tool_count() == 1)?;
        // One call times out at its deadline; the service is told to cancel it.
        let r = c.call("kt_hang", r#"{"timeout_ms":300}"#)?;
        kassert!(r.error && r.text.contains("did not answer") && !r.text.contains("Still running"), "timeout answer: {}", r.text);
        until("cancel seen", 5, || f.saw("notifications/cancelled"))?;
        // mcp_conns (3) calls wait on the hung service at once, each on its own connection ...
        let done = Arc::new(AtomicUsize::new(0));
        for i in 0..3 {
            let mut c2 = c.second();
            let d = done.clone();
            sched::spawn("mc-svc-hangcall", sched::Class::Normal, Box::new(move || {
                let _ = c2.call("kt_hang", r#"{"timeout_ms":3000}"#);
                c2.close();
                d.fetch_add(1, Ordering::AcqRel);
            }))
            .map_err(|_| format!("no thread {i}"))?;
        }
        until("three calls waiting", 5, || f.seen().iter().filter(|l| l.contains("tools/call")).count() >= 4)?;
        // ... and the lifeline still answers on a further connection: its places are apart.
        let mut c3 = c.second();
        let v = c3.ok("version", "{}");
        // A fourth service call is refused at once instead of waiting.
        let busy = c3.call("kt_hang", r#"{"timeout_ms":3000}"#);
        c3.close();
        kassert!(v.is_ok_and(|t| t.starts_with("BitsOS V2")), "version while the service hangs");
        let busy = busy?;
        kassert!(busy.error && busy.text.contains("waiting already"), "fourth call: {}", busy.text);
        until("hung calls ended", 10, || done.load(Ordering::Acquire) == 3)?;
        kassert!(s.reg.tool_count() == 1 && !f.ended.load(Ordering::Acquire), "a hung (not dead) service keeps its tools");
        Ok(())
    })
}
ktest::ktest!("mc.svc.hung", Safe, hung);

fn crash() -> TestResult {
    with_services(25210, TOOLS.entries(), |_| {}, |c, s: &Services| {
        let f = fake(&s.path, alloc::vec![register(1, "ktdie", &[tool("kt_die", "change"), tool("kt_die2", "read")])], Mode::Die)?;
        until("registered", 5, || s.reg.tool_count() == 2)?;
        let r = c.call("kt_die", r#"{"timeout_ms":5000}"#)?;
        kassert!(r.error && r.text.contains("ended before it answered"), "answer: {}", r.text);
        until("tools gone", 5, || s.reg.tool_count() == 0 && s.reg.services().is_empty())?;
        kassert!(!names(&listed(c)?).iter().any(|n| n.starts_with("kt_die")), "tools/list still has the dead service's tools");
        let r = c.call("kt_die2", "{}")?;
        kassert!(r.rpc_error == Some(-32602) && r.text.is_empty(), "a vanished tool is unknown");
        until("fake ended", 5, || f.ended.load(Ordering::Acquire))?;
        Ok(())
    })
}
ktest::ktest!("mc.svc.crash", Safe, crash);

fn hundred() -> TestResult {
    with_services(25230, TOOLS.entries(), |_| {}, |c, s: &Services| {
        let tools: Vec<String> = (0..100).map(|i| tool(&format!("kt_t{i:03}"), "read")).collect();
        let _f = fake(&s.path, alloc::vec![register(1, "kthundred", &tools)], Mode::Echo)?;
        until("registered", 5, || s.reg.tool_count() == 100)?;
        let n = names(&listed(c)?);
        kassert!(n.iter().filter(|n| n.starts_with("kt_t")).count() == 100 && n.iter().any(|n| n == "version"), "listed {}", n.len());
        let t = c.ok("kt_t099", r#"{"n":1}"#)?;
        kassert!(t.contains("echo kt_t099"), "answer {t}");
        Ok(())
    })
}
ktest::ktest!("mc.svc.hundred", Safe, hundred);

fn stop_ends_all() -> TestResult {
    with_services(25250, TOOLS.entries(), |_| {}, |c, s: &Services| {
        let f = fake(&s.path, alloc::vec![register(1, "ktstop", &[tool("kt_wait", "read")])], Mode::Hang)?;
        until("registered", 5, || s.reg.tool_count() == 1)?;
        let mut c2 = c.second();
        let got = Arc::new(SpinLock::named("mc.svc.got", None::<String>));
        let g = got.clone();
        sched::spawn("mc-svc-stopcall", sched::Class::Normal, Box::new(move || {
            let r = c2.call("kt_wait", r#"{"timeout_ms":8000}"#).map(|r| r.text).unwrap_or_else(|e| e);
            c2.close();
            *g.lock() = Some(r);
        }))
        .map_err(|_| "no thread")?;
        until("call waiting", 5, || f.saw("tools/call"))?;
        let t0 = time::now_ns();
        s.reg.stop();
        until("call failed", 5, || got.lock().is_some())?;
        let text = got.lock().clone().unwrap_or_default();
        kassert!(text.contains("ended before it answered"), "answer {text} after {} ms", (time::now_ns() - t0) / 1_000_000);
        kassert!(s.reg.wait_stopped(5 * SEC), "registry threads did not end");
        until("fake ended", 5, || f.ended.load(Ordering::Acquire))?;
        let ns = vfs::root_ns();
        kassert!(vfs::Ctx::kernel(&ns).lookup(None, s.path.as_bytes(), false).is_err(), "socket file left after stop");
        Ok(())
    })
}
ktest::ktest!("mc.svc.stop", Safe, stop_ends_all);
