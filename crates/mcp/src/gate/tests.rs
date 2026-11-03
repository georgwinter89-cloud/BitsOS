//! Host tests of the gate's commands over fake tools (std threads and time through `host::StdEnv`).
extern crate std;
use super::*;
use crate::{Json, ToolOutput};
use alloc::vec;
use std::time::Duration;

const TOKEN: &str = "gate-test-token";

fn facts(_: &ToolContext, args: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(format!("facts {}", args.get("section").and_then(Json::as_str).unwrap_or("all")))])
}
fn slow(ctx: &ToolContext, _: &Json) -> ToolResult {
    for _ in 0..40 {
        if ctx.stop_requested() {
            return Err(ToolError::Failed(String::from("stopped")));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(vec![ToolOutput::Text(String::from("slow done"))])
}
fn raw(_: &ToolContext, _: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(String::from("raw memory"))])
}

static TOOLS: [ToolEntry; 6] = [
    ToolEntry { name: "hw_info", description: "Facts.", input_schema: r#"{"type":"object","properties":{"section":{"type":"string"}}}"#, owner: "MC", safety: Safety::Read, handler: facts },
    ToolEntry { name: "events", description: "Slow.", input_schema: r#"{"type":"object"}"#, owner: "RB", safety: Safety::Read, handler: slow },
    ToolEntry { name: "read_phys", description: "Raw.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Read, handler: raw },
    ToolEntry { name: "reload", description: "Reload.", input_schema: r#"{"type":"object","properties":{"url":{"type":"string"}}}"#, owner: "RB", safety: Safety::Change, handler: raw },
    ToolEntry { name: "output", description: "Output.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Read, handler: crate::server_only },
    ToolEntry { name: "stop", description: "Stop.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Change, handler: crate::server_only },
];

fn gate() -> Gate {
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr.port = 8766;
    Gate::new(cfg, Arc::new(crate::host::StdEnv), &TOOLS, None, Mode::Both)
}

fn call(tool: &str, args: &str, timeout_ms: u64) -> Vec<u8> {
    wire::encode_call(&Request { tool: tool.into(), args: Json::parse(args).unwrap(), timeout_ms, peer: "10.0.2.2:4000".into(), client: String::new() })
}

#[test]
fn hello_and_list_offer_only_the_narrow_set() {
    let g = gate();
    let a = AtomicBool::new(false);
    let h = wire::decode_hello_answer(&g.command(&a, b"hello").unwrap()).unwrap();
    assert_eq!((h.mode, h.port, h.conns), (Mode::Both, 8766, 8));
    let l = wire::decode_list_answer(&g.command(&a, b"list").unwrap()).unwrap();
    let names: Vec<(&str, bool)> = l.iter().map(|t| (t.name.as_str(), t.installer)).collect();
    assert_eq!(names, vec![("events", false), ("hw_info", false), ("output", false), ("reload", true), ("stop", false)]);
    // The schema text goes over byte for byte.
    assert_eq!(l[1].schema, TOOLS[0].input_schema);
}

#[test]
fn auth_call_install_answered() {
    let g = gate();
    let a = AtomicBool::new(false);
    assert_eq!(g.command(&a, &call("hw_info", "{}", 1000)), Err(EACCES), "a call before auth");
    assert_eq!(g.command(&a, &wire::encode_auth("10.0.2.2:4000", b"wrong-token-xx")), Err(EACCES));
    assert_eq!(g.command(&a, b"answered"), Err(EPERM));
    assert_eq!(g.command(&a, &wire::encode_auth("10.0.2.2:4000", TOKEN.as_bytes())).as_deref(), Ok("ok\n"));
    let r = wire::decode_result(&g.command(&a, &call("hw_info", r#"{"section":"cpu"}"#, 1000)).unwrap()).unwrap();
    assert!(matches!(&r, Ok(Ok(v)) if matches!(&v[..], [ToolOutput::Text(t)] if t == "facts cpu")), "{r:?}");
    assert_eq!(g.command(&a, &call("read_phys", "{}", 1000)), Err(EPERM), "a dropped tool");
    assert_eq!(g.command(&a, &call("nonesuch", "{}", 1000)), Err(EPERM));
    assert_eq!(g.command(&a, &call("reload", "{}", 1000)), Err(EPERM), "an installer tool is not a call");
    let inst = wire::encode_install(&Request { tool: "reload".into(), args: Json::Obj(Vec::new()), timeout_ms: 1000, peer: "10.0.2.2:4000".into(), client: String::new() });
    assert_eq!(g.command(&a, &inst), Err(EPERM), "no installer has claimed its role");
    let before = crate::conn::ANSWERED.get();
    assert_eq!(g.command(&a, b"answered").as_deref(), Ok("ok\n"));
    assert!(crate::conn::ANSWERED.get() > before);
    assert_eq!(g.command(&a, b"answered"), Err(EPERM), "one answered per auth");
    assert_eq!(g.command(&a, &call("hw_info", "{}", 1000)), Err(EACCES), "answered ends the authentication");
    for bad in [&b"nonsense"[..], b"call {", b""] {
        assert_eq!(g.command(&a, bad), Err(EINVAL));
    }
}

#[test]
fn still_running_output_and_no_pid_targets() {
    let g = gate();
    let a = AtomicBool::new(false);
    g.command(&a, &wire::encode_auth("10.0.2.2:4000", TOKEN.as_bytes())).unwrap();
    let r = wire::decode_result(&g.command(&a, &call("events", "{}", 100)).unwrap()).unwrap();
    let Err(still) = r else { panic!("{r:?}") };
    assert!(still.starts_with("Still running: events (call "), "{still}");
    let n: u64 = still.split("(call ").nth(1).and_then(|t| t.split(')').next()).and_then(|t| t.parse().ok()).unwrap();
    std::thread::sleep(Duration::from_millis(1300));
    let r = wire::decode_result(&g.command(&a, &call("output", &format!("{{\"call\":{n}}}"), 1000)).unwrap()).unwrap();
    assert!(format!("{r:?}").contains("slow done"), "{r:?}");
    let r = wire::decode_result(&g.command(&a, &call("stop", r#"{"pid":1,"confirm":true}"#, 1000)).unwrap()).unwrap();
    assert!(matches!(&r, Ok(Err(ToolError::Refused(w))) if w.contains("pid")), "{r:?}");
}

#[test]
fn session_frees_its_answer_once_read() {
    let g: &'static Gate = alloc::boxed::Box::leak(alloc::boxed::Box::new(gate()));
    let s = Session { gate: g, seat: 900, answer: ksync::Mutex::named("t", (Vec::new(), 0)), busy: ksync::Mutex::named("t", ()), authed: AtomicBool::new(false) };
    assert_eq!(s.write_all(b"list"), Ok(4));
    let want = g.command(&AtomicBool::new(false), b"list").unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 7];
    loop {
        let n = s.read(&mut buf);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, want.as_bytes());
    // Read to the end: nothing is kept, and a further read still says end.
    assert_eq!(s.answer.lock().0.capacity(), 0);
    assert_eq!(s.read(&mut buf), 0);
    // The next command answers afresh.
    assert_eq!(s.write_all(b"hello"), Ok(5));
    assert!(s.read(&mut buf) > 0);
}
