//! MC-10 host tests: a live full-access grant offers every kernel tool to its client and to no one else, only
//! the installer sets the table, the clock rule ends grants (a fake wall clock), `request_access` and
//! `pair_client` are questions answered by the installer.
extern crate std;
use super::*;
use crate::{Json, ToolOutput};
use alloc::vec;
use mcp_wire::access::{self as acc, Grant};
use mcp_wire::installer::{Decision, Kind};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize};
use std::time::Duration;

const TOKEN: &str = "gate-access-token";
const T: i64 = 1_800_000_000;
static WALL: AtomicI64 = AtomicI64::new(T);
static RAN: AtomicUsize = AtomicUsize::new(0);
/// The tests share the fake clock: one at a time.
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn a() -> String {
    "a1".repeat(32)
}
fn b() -> String {
    "b2".repeat(32)
}

fn fake_wall() -> Option<i64> {
    Some(WALL.load(Ordering::SeqCst))
}

fn facts(_: &ToolContext, _: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(String::from("facts"))])
}
fn raw(_: &ToolContext, _: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(String::from("raw memory"))])
}
fn pin(_: &ToolContext, _: &Json) -> ToolResult {
    RAN.fetch_add(1, Ordering::SeqCst);
    Ok(vec![ToolOutput::Text(String::from("pinned"))])
}

static TOOLS: [ToolEntry; 5] = [
    ToolEntry { name: "hw_info", description: "Facts.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Read, handler: facts },
    ToolEntry { name: "read_phys", description: "Raw.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Read, handler: raw },
    ToolEntry { name: "run", description: "Run.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::Change, handler: raw },
    ToolEntry { name: "write_file", description: "Write.", input_schema: r#"{"type":"object"}"#, owner: "MC", safety: Safety::DeleteOverwrite, handler: raw },
    ToolEntry { name: "system_pin", description: "Pin.", input_schema: r#"{"type":"object"}"#, owner: "RB", safety: Safety::Change, handler: pin },
];

fn the_installer() -> Result<u32, String> {
    Ok(4242)
}

/// A gate whose installer holds seat 9.
fn gate() -> Gate {
    let mut cfg = ServerConfig::new(TOKEN);
    cfg.addr.port = 8765;
    let g = Gate::new(cfg, Arc::new(crate::host::StdEnv), &TOOLS, None, Mode::Service).with_check(the_installer).with_wall(fake_wall);
    g.command_as(9, &AtomicBool::new(false), &inst::encode_claim()).unwrap();
    g
}

fn authed(g: &Gate) -> AtomicBool {
    let x = AtomicBool::new(false);
    g.command_as(1, &x, &wire::encode_auth("10.0.2.2:4000", TOKEN.as_bytes())).unwrap();
    x
}

fn req(tool: &str, args: &str, client: &str) -> Request {
    Request { tool: tool.into(), args: Json::parse(args).unwrap(), timeout_ms: 5000, peer: "10.0.2.2:4000".into(), client: client.into() }
}

fn names(g: &Gate, client: &str) -> Vec<(String, bool)> {
    let l = wire::decode_list_answer(&g.command(&AtomicBool::new(false), &wire::encode_list_for(client)).unwrap()).unwrap();
    l.into_iter().map(|t| (t.name, t.installer)).collect()
}

fn text(answer: &str) -> String {
    match wire::decode_result(answer).unwrap() {
        Ok(Ok(out)) => out.iter().map(|o| if let ToolOutput::Text(t) = o { t.clone() } else { String::new() }).collect(),
        other => format!("{other:?}"),
    }
}

fn grant(g: &Gate, list: &[Grant]) -> String {
    g.command_as(9, &AtomicBool::new(false), &acc::encode_table(list)).unwrap()
}

fn hour(client: &str) -> Grant {
    Grant { client: client.into(), until: T + 3600, set_at: T, nvme: false }
}

#[test]
fn a_grant_opens_every_tool_for_its_client_and_no_one_else() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    WALL.store(T, Ordering::SeqCst);
    let g = gate();
    let au = authed(&g);
    let narrow = vec![("hw_info".to_string(), false), ("system_pin".to_string(), true)];
    assert_eq!(names(&g, &a()), narrow, "no grant: the narrow set");
    assert_eq!(g.command(&au, &wire::encode_call(&req("read_phys", "{}", &a()))).err(), Some(EPERM));
    // MC-11: before the installer's first table the status says "not known yet", not "no grant".
    let early = acc::decode_status(&g.command(&au, &acc::encode_access(&a())).unwrap()).unwrap();
    assert!(!early.granted && !early.loaded && acc::status_text(&early).starts_with(acc::NOT_LOADED), "{early:?}");
    assert!(grant(&g, &[hour(&a())]).starts_with("{\"taken\":1,\"refused\":[]"));
    let all: Vec<(String, bool)> = ["hw_info", "read_phys", "run", "system_pin", "write_file"].iter().map(|n| (n.to_string(), false)).collect();
    assert_eq!(names(&g, &a()), all, "the grant: every tool, all of them calls");
    assert_eq!(names(&g, &b()), narrow, "another client keeps the narrow set");
    assert_eq!(names(&g, ""), narrow, "a plain connection too");
    assert_eq!(text(&g.command(&au, &wire::encode_call(&req("read_phys", "{}", &a()))).unwrap()), "raw memory");
    assert_eq!(text(&g.command(&au, &wire::encode_call(&req("run", "{}", &a()))).unwrap()), "raw memory");
    assert_eq!(g.command(&au, &wire::encode_call(&req("run", "{}", &b()))).err(), Some(EPERM));
    assert_eq!(g.command(&au, &wire::encode_call(&req("run", "{}", ""))).err(), Some(EPERM));
    // An installer tool runs without a question for the granted client (nobody calls `next`).
    let before = RAN.load(Ordering::SeqCst);
    assert_eq!(text(&g.command(&au, &wire::encode_install(&req("system_pin", "{}", &a()))).unwrap()), "pinned");
    assert_eq!(text(&g.command(&au, &wire::encode_call(&req("system_pin", "{}", &a()))).unwrap()), "pinned");
    assert_eq!(RAN.load(Ordering::SeqCst), before + 2);
    // Its status, then its end by the wall clock.
    let st = acc::decode_status(&g.command(&au, &acc::encode_access(&a())).unwrap()).unwrap();
    assert!(st.granted && st.until == T + 3600 && st.left_s > 3590, "{st:?}");
    let other = acc::decode_status(&g.command(&au, &acc::encode_access(&b())).unwrap()).unwrap();
    assert!(!other.granted && other.loaded && acc::status_text(&other).starts_with("no full access"), "{other:?}");
    assert_eq!(g.command(&AtomicBool::new(false), &acc::encode_access(&a())).err(), Some(EACCES), "status needs the token too");
    WALL.store(T + 3600, Ordering::SeqCst);
    assert_eq!(names(&g, &a()), narrow, "expired: narrow again");
    assert_eq!(g.command(&au, &wire::encode_call(&req("read_phys", "{}", &a()))).err(), Some(EPERM));
}

#[test]
fn only_the_installer_sets_grants_and_the_clock_rule_judges_them() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    WALL.store(T, Ordering::SeqCst);
    let g = gate();
    let au = authed(&g);
    // Not the installer's seat: EPERM, nothing taken.
    assert_eq!(g.command_as(1, &au, &acc::encode_table(&[hour(&a())])).err(), Some(EPERM));
    assert_eq!(names(&g, &a()).len(), 2);
    // Too long, set in the future, already over: refused by the rule.
    let bad = [Grant { until: T + 8 * 86_400, ..hour(&a()) }, Grant { client: b(), set_at: T + 600, until: T + 1200, nvme: true }, Grant { client: "c3".repeat(32), until: T - 1, set_at: T - 60, nvme: false }];
    let ans = grant(&g, &bad);
    assert!(ans.starts_with("{\"taken\":0") && ans.contains("too_long") && ans.contains("clock_back") && ans.contains("expired"), "{ans}");
    assert_eq!(names(&g, &a()).len(), 2);
    // A jump of the wall clock by more than the grant's length ends it.
    grant(&g, &[hour(&a())]);
    assert_eq!(names(&g, &a()).len(), 5);
    WALL.store(T + 2 * 3600 + 1, Ordering::SeqCst);
    assert_eq!(names(&g, &a()).len(), 2, "ended by the jump");
    // The same table again after the jump is judged from now (it is past its end).
    assert!(grant(&g, &[hour(&a())]).contains("expired"));
    // An empty table ends every grant (a revoke at the keyboard).
    WALL.store(T, Ordering::SeqCst);
    grant(&g, &[hour(&a())]);
    assert_eq!(names(&g, &a()).len(), 5);
    grant(&g, &[]);
    assert_eq!(names(&g, &a()).len(), 2, "revoked");
}

/// The installer's side of a question in a thread: takes the next request, checks it, sets `table` and
/// decides `yes`.
fn answer_next(g: &'static Gate, table: Vec<Grant>, yes: bool) -> std::thread::JoinHandle<inst::Asked> {
    std::thread::spawn(move || {
        let x = AtomicBool::new(false);
        for _ in 0..40 {
            if let Some(Some(asked)) = inst::decode_next_answer(&g.command_as(9, &x, &inst::encode_next(250)).unwrap()) {
                if yes {
                    g.command_as(9, &x, &acc::encode_table(&table)).unwrap();
                }
                g.command_as(9, &x, &inst::encode_decide(&Decision { id: asked.id, yes, by: "dialog".into(), reason: String::new() })).unwrap();
                return asked;
            }
        }
        panic!("no question reached the installer");
    })
}

#[test]
fn request_access_and_pair_client_are_questions_for_the_installer() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    WALL.store(T, Ordering::SeqCst);
    let g: &'static Gate = alloc::boxed::Box::leak(alloc::boxed::Box::new(gate()));
    let au = authed(g);
    // Only a TLS client may ask; hours are checked; nothing reaches the installer for those.
    let plain = text(&g.command(&au, &wire::encode_install(&req("request_access", "{}", ""))).unwrap());
    assert!(plain.contains("only for a paired client"), "{plain}");
    let bad = wire::decode_result(&g.command(&au, &wire::encode_install(&req("request_access", r#"{"hours":200}"#, &a()))).unwrap()).unwrap();
    assert!(matches!(bad, Ok(Err(ToolError::BadArgument(_)))), "{bad:?}");
    // Yes: the installer sets the table first; the answer names the end.
    let h = answer_next(g, vec![Grant { client: a(), until: T + 8 * 3600, set_at: T, nvme: false }], true);
    let ans = text(&g.command(&au, &wire::encode_install(&req("request_access", "{\"hours\":8,\"reason\":\"die Welle\\nprüfen\"}", &a()))).unwrap());
    let asked = h.join().unwrap();
    assert_eq!((asked.kind, asked.client.as_str()), (Kind::Access, a().as_str()));
    assert_eq!(asked.args.get("hours").and_then(Json::as_i64), Some(8));
    assert_eq!(asked.args.get("reason").and_then(Json::as_str), Some("die Welleprüfen"), "control characters dropped");
    assert!(ans.contains("full access for this client until") && (ans.contains("8 h 0 min left") || ans.contains("7 h 59 min left")), "{ans}");
    assert_eq!(names(g, &a()).len(), 5);
    // No: refused, the table unchanged.
    let h = answer_next(g, vec![], false);
    let no = wire::decode_result(&g.command(&au, &wire::encode_install(&req("request_access", "{}", &b()))).unwrap()).unwrap();
    h.join().unwrap();
    assert!(matches!(&no, Ok(Err(ToolError::Refused(t))) if t.contains("said no")), "{no:?}");
    assert_eq!(names(g, &b()).len(), 2);
    // A yes without a grant in the table is said so.
    let h = answer_next(g, vec![], true);
    let odd = wire::decode_result(&g.command(&au, &wire::encode_install(&req("request_access", "{}", &b()))).unwrap()).unwrap();
    h.join().unwrap();
    assert!(matches!(&odd, Ok(Err(ToolError::Failed(t))) if t.contains("no grant")), "{odd:?}");
    // pair_client: a plain yes or no.
    let h = answer_next(g, vec![Grant { client: a(), until: T + 8 * 3600, set_at: T, nvme: false }], true);
    let pair = text(&g.command(&au, &wire::encode_install(&req("pair_client", r#"{"fingerprint":"x"}"#, ""))).unwrap());
    assert_eq!(h.join().unwrap().kind, Kind::Pair);
    assert!(pair.starts_with("yes"), "{pair}");
    // Without an installer: EPERM, as every installer request.
    let lone = Gate::new(ServerConfig::new(TOKEN), Arc::new(crate::host::StdEnv), &TOOLS, None, Mode::Service).with_wall(fake_wall);
    let la = authed(&lone);
    assert_eq!(lone.command(&la, &wire::encode_install(&req("request_access", "{}", &a()))).err(), Some(EPERM));
    std::thread::sleep(Duration::from_millis(1));
}

static TOLD_EXPIRY: AtomicU64 = AtomicU64::new(u64::MAX);
fn exp_open(until: u64, _: &str) {
    TOLD_EXPIRY.store(until, Ordering::SeqCst);
}
fn exp_close(_: &str) {
    TOLD_EXPIRY.store(0, Ordering::SeqCst);
}

/// A grant with the NVMe tick opens the sink until the grant's end on the monotonic clock; a sweep after the
/// end closes it, and the block layer's own end holds even when no command comes (`runtime_open`).
#[test]
fn the_nvme_tick_ends_with_the_grant_without_a_command() {
    let t = access::Table::with_sink(access::NvmeSink { open: exp_open, close: exp_close });
    let g = Grant { nvme: true, ..hour(&a()) };
    assert_eq!(t.set(vec![g], Some(T), 0).taken, 1);
    let end = 3600 * 1_000_000_000u64;
    assert_eq!(TOLD_EXPIRY.load(Ordering::SeqCst), end, "open until the grant's end");
    t.sweep(Some(T + 3599), end - 1_000_000_000);
    assert_eq!(TOLD_EXPIRY.load(Ordering::SeqCst), end, "still open a second before");
    assert!(block::policy::runtime_open(end, end - 1) && !block::policy::runtime_open(end, end), "the block layer ends it by itself at the end");
    t.sweep(Some(T + 3600), end);
    assert_eq!((TOLD_EXPIRY.load(Ordering::SeqCst), t.nvme_told()), (0, 0), "closed by the sweep after the end");
    // A grant without the tick never opens it.
    t.set(vec![hour(&b())], Some(T + 3600), end);
    assert_eq!(TOLD_EXPIRY.load(Ordering::SeqCst), 0);
}

static TOLD_RACE: AtomicU64 = AtomicU64::new(0);
fn race_open(until: u64, _: &str) {
    TOLD_RACE.store(until, Ordering::SeqCst);
}
fn race_close(_: &str) {
    TOLD_RACE.store(0, Ordering::SeqCst);
}

/// Tables set and swept from several threads: once they are quiet, what the sink was told last is what the
/// live grants want (no stale "open" applied after a "closed").
#[test]
fn the_nvme_sink_follows_the_last_table_under_concurrency() {
    let t = std::sync::Arc::new(access::Table::with_sink(access::NvmeSink { open: race_open, close: race_close }));
    let g = Grant { nvme: true, ..hour(&a()) };
    for round in 0..20 {
        let hs: Vec<_> = (0..3)
            .map(|k| {
                let (t, g) = (t.clone(), g.clone());
                std::thread::spawn(move || {
                    for i in 0..200 {
                        match k {
                            0 => {
                                t.set(vec![g.clone()], Some(T), 0);
                            }
                            1 => {
                                t.set(Vec::new(), Some(T), 0);
                            }
                            _ => t.sweep(Some(T + (i % 2) as i64), 0),
                        }
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let want = if t.live(&a()) { 3600 * 1_000_000_000u64 } else { 0 };
        assert_eq!((TOLD_RACE.load(Ordering::SeqCst), t.nvme_told()), (want, want), "round {round}");
    }
}
