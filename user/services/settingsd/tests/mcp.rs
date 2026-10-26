//! CL-2: the real daemon against a fake kernel registry speaking MC-6's line protocol (crates/mcp/src/svc/proto.rs):
//! registration, set/apply/undo of every key through the tools, R8 refusals, confirm and secret keys, bad
//! arguments, deadlines and cancellation, reconnecting.

mod common;
use common::*;
use settingsd::builtin;
use settingsd::json::{Json, parse};
use settingsd::key::KeyDef;
use settingsd::value::{Type, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    err: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn events(&self) -> String {
        std::fs::read_to_string(&self.err).unwrap_or_default()
    }
}

fn start(env: &Env, mcp: &Path, deadline_ms: u64) -> Daemon {
    let p = &env.paths;
    let err = env.root.join("daemon.err");
    let child = Command::new(env!("CARGO_BIN_EXE_settingsd"))
        .args(["serve", "--deadline-ms", &deadline_ms.to_string()])
        .args([&"--stick".into(), &p.stick, &"--fallback".into(), &p.fallback, &"--sys".into(), &p.sys])
        .args([&"--socket".into(), &p.socket, &"--mcp-socket".into(), &mcp.to_path_buf()])
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    Daemon { child, err }
}

fn listen(env: &Env) -> (UnixListener, PathBuf) {
    let path = env.root.join("mcp.sock");
    let l = UnixListener::bind(&path).unwrap();
    l.set_nonblocking(true).unwrap();
    (l, path)
}

/// One service connection as the kernel sees it.
struct Svc {
    w: UnixStream,
    r: BufReader<UnixStream>,
    next: u64,
    tools: Vec<Json>,
    early: HashMap<u64, Answer>,
}

#[derive(Debug, Clone)]
struct Answer {
    err: bool,
    bad: bool,
    text: String,
}

impl Answer {
    fn ok(&self, what: &str) -> Json {
        assert!(!self.err && !self.bad, "{what}: {self:?}");
        parse(&self.text).unwrap_or_else(|e| panic!("{what}: {e}: {}", self.text))
    }
    fn code(&self) -> &str {
        assert!(self.err, "want an error: {self:?}");
        self.text.split(':').next().unwrap_or("")
    }
}

fn accept(l: &UnixListener) -> UnixStream {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match l.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                return s;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("settingsd did not connect: {e}"),
        }
    }
}

fn read_json(r: &mut BufReader<UnixStream>) -> Json {
    let mut l = String::new();
    r.read_line(&mut l).unwrap();
    parse(l.trim()).unwrap_or_else(|e| panic!("{e}: {l:?}"))
}

/// Accepts settingsd and answers its `register` with `refuse` (an error message) or the names.
fn session(l: &UnixListener, refuse: Option<&str>) -> Option<Svc> {
    let s = accept(l);
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut w = s;
    let m = read_json(&mut r);
    assert_eq!(m.str("method"), Some("register"), "{m:?}");
    let id = m.get("id").and_then(Json::as_i64).unwrap();
    let p = m.get("params").unwrap();
    assert_eq!(p.str("service"), Some("settingsd"));
    let Some(Json::Arr(tools)) = p.get("tools") else { panic!("no tools") };
    let reply = match refuse {
        Some(why) => format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32602,"message":"{why}"}}}}"#),
        None => {
            let names: Vec<Json> = tools.iter().map(|t| Json::from(t.str("name").unwrap())).collect();
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"registered":{}}}}}"#, Json::from(names).to_text())
        }
    };
    w.write_all(format!("{reply}\n").as_bytes()).unwrap();
    refuse.is_none().then(|| Svc { w, r, next: 100, tools: tools.clone(), early: HashMap::new() })
}

impl Svc {
    fn send(&mut self, tool: &str, args: &str, timeout_ms: u64) -> u64 {
        self.next += 1;
        let id = self.next;
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{args},"_meta":{{"progressToken":{id},"timeout_ms":{timeout_ms}}}}}}}"#
        );
        self.w.write_all(format!("{line}\n").as_bytes()).unwrap();
        id
    }
    fn wait(&mut self, id: u64) -> Answer {
        loop {
            if let Some(a) = self.early.remove(&id) {
                return a;
            }
            let m = read_json(&mut self.r);
            let Some(got) = m.get("id").and_then(Json::as_i64) else { continue };
            let a = match (m.get("result"), m.get("error")) {
                (Some(r), _) => Answer {
                    err: r.get("isError").and_then(Json::as_bool).unwrap(),
                    bad: false,
                    text: match r.get("content") {
                        Some(Json::Arr(c)) => c[0].str("text").unwrap().to_string(),
                        _ => panic!("{m:?}"),
                    },
                },
                (None, Some(e)) => {
                    assert_eq!(e.get("code").and_then(Json::as_i64), Some(-32602), "{m:?}");
                    Answer { err: false, bad: true, text: e.str("message").unwrap_or("").to_string() }
                }
                _ => panic!("{m:?}"),
            };
            self.early.insert(got as u64, a);
        }
    }
    fn call(&mut self, tool: &str, args: &str) -> Answer {
        let id = self.send(tool, args, 10_000);
        self.wait(id)
    }
    fn get(&mut self, key: &str) -> Json {
        self.call("settings_get", &format!(r#"{{"key":"{key}"}}"#)).ok(key).get("value").cloned().unwrap()
    }
}

fn sample(d: &KeyDef) -> Value {
    match (&d.ty, &d.default) {
        (Type::Bool, Value::Bool(b)) => Value::Bool(!b),
        (Type::Int { min, max, .. }, Value::Int(n)) => Value::Int(if n == max { *min } else { *max }),
        // A word default (kernel.preload.max_mb = all): a number is the change.
        (Type::Int { min, word: Some(_), .. }, Value::Text(_)) => Value::Int(*min),
        (Type::Choice(c), v) => Value::Text(c.iter().find(|x| Value::Text((*x).clone()) != *v).unwrap().clone()),
        (Type::Color, _) => Value::Text("#123abc".into()),
        (Type::Text { .. }, _) => Value::Text("Test Font".into()),
        (Type::LogSpec, _) => Value::Text("warn,net=debug".into()),
        other => panic!("no sample for {other:?}"),
    }
}

/// Values of the right JSON shape but out of range, and values of the wrong type; each must be bad_value.
fn wrong(d: &KeyDef) -> Vec<String> {
    let mut v = match &d.ty {
        Type::Bool => vec![r#""maybe""#.to_string(), "5".into()],
        Type::Int { min, max, .. } => {
            [max.checked_add(1), min.checked_sub(1)].into_iter().flatten().map(|n| n.to_string()).collect()
        }
        Type::Choice(_) => vec![r#""no_such_choice""#.into()],
        Type::Color => vec![r##""#12""##.into(), r#""red""#.into()],
        Type::Text { max } => vec![format!("\"{}\"", "x".repeat(max + 1)), r#""a\tb""#.into()],
        Type::LogSpec => vec![r#""net=loud""#.into(), r#""=debug""#.into()],
    };
    v.push(if d.ty == Type::Bool { "7".into() } else { "true".into() });
    v
}

fn kernel_attrs(env: &Env) {
    std::fs::write(env.paths.sys.join("loglevel"), "info\n").unwrap();
    std::fs::write(env.paths.sys.join("console_log"), "warn\n").unwrap();
}

#[test]
fn tools_set_apply_undo_every_key() {
    let env = Env::new("mcp_every", true);
    kernel_attrs(&env);
    let (l, path) = listen(&env);
    let d = start(&env, &path, 1000);
    let mut k = session(&l, None).unwrap();
    let names: Vec<(&str, &str)> = k.tools.iter().map(|t| (t.str("name").unwrap(), t.str("safety").unwrap())).collect();
    assert_eq!(
        names,
        [
            ("settings_list", "read"),
            ("settings_describe", "read"),
            ("settings_get", "read"),
            ("settings_set", "change"),
            ("settings_undo", "change"),
            ("settings_history", "read")
        ]
    );
    let all = builtin::all();
    let list = k.call("settings_list", "{}").ok("list");
    assert!(matches!(list.get("keys"), Some(Json::Arr(a)) if a.len() == all.len()), "{list:?}");
    let cut = k.call("settings_list", r#"{"prefix":"theme","limit":3}"#).ok("list limit");
    assert!(matches!(cut.get("keys"), Some(Json::Arr(a)) if a.len() == 3));
    assert!(cut.get("more").and_then(Json::as_i64).unwrap() > 0);
    let desc = k.call("settings_describe", r#"{"key":"theme.mode"}"#).ok("describe");
    assert!(desc.get("key").and_then(|k| k.get("text")).is_some(), "{desc:?}");
    for def in &all {
        let before = k.get(&def.name);
        let journal = k.call("settings_history", r#"{"n":1}"#).text;
        for bad in wrong(def) {
            let a = k.call("settings_set", &format!(r#"{{"key":"{}","value":{bad}}}"#, def.name));
            assert_eq!(a.code(), "bad_value", "{} = {bad}: {a:?}", def.name);
        }
        let same = k.call("settings_set", &format!(r#"{{"key":"{}","value":{}}}"#, def.name, before.to_text()));
        assert_eq!(same.ok(&def.name).get("changed"), Some(&Json::Bool(false)), "{}: {same:?}", def.name);
        assert_eq!(k.call("settings_history", r#"{"n":1}"#).text, journal, "{}: refused or same value journaled", def.name);
        assert_eq!(k.get(&def.name), before, "{}", def.name);
        let v = sample(def).to_json();
        let a = k.call("settings_set", &format!(r#"{{"key":"{}","value":{}}}"#, def.name, v.to_text())).ok(&def.name);
        assert_eq!(a.get("changed"), Some(&Json::Bool(true)), "{}: {a:?}", def.name);
        assert_eq!(a.get("new"), Some(&v), "{}", def.name);
        assert!(a.str("applied").is_some() && a.get("restart").is_some() && a.get("seq").is_some(), "{a:?}");
        assert_eq!(k.get(&def.name), v, "{}", def.name);
        let h = k.call("settings_history", r#"{"n":1}"#).ok("history");
        let Some(Json::Arr(e)) = h.get("entries") else { panic!("{h:?}") };
        assert_eq!((e[0].str("key"), e[0].str("source")), (Some(def.name.as_str()), Some("mcp")), "{h:?}");
        let u = k.call("settings_undo", "{}").ok("undo");
        assert!(matches!(u.get("undone"), Some(Json::Arr(a)) if a.len() == 1), "{u:?}");
        assert_eq!(k.get(&def.name), before, "{} after undo", def.name);
    }
    assert!(!env.stick_file("CONFIG.TXT").contains("loglevel="), "kernel lines gone after undo");
    // Three changes, one undo of n=3: newest first, then nothing is left.
    let sets = [("theme.mode", r#""light""#), ("theme.radius", "4"), ("theme.scale", "150")];
    let olds: Vec<Json> = sets.iter().map(|(key, _)| k.get(key)).collect();
    for (key, v) in sets {
        k.call("settings_set", &format!(r#"{{"key":"{key}","value":{v}}}"#)).ok(key);
    }
    let u = k.call("settings_undo", r#"{"n":3}"#).ok("undo 3");
    let Some(Json::Arr(done)) = u.get("undone") else { panic!("{u:?}") };
    let keys: Vec<&str> = done.iter().map(|e| e.str("key").unwrap()).collect();
    assert_eq!(keys, ["theme.scale", "theme.radius", "theme.mode"], "{u:?}");
    for ((key, _), old) in sets.iter().zip(&olds) {
        assert_eq!(&k.get(key), old, "{key} after undo 3");
    }
    assert_eq!(k.call("settings_undo", "{}").code(), "nothing_to_undo");
    // The kernel checks `value` against this union (crates/mcp/src/schema.rs); the QEMU scenario proves it.
    let set = k.tools.iter().find(|t| t.str("name") == Some("settings_set")).unwrap();
    let ty = set.get("inputSchema").and_then(|s| s.get("properties")).and_then(|p| p.get("value")).and_then(|v| v.get("type"));
    assert_eq!(ty.map(Json::to_text).as_deref(), Some(r#"["boolean","integer","string"]"#), "{set:?}");
    let ev = d.events();
    assert_eq!(ev.matches("mcp.registered").count(), 1, "{ev}");
    assert!(ev.contains("settingsd mcp.call tool=settings_set key=theme.mode result=ok"), "{ev}");
}

#[test]
fn r8_switches_refused_and_nothing_changes() {
    let env = Env::new("mcp_r8", true);
    kernel_attrs(&env);
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "wlan=off\nnvme_write=off\nloglevel=info\n").unwrap();
    let (l, path) = listen(&env);
    let _d = start(&env, &path, 1000);
    let mut k = session(&l, None).unwrap();
    let cfg = std::fs::read(env.paths.stick.join("CONFIG.TXT")).unwrap();
    let hist = k.call("settings_history", r#"{"n":100}"#).text;
    for key in ["nvme_write", "stick_write", "usb_msd_write", "kernel.nvme_write", "kernel.usb_msd_write", "token", "kernel.wlan_psk"] {
        for v in ["true", "\"on\"", "1"] {
            let a = k.call("settings_set", &format!(r#"{{"key":"{key}","value":{v},"confirmed":true}}"#));
            assert_eq!(a.code(), "refused_r8", "{key}={v}: {a:?}");
        }
        assert_eq!(k.call("settings_get", &format!(r#"{{"key":"{key}"}}"#)).code(), "refused_r8", "{key}");
        assert_eq!(k.call("settings_describe", &format!(r#"{{"key":"{key}"}}"#)).code(), "refused_r8", "{key}");
    }
    assert_eq!(std::fs::read(env.paths.stick.join("CONFIG.TXT")).unwrap(), cfg, "CONFIG.TXT byte-identical");
    assert_eq!(k.call("settings_history", r#"{"n":100}"#).text, hist, "nothing journaled");
    assert_eq!(k.call("settings_undo", "{}").code(), "nothing_to_undo");
}

#[test]
fn confirm_secret_and_bad_arguments() {
    let env = Env::new("mcp_confirm", true);
    let (l, path) = listen(&env);
    let d = start(&env, &path, 1000);
    let mut k = session(&l, None).unwrap();
    let mut c = UnixStream::connect(&env.paths.socket).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(br#"{"op":"register","owner":"testsvc","keys":[{"key":"test.risky","schema":{"type":"int","min":0,"max":9},"default":1,"text":{"de":"R","en":"R"},"safety":"confirm"},{"key":"test.pass","schema":{"type":"text","max_bytes":64},"default":"","text":{"de":"P","en":"P"},"safety":"secret"}]}"#).unwrap();
    c.write_all(b"\n").unwrap();
    let reg = read_json(&mut BufReader::new(c.try_clone().unwrap()));
    assert_eq!(reg.get("ok"), Some(&Json::Bool(true)), "{reg:?}");

    let a = k.call("settings_set", r#"{"key":"test.risky","value":5}"#);
    assert_eq!(a.code(), "needs_confirm");
    assert!(a.text.contains("confirmed"), "{a:?}");
    assert_eq!(k.get("test.risky"), Json::Int(1));
    k.call("settings_set", r#"{"key":"test.risky","value":5,"confirmed":true}"#).ok("confirmed set");
    assert_eq!(k.get("test.risky"), Json::Int(5));
    assert_eq!(k.call("settings_undo", r#"{"n":1}"#).code(), "needs_confirm");
    k.call("settings_undo", r#"{"n":1,"confirmed":true}"#).ok("confirmed undo");
    assert_eq!(k.get("test.risky"), Json::Int(1));

    let secret = "hunter2-geheim";
    let a = k.call("settings_set", &format!(r#"{{"key":"test.pass","value":"{secret}"}}"#));
    assert!(!a.text.contains(secret), "{a:?}");
    a.ok("secret set");
    assert_eq!(k.get("test.pass"), Json::from("***"));
    // `owner` would reveal a secret on the socket; the tool does not take it over.
    let a = k.call("settings_get", r#"{"key":"test.pass","owner":"testsvc"}"#);
    assert!(!a.text.contains(secret), "{a:?}");
    for t in [("settings_list", "{}"), ("settings_history", r#"{"n":50}"#), ("settings_describe", r#"{"key":"test.pass"}"#)] {
        assert!(!k.call(t.0, t.1).text.contains(secret), "{t:?}");
    }
    assert_eq!(k.call("settings_undo", "{}").code(), "not_undoable");

    for (tool, args) in [
        ("settings_set", r#"{"key":"theme.scale"}"#),
        ("settings_set", r#"{"key":"theme.scale","value":[1]}"#),
        ("settings_set", r#"{"key":"theme.scale","value":{"a":1}}"#),
        ("settings_set", r#"{"key":"theme.scale","value":125,"confirmed":"yes"}"#),
        ("settings_get", r#"{"key":5}"#),
        ("settings_get", "{}"),
        ("settings_undo", r#"{"n":0}"#),
        ("settings_undo", r#"{"n":-1}"#),
        ("settings_history", r#"{"n":1.5}"#),
        ("settings_list", r#"{"limit":"all"}"#),
        ("settings_set", r#"{"key":"theme.scale","value":9007199254740993}"#),
        ("settings_set", r#"{"key":"theme.scale","value":-9007199254740993}"#),
        ("settings_nope", "{}"),
    ] {
        assert!(k.call(tool, args).bad, "{tool} {args}");
    }
    assert_eq!(k.call("settings_get", r#"{"key":"nope.key"}"#).code(), "unknown_key");
    assert_eq!(k.call("settings_set", r#"{"key":"theme.scale","value":"loud"}"#).code(), "bad_value");
    assert_eq!(k.get("theme.scale"), Json::Int(125));
    let ev = d.events();
    assert!(!ev.contains(secret), "{ev}");
}

#[test]
fn reads_do_not_wait_and_stale_changes_are_not_started() {
    let env = Env::new("mcp_hung", true);
    let (l, path) = listen(&env);
    let d = start(&env, &path, 1000);
    let mut k = session(&l, None).unwrap();
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    mkfifo(&env.paths.stick.join("settings/theme.toml.new"));
    // The first change waits up to its deadline (1 s) for the stick; the others queue behind it.
    let slow = k.send("settings_set", r#"{"key":"theme.radius","value":9}"#, 10_000);
    std::thread::sleep(Duration::from_millis(150));
    let stale = k.send("settings_set", r#"{"key":"theme.radius","value":4}"#, 200);
    let cancelled = k.send("settings_set", r#"{"key":"theme.radius","value":7}"#, 10_000);
    k.w.write_all(format!("{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{{\"requestId\":{cancelled},\"reason\":\"stop\"}}}}\n").as_bytes()).unwrap();
    let t = Instant::now();
    for (tool, args) in [("settings_get", r#"{"key":"theme.radius"}"#), ("settings_list", r#"{"prefix":"theme"}"#)] {
        k.call(tool, args).ok(tool);
    }
    assert!(t.elapsed() < Duration::from_millis(500), "reads waited {:?}", t.elapsed());
    let a = k.wait(slow).ok("slow set");
    assert_eq!(a.str("stored"), Some("fallback"));
    let after = k.call("settings_history", r#"{"n":10}"#).ok("history");
    let Some(Json::Arr(e)) = after.get("entries") else { panic!() };
    assert_eq!(e.len(), 1, "only the first change ran: {after:?}");
    assert_eq!(k.get("theme.radius"), Json::Int(9));
    assert!(!k.early.contains_key(&stale) && !k.early.contains_key(&cancelled));
    let ev = d.events();
    assert!(ev.contains("mcp.skipped tool=settings_set key=theme.radius reason=deadline"), "{ev}");
    assert!(ev.contains(&format!("mcp.cancelled call={cancelled}")), "{ev}");
}

#[test]
fn changes_queued_when_the_connection_ends_are_not_started() {
    let env = Env::new("mcp_lost", true);
    let (l, path) = listen(&env);
    let d = start(&env, &path, 1000);
    let mut k = session(&l, None).unwrap();
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    mkfifo(&env.paths.stick.join("settings/theme.toml.new"));
    let _slow = k.send("settings_set", r#"{"key":"theme.radius","value":9}"#, 10_000);
    std::thread::sleep(Duration::from_millis(150));
    let q1 = k.send("settings_set", r#"{"key":"theme.radius","value":4}"#, 10_000);
    let q2 = k.send("settings_undo", r#"{"n":1}"#, 10_000);
    std::thread::sleep(Duration::from_millis(100));
    // The registry goes away (the kernel ends all open calls with it) while the first change waits for the stick.
    k.w.shutdown(std::net::Shutdown::Both).unwrap();
    drop(k);
    let mut k = session(&l, None).unwrap();
    // The running change finishes after its 1 s stick deadline; the queued ones must not follow it.
    std::thread::sleep(Duration::from_millis(1500));
    let h = k.call("settings_history", r#"{"n":10}"#).ok("history");
    let Some(Json::Arr(e)) = h.get("entries") else { panic!("{h:?}") };
    assert_eq!(e.len(), 1, "only the running change happened: {h:?}");
    assert_eq!(k.get("theme.radius"), Json::Int(9));
    let ev = d.events();
    for id in [q1, q2] {
        assert!(ev.contains(&format!("mcp.skipped call={id} reason=connection_lost")), "{ev}");
    }
}

#[test]
fn waits_for_the_registry_and_registers_again() {
    let env = Env::new("mcp_again", true);
    let path = env.root.join("mcp.sock");
    let d = start(&env, &path, 1000);
    // No registry yet: settingsd keeps trying (several retries), reports it once.
    std::thread::sleep(Duration::from_millis(800));
    let (l, _) = listen(&env);
    assert!(session(&l, Some("names starting with settings are for uid 0")).is_none());
    assert!(session(&l, Some("names starting with settings are for uid 0")).is_none());
    let k = session(&l, None).unwrap();
    k.w.shutdown(std::net::Shutdown::Both).unwrap();
    drop(k);
    let mut k = session(&l, None).unwrap();
    assert_eq!(k.get("theme.mode"), Json::from("dark"));
    let ev = d.events();
    assert_eq!(ev.matches("mcp.waiting").count(), 1, "{ev}");
    assert_eq!(ev.matches("mcp.refused").count(), 1, "{ev}");
    assert_eq!(ev.matches("mcp.registered").count(), 2, "{ev}");
    assert_eq!(ev.matches("mcp.lost").count(), 1, "{ev}");
    // The settings socket served all along.
    assert!(UnixStream::connect(&env.paths.socket).is_ok());
}
