//! bitsact against a fake kernel registry and a fake settingsd (std Unix sockets): registration, a read action, the
//! permission gate of a change action (deny, unreachable, an allow over MCP or without a journal entry refused,
//! Georg's allow), cancel, an unknown tool, typed-argument errors, a panicking action still answering.

use bitsact::events::Events;
use bitsact::{Action, Config, Ctx, Handler, Json, Reply, Safety};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct H;
impl Handler for H {
    fn call(&self, ctx: &Ctx) -> Reply {
        match ctx.call.name.as_str() {
            "t_read" => match ctx.args().u64_in("n", 1, 1, 5) {
                Ok(n) => Reply::text(format!("read {n}")),
                Err(r) => r,
            },
            "t_change" => Reply::text("changed"),
            "t_panic" => panic!("t_panic's bug"),
            "t_slow" => {
                while ctx.alive("slow").is_ok() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Reply::text(if ctx.cancelled() { "saw cancel" } else { "saw deadline" })
            }
            _ => Reply::error("?"),
        }
    }
}

/// A fake settingsd: `get` answers the current value of the one key (None = unknown), `register` makes it deny;
/// `history` answers the source of its newest change (`src`, None = no entry).
fn settingsd(path: std::path::PathBuf, value: Arc<Mutex<Option<String>>>, src: Arc<Mutex<Option<String>>>, log: Arc<Mutex<Vec<String>>>) {
    let l = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut w = s.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            let j = Json::parse(line.trim()).unwrap();
            log.lock().unwrap().push(line.trim().to_string());
            let a = match j.get("op").and_then(Json::as_str) {
                Some("register") => {
                    let mut v = value.lock().unwrap();
                    if v.is_none() {
                        *v = Some("deny".into());
                    }
                    r#"{"ok":true,"registered":1}"#.to_string()
                }
                Some("get") => match &*value.lock().unwrap() {
                    Some(v) => format!(r#"{{"ok":true,"value":"{v}","changed":false}}"#),
                    None => r#"{"ok":false,"error":"unknown_key","message":"no such key"}"#.to_string(),
                },
                Some("history") => {
                    let key = j.get("prefix").and_then(Json::as_str).unwrap_or("");
                    match &*src.lock().unwrap() {
                        Some(s) => format!(r#"{{"ok":true,"entries":[{{"seq":7,"key":"{key}","new":"allow","source":"{s}","op":"set"}}]}}"#),
                        None => r#"{"ok":true,"entries":[]}"#.to_string(),
                    }
                }
                _ => r#"{"ok":false,"error":"bad_request","message":"?"}"#.to_string(),
            };
            let _ = w.write_all(format!("{a}\n").as_bytes());
        }
    });
}

struct Kernel {
    w: UnixStream,
    r: BufReader<UnixStream>,
}

impl Kernel {
    fn line(&mut self) -> Json {
        let mut l = String::new();
        self.r.read_line(&mut l).unwrap();
        Json::parse(l.trim()).unwrap()
    }
    fn call(&mut self, id: u64, name: &str, args: &str, timeout_ms: u64) {
        let m = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args},"_meta":{{"progressToken":{id},"timeout_ms":{timeout_ms}}}}}}}"#);
        self.w.write_all(format!("{m}\n").as_bytes()).unwrap();
    }
    /// The answer to `id` (progress lines skipped): (isError or rpc error, text).
    fn answer(&mut self, id: u64) -> (bool, String) {
        loop {
            let j = self.line();
            if j.get("method").is_some() || j.get("id").and_then(Json::as_u64) != Some(id) {
                continue;
            }
            if let Some(e) = j.get("error") {
                return (true, format!("rpc {}", e.get("message").and_then(Json::as_str).unwrap_or("")));
            }
            let r = j.get("result").unwrap();
            let text = r.get("content").and_then(Json::as_array).map(|c| c.iter().filter_map(|i| i.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            return (r.get("isError").and_then(Json::as_bool) == Some(true), text);
        }
    }
}

#[test]
fn register_gate_cancel() {
    let dir = std::env::temp_dir().join(format!("bitsact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (ksock, ssock) = (dir.join("mcp.sock"), dir.join("settings.sock"));
    let kl = UnixListener::bind(&ksock).unwrap();
    let cfg = Config { service: "acttest".into(), mcp_socket: ksock.display().to_string(), settings_socket: ssock.display().to_string() };
    let actions = vec![
        Action::new("t_read", "read", r#"{"type":"object","properties":{"n":{"type":"integer"}}}"#, Safety::Read),
        Action::new("t_change", "change", r#"{"type":"object"}"#, Safety::Change).gate_text("Test darf ändern", "Test may change"),
        Action::new("t_slow", "slow", r#"{"type":"object"}"#, Safety::Read),
        Action::new("t_panic", "panics", r#"{"type":"object"}"#, Safety::Read),
    ];
    let (ev, events) = Events::capture("acttest");
    let svc = std::thread::spawn(move || bitsact::serve(&cfg, actions, Arc::new(H), ev));
    let (s, _) = kl.accept().unwrap();
    let mut k = Kernel { w: s.try_clone().unwrap(), r: BufReader::new(s) };
    let reg = k.line();
    assert_eq!(reg.get("method").and_then(Json::as_str), Some("register"));
    let tools = reg.get("params").and_then(|p| p.get("tools")).and_then(Json::as_array).unwrap().to_vec();
    assert_eq!(tools.len(), 4);
    assert_eq!(tools[1].get("safety").and_then(Json::as_str), Some("change"));
    k.w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"registered\":[\"t_read\",\"t_change\",\"t_slow\",\"t_panic\"]}}\n").unwrap();

    // A read action; a bad argument; an unknown tool.
    k.call(10, "t_read", r#"{"n":3}"#, 5000);
    assert_eq!(k.answer(10), (false, "read 3".into()));
    k.call(11, "t_read", r#"{"n":9}"#, 5000);
    let (e, t) = k.answer(11);
    assert!(e && t.contains("n: want an integer from 1 to 5"), "{t}");
    k.call(12, "t_nope", "{}", 5000);
    assert!(k.answer(12).0);

    // settingsd unreachable: the change action is refused (fail closed).
    k.call(20, "t_change", "{}", 5000);
    let (e, t) = k.answer(20);
    assert!(e && t.contains("cannot be read now") && t.contains("claude.actions.t_change"), "{t}");

    // settingsd up, key unknown: registered as deny, refused with the way to allow it.
    let value = Arc::new(Mutex::new(None));
    let src = Arc::new(Mutex::new(None));
    let slog = Arc::new(Mutex::new(Vec::new()));
    settingsd(ssock.clone(), value.clone(), src.clone(), slog.clone());
    k.call(21, "t_change", "{}", 5000);
    let (e, t) = k.answer(21);
    assert!(e && t.contains("not permitted") && t.contains("settings set claude.actions.t_change allow --confirm"), "{t}");
    let reg = slog.lock().unwrap().iter().find(|l| l.contains("\"register\"")).cloned().unwrap();
    for want in [r#""key":"claude.actions.t_change""#, r#""default":"deny""#, r#""safety":"confirm""#, r#""owner":"acttest""#, "Test darf ändern"] {
        assert!(reg.contains(want), "{want} in {reg}");
    }
    // An MCP client allows it itself (settings_set confirmed: the journal's source is mcp): still refused.
    *value.lock().unwrap() = Some("allow".into());
    *src.lock().unwrap() = Some("mcp".into());
    k.call(24, "t_change", "{}", 5000);
    let (e, t) = k.answer(24);
    assert!(e && t.contains("over MCP") && t.contains("not by Georg"), "{t}");
    // allow without a journal entry (trimmed away): refused, Georg sets it again.
    *src.lock().unwrap() = None;
    k.call(25, "t_change", "{}", 5000);
    let (e, t) = k.answer(25);
    assert!(e && t.contains("no longer says who set it"), "{t}");
    // Georg allows it (shell: source cli): the next call runs.
    *src.lock().unwrap() = Some("cli".into());
    k.call(22, "t_change", "{}", 5000);
    assert_eq!(k.answer(22), (false, "changed".into()));
    *value.lock().unwrap() = Some("deny".into());
    k.call(23, "t_change", "{}", 5000);
    assert!(k.answer(23).0);

    // A panicking action still answers (an error), and the service goes on.
    k.call(26, "t_panic", "{}", 5000);
    let (e, t) = k.answer(26);
    assert!(e && t.contains("t_panic's bug"), "{t}");
    k.call(27, "t_read", "{}", 5000);
    assert_eq!(k.answer(27), (false, "read 1".into()));

    // Cancel reaches the running call; a deadline without cancel ends it too. Calls run side by side.
    k.call(30, "t_slow", "{}", 60_000);
    k.call(31, "t_slow", "{}", 300);
    let t0 = Instant::now();
    assert_eq!(k.answer(31), (false, "saw deadline".into()));
    assert!(t0.elapsed() < Duration::from_secs(5));
    k.w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":30,\"reason\":\"timeout\"}}\n").unwrap();
    assert_eq!(k.answer(30), (false, "saw cancel".into()));

    drop(k);
    assert!(svc.join().unwrap().is_ok());
    let ev = events.lock().unwrap().join("\n");
    for want in ["acttest registered tools=4", "perm.refused tool=t_change", "perm.self_granted tool=t_change", "perm.no_journal", "perm.allowed tool=t_change", "call.panicked tool=t_panic", "call.cancelled id=30", "acttest ended"] {
        assert!(ev.contains(want), "{want} in\n{ev}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn key_names() {
    assert_eq!(bitsact::perm::key_for("brave_fill_form"), "claude.actions.brave_fill_form");
    assert_eq!(bitsact::perm::key_for("My.Tool"), "claude.actions.my_tool");
}
