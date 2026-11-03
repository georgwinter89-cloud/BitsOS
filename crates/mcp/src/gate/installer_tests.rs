//! MC-9a host tests of the installer's path through the gate: claim (who), next/decide, refusals, the tool run
//! only after a yes, the action after the answer, a lost installer.
extern crate std;
use super::*;
use crate::{Json, ToolOutput};
use mcp_wire::installer::Decision;
use alloc::vec;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

const TOKEN: &str = "gate-installer-token";
static RAN: AtomicUsize = AtomicUsize::new(0);
static AFTER: AtomicUsize = AtomicUsize::new(0);

fn facts(_: &ToolContext, _: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(String::from("facts"))])
}
fn pin(_: &ToolContext, args: &Json) -> ToolResult {
    RAN.fetch_add(1, Ordering::SeqCst);
    Ok(vec![ToolOutput::Text(format!("pinned {}", args.get("n").and_then(Json::as_u64).unwrap_or(0)))])
}
fn reboot(ctx: &ToolContext, _: &Json) -> ToolResult {
    if !ctx.after_answer(alloc::boxed::Box::new(|| {
        AFTER.fetch_add(1, Ordering::SeqCst);
    })) {
        return Err(ToolError::Failed(String::from("no answer to wait for")));
    }
    Ok(vec![ToolOutput::Text(String::from("rebooting once this answer is out"))])
}

static TOOLS: [ToolEntry; 3] = [
    ToolEntry { name: "hw_info", description: "Facts.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Read, handler: facts },
    ToolEntry { name: "system_pin", description: "Pin.", input_schema: r#"{"type":"object"}"#, owner: "RB", safety: Safety::Change, handler: pin },
    ToolEntry { name: "reboot", description: "Reboot.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Change, handler: reboot },
];

fn the_installer() -> Result<u32, String> {
    Ok(4242)
}
fn someone_else() -> Result<u32, String> {
    Err(String::from("pid 77 does not run the boot's /bin/bitsinstall"))
}

fn gate(check: fn() -> Result<u32, String>) -> Gate {
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr.port = 8766;
    Gate::new(cfg, Arc::new(crate::host::StdEnv), &TOOLS, None, Mode::Both).with_check(check)
}

fn authed(g: &Gate) -> AtomicBool {
    let a = AtomicBool::new(false);
    g.command_as(1, &a, &wire::encode_auth("10.0.2.2:4000", TOKEN.as_bytes())).unwrap();
    a
}

fn install(tool: &str, args: &str) -> Vec<u8> {
    wire::encode_install(&Request { tool: tool.into(), args: Json::parse(args).unwrap(), timeout_ms: 5000, peer: "10.0.2.2:4000".into(), client: String::new() })
}

fn decide(id: u64, yes: bool, by: &str) -> Vec<u8> {
    inst::encode_decide(&Decision { id, yes, by: by.into(), reason: String::from("test") })
}

/// The installer's `next` until a request comes (bounded).
fn next(g: &Gate, seat: u64) -> inst::Asked {
    let a = AtomicBool::new(false);
    for _ in 0..40 {
        let t = g.command_as(seat, &a, &inst::encode_next(250)).unwrap();
        if let Some(Some(x)) = inst::decode_next_answer(&t) {
            return x;
        }
    }
    panic!("no request reached the installer");
}

#[test]
fn only_the_installer_claims_and_only_once() {
    let a = AtomicBool::new(false);
    // The real check without the process layer's hook: nobody is the installer.
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr.port = 8766;
    let real = Gate::new(cfg, Arc::new(crate::host::StdEnv), &TOOLS, None, Mode::Both);
    assert_eq!(real.command_as(5, &a, b"claim"), Err(EPERM));
    assert_eq!(gate(someone_else).command_as(5, &a, b"claim"), Err(EPERM));
    let g = gate(the_installer);
    assert_eq!(g.command_as(0, &a, b"claim"), Err(EPERM), "a seatless command never holds the role");
    assert_eq!(super::installer_holder(&g), None, "nobody holds the role yet");
    assert_eq!(g.command_as(5, &a, b"claim").as_deref(), Ok(r#"{"installer":"claimed","pid":4242}"#));
    assert_eq!(super::installer_holder(&g), Some(4242), "/proc/bitsos/installer names the holder");
    assert_eq!(g.command_as(5, &a, b"claim").is_ok(), true, "the holder may claim again");
    assert_eq!(g.command_as(6, &a, b"claim"), Err(EBUSY));
    assert_eq!(g.command_as(6, &a, &inst::encode_next(0)), Err(EPERM), "next without the role");
    assert_eq!(g.command_as(6, &a, &decide(1, true, "dialog")), Err(EPERM), "decide without the role");
    assert_eq!(g.command_as(5, &a, &inst::encode_next(0)).as_deref(), Ok("none\n"));
    assert_eq!(g.command_as(5, &a, &decide(99, true, "dialog")), Err(ENOENT));
    g.seat_closed(5);
    assert_eq!(super::installer_holder(&g), None, "the holder is gone with its session");
    assert_eq!(g.command_as(6, &a, b"claim").is_ok(), true, "the role is free once its session closed");
}

#[test]
fn installer_checks_its_caller_at_every_command() {
    static OK: AtomicBool = AtomicBool::new(true);
    fn flip() -> Result<u32, String> {
        if OK.load(Ordering::SeqCst) { Ok(1) } else { Err(String::from("not the installer any more")) }
    }
    let g = gate(flip);
    let a = AtomicBool::new(false);
    g.command_as(5, &a, b"claim").unwrap();
    OK.store(false, Ordering::SeqCst);
    assert_eq!(g.command_as(5, &a, &inst::encode_next(0)), Err(EPERM), "a descriptor handed to another program");
}

#[test]
fn no_runs_nothing_yes_runs_the_tool() {
    let g = gate(the_installer);
    let a = authed(&g);
    assert_eq!(g.command_as(1, &a, &install("system_pin", r#"{"n":3}"#)), Err(EPERM), "no installer yet");
    assert_eq!(g.command_as(1, &AtomicBool::new(false), &install("system_pin", "{}")), Err(EACCES), "install before auth");
    let i = AtomicBool::new(false);
    g.command_as(5, &i, b"claim").unwrap();
    assert_eq!(g.command_as(1, &a, &install("hw_info", "{}")), Err(EPERM), "not an installer tool");
    assert_eq!(g.command_as(5, &i, &install("system_pin", "{}")), Err(EACCES), "the installer cannot ask (not authed)");
    let before = RAN.load(Ordering::SeqCst);
    std::thread::scope(|s| {
        let h = s.spawn(|| g.command_as(1, &a, &install("system_pin", r#"{"n":3}"#)));
        let x = next(&g, 5);
        assert_eq!((x.tool.as_str(), x.kind, x.peer.as_str()), ("system_pin", inst::Kind::Pin, "10.0.2.2:4000"));
        assert_eq!(x.args.get("n").and_then(Json::as_u64), Some(3));
        assert_eq!(g.command_as(5, &i, &decide(x.id, false, "dialog")).as_deref(), Ok("ok\n"));
        assert_eq!(g.command_as(5, &i, &decide(x.id, true, "dialog")), Err(ENOENT), "decided once");
        let r = wire::decode_result(&h.join().unwrap().unwrap()).unwrap();
        assert!(matches!(&r, Ok(Err(ToolError::Refused(t))) if t.contains("said no")), "{r:?}");
    });
    assert_eq!(RAN.load(Ordering::SeqCst), before, "a no runs nothing");
    std::thread::scope(|s| {
        let h = s.spawn(|| g.command_as(1, &a, &install("system_pin", r#"{"n":4}"#)));
        let x = next(&g, 5);
        g.command_as(5, &i, &decide(x.id, true, "grant")).unwrap();
        let r = wire::decode_result(&h.join().unwrap().unwrap()).unwrap();
        assert!(matches!(&r, Ok(Ok(v)) if matches!(&v[..], [ToolOutput::Text(t)] if t == "pinned 4")), "{r:?}");
    });
    assert_eq!(RAN.load(Ordering::SeqCst), before + 1);
    assert_eq!(g.inst.waiting(), 0);
}

#[test]
fn a_decision_needs_the_request_handed_out() {
    let g = gate(the_installer);
    let a = authed(&g);
    let i = AtomicBool::new(false);
    g.command_as(5, &i, b"claim").unwrap();
    std::thread::scope(|s| {
        let h = s.spawn(|| g.command_as(1, &a, &install("system_pin", "{}")));
        while g.inst.waiting() == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The id the kernel will hand out next, decided blind: refused.
        assert_eq!(g.command_as(5, &i, &decide(1, true, "dialog")), Err(ENOENT));
        let x = next(&g, 5);
        g.command_as(5, &i, &decide(x.id, false, "timeout")).unwrap();
        let r = wire::decode_result(&h.join().unwrap().unwrap()).unwrap();
        assert!(matches!(&r, Ok(Err(ToolError::Refused(t))) if t.contains("in time")), "{r:?}");
    });
}

#[test]
fn a_lost_installer_refuses_what_waits() {
    let g = gate(the_installer);
    let a = authed(&g);
    let i = AtomicBool::new(false);
    g.command_as(5, &i, b"claim").unwrap();
    std::thread::scope(|s| {
        let h = s.spawn(|| g.command_as(1, &a, &install("system_pin", "{}")));
        let _ = next(&g, 5);
        g.seat_closed(5);
        let r = wire::decode_result(&h.join().unwrap().unwrap()).unwrap();
        assert!(matches!(&r, Ok(Err(ToolError::Refused(t))) if t.contains("installer ended")), "{r:?}");
    });
    assert_eq!(g.command_as(1, &a, &install("system_pin", "{}")), Err(EPERM), "no installer any more");
}

#[test]
fn the_action_after_the_answer_waits_for_it() {
    let g = gate(the_installer);
    let a = authed(&g);
    let i = AtomicBool::new(false);
    g.command_as(5, &i, b"claim").unwrap();
    let before = AFTER.load(Ordering::SeqCst);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        let h = s.spawn(|| g.command_as(1, &a, &install("reboot", "{}")));
        let x = next(&g, 5);
        g.command_as(5, &i, &decide(x.id, true, "dialog")).unwrap();
        let r = wire::decode_result(&h.join().unwrap().unwrap()).unwrap();
        assert!(format!("{r:?}").contains("rebooting once"), "{r:?}");
    });
    assert_eq!(AFTER.load(Ordering::SeqCst), before, "not before the answer had time to travel");
    while AFTER.load(Ordering::SeqCst) == before && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(AFTER.load(Ordering::SeqCst), before + 1, "the action ran once");
    assert!(t0.elapsed() >= Duration::from_millis(900), "after the grace: {:?}", t0.elapsed());
}

fn req(tool: &str) -> Request {
    Request { tool: tool.into(), args: Json::parse("{}").unwrap(), timeout_ms: 5000, peer: "10.0.2.2:4000".into(), client: String::new() }
}

#[test]
fn a_decision_after_the_kernel_gave_up_is_told_so() {
    let b = installer::Broker::new();
    b.claim(5).unwrap();
    let p = b.ask(req("system_pin")).unwrap();
    // Handed out, then the wait ends (timeout 0) before the installer answers.
    let asked = b.next(&crate::host::StdEnv, 0, &|| false).unwrap();
    let d = b.wait(&crate::host::StdEnv, &p, 0, &|| false);
    assert!(!d.yes && d.by == "kernel" && d.reason.contains("no decision"), "{d:?}");
    // The kernel's no sits in the slot: a late yes is refused, not counted.
    assert_eq!(b.decide(Decision { id: asked.id, yes: true, by: "dialog".into(), reason: String::new() }).err(), Some(ENOENT));
    // The same when the caller went away.
    let p = b.ask(req("system_pin")).unwrap();
    let d = b.wait(&crate::host::StdEnv, &p, 1_000_000_000, &|| true);
    assert!(!d.yes && d.reason.contains("ended"), "{d:?}");
}

#[test]
fn no_holder_no_queue() {
    let b = installer::Broker::new();
    assert!(b.ask(req("system_pin")).is_none(), "nobody to decide");
    b.claim(5).unwrap();
    assert!(b.release(5));
    assert!(b.ask(req("system_pin")).is_none(), "released: refused at once, not after 300 s");
    assert_eq!(b.waiting(), 0);
}
