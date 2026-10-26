//! The real daemon and the `settings` command over the socket: requests, watch, hostile input, kill -9.

mod common;
use common::*;
use settingsd::json::{Json, parse};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(env: &Env) -> Daemon {
    let p = &env.paths;
    let child = Command::new(env!("CARGO_BIN_EXE_settingsd"))
        .args(["serve", "--deadline-ms", "1000"])
        .arg("--stick")
        .arg(&p.stick)
        .arg("--fallback")
        .arg(&p.fallback)
        .arg("--sys")
        .arg(&p.sys)
        .arg("--socket")
        .arg(&p.socket)
        .stderr(std::fs::File::create(env.root.join("daemon.err")).unwrap())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&p.socket).is_err() {
        assert!(Instant::now() < end, "settingsd did not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child }
}

struct Conn {
    w: UnixStream,
    r: BufReader<UnixStream>,
}

impl Conn {
    fn new(env: &Env) -> Conn {
        let w = UnixStream::connect(&env.paths.socket).unwrap();
        w.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Conn { r: BufReader::new(w.try_clone().unwrap()), w }
    }
    fn raw(&mut self, line: &[u8]) -> Json {
        self.w.write_all(line).unwrap();
        self.w.write_all(b"\n").unwrap();
        self.line()
    }
    fn line(&mut self) -> Json {
        let mut l = String::new();
        self.r.read_line(&mut l).unwrap();
        parse(l.trim()).unwrap_or_else(|e| panic!("{e}: {l:?}"))
    }
    fn req(&mut self, j: &str) -> Json {
        self.raw(j.as_bytes())
    }
}

fn cli(env: &Env, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_settings"))
        .arg("--socket")
        .arg(&env.paths.socket)
        .args(args)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into(),
        String::from_utf8_lossy(&o.stderr).into(),
    )
}

#[test]
fn socket_api_and_cli() {
    let env = Env::new("socket", true);
    let d = start(&env);
    let mut c = Conn::new(&env);
    let mut w = Conn::new(&env);
    assert_eq!(w.req(r#"{"op":"watch","prefix":"theme."}"#).str("watching"), Some("theme."));
    let a = c.req(r##"{"op":"set","key":"theme.accent","value":"#FF0000","source":"claude"}"##);
    assert_eq!(a.str("new"), Some("#ff0000"), "{a:?}");
    let ev = w.line();
    assert_eq!(
        (ev.str("event"), ev.str("key"), ev.str("source")),
        (Some("changed"), Some("theme.accent"), Some("claude"))
    );
    assert_eq!(c.req(r#"{"op":"get","key":"theme.accent"}"#).str("value"), Some("#ff0000"));
    let desc = c.req(r#"{"op":"describe","key":"theme.scale"}"#);
    let k = desc.get("key").unwrap();
    assert_eq!(k.get("schema").and_then(|s| s.get("min")), Some(&Json::Int(100)));
    assert!(k.get("text").and_then(|t| t.str("de")).is_some_and(|t| t.contains("Skalierung")));
    assert_eq!(c.req(r#"{"op":"set","key":"nvme_write","value":"on"}"#).str("error"), Some("refused_r8"));
    assert_eq!(c.req(r#"{"op":"register","owner":"settingsd","keys":[]}"#).str("error"), Some("not_owner"));
    let reg = c.req(
        r#"{"op":"register","owner":"ctxd","keys":[{"key":"context.depth","schema":{"type":"int","min":0,"max":9},"default":1,"text":{"de":"Tiefe","en":"Depth"}}]}"#,
    );
    assert_eq!(reg.get("registered"), Some(&Json::Int(1)), "{reg:?}");
    assert_eq!(c.req(r#"{"op":"status"}"#).str("place"), Some("stick"));

    // The command line, as Georg would use it in the shell.
    let (code, out, _) = cli(&env, &["set", "theme.scale", "150"]);
    assert_eq!(code, 0);
    assert!(out.contains("theme.scale: 125 -> 150"), "{out}");
    let (code, out, _) = cli(&env, &["get", "theme.scale"]);
    assert_eq!((code, out.trim()), (0, "150"));
    let (code, out, _) = cli(&env, &["list", "theme.font"]);
    assert_eq!(code, 0);
    assert!(out.contains("theme.font.ui = Exo 2"), "{out}");
    let (code, out, _) = cli(&env, &["history", "3"]);
    assert_eq!(code, 0);
    assert!(out.lines().next().unwrap().contains("theme.scale: 125 -> 150 by cli"), "{out}");
    let (code, out, _) = cli(&env, &["undo"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("undone theme.scale: 150 -> 125"), "{out}");
    let (code, _, err) = cli(&env, &["set", "nvme_write", "on"]);
    assert_eq!(code, 1);
    assert!(err.contains("refused_r8"), "{err}");
    let (code, _, _) = cli(&env, &["frobnicate"]);
    assert_eq!(code, 2);

    // kill -9 and a new start: everything is still there.
    drop(d);
    let _d = start(&env);
    let mut c = Conn::new(&env);
    assert_eq!(c.req(r#"{"op":"get","key":"theme.accent"}"#).str("value"), Some("#ff0000"));
    let h = c.req(r#"{"op":"history","n":10}"#);
    assert!(matches!(h.get("entries"), Some(Json::Arr(a)) if a.len() == 3), "{h:?}");
}

#[test]
fn hostile_input_is_answered_not_fatal() {
    let env = Env::new("hostile", true);
    let _d = start(&env);
    let mut c = Conn::new(&env);
    for bad in [
        &b"garbage"[..],
        b"{",
        b"[]",
        b"{\"op\":3}",
        b"{\"op\":\"nope\"}",
        b"\xff\xfe{}",
        b"{\"op\":\"undo\",\"n\":-1}",
    ] {
        let a = c.raw(bad);
        assert_eq!(a.get("ok"), Some(&Json::Bool(false)), "{bad:?}");
    }
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert_eq!(c.req(&deep).str("error"), Some("bad_request"));
    let huge = format!(r#"{{"op":"set","key":"theme.font.ui","value":"{}"}}"#, "x".repeat(2 << 20));
    let a = c.req(&huge);
    assert!(a.str("message").is_some_and(|m| m.contains("longer than")), "{a:?}");
    let a = c.req(r#"{"op":"set","key":"theme.font.ui","value":"Exo 2 Light"}"#);
    assert_eq!(a.str("new"), Some("Exo 2 Light"), "the connection still works");
    // A watcher that never reads cannot stall the others.
    let mut lazy = Conn::new(&env);
    lazy.req(r#"{"op":"watch"}"#);
    for i in 0..1500 {
        let a = c.req(&format!(r#"{{"op":"set","key":"theme.radius","value":{}}}"#, i % 30));
        assert_eq!(a.get("ok"), Some(&Json::Bool(true)));
    }
}

#[test]
fn socket_is_open_to_every_uid() {
    // init runs services with umask 022; connect(2) needs write permission on the socket.
    let _ = usys::umask(0o022);
    let env = Env::new("sockmode", true);
    let _d = start(&env);
    let m = std::fs::metadata(&env.paths.socket).unwrap();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777, 0o666);
}

#[test]
fn reads_do_not_wait_for_a_hung_stick() {
    let env = Env::new("hungread_sock", true);
    let _d = start(&env);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    let fifo = env.paths.stick.join("settings/theme.toml.new");
    mkfifo(&fifo);
    let mut c = Conn::new(&env);
    let setter = {
        let mut s = Conn::new(&env);
        std::thread::spawn(move || s.req(r#"{"op":"set","key":"theme.radius","value":9}"#))
    };
    std::thread::sleep(Duration::from_millis(150));
    // The set waits up to its deadline (1 s) for the stick; reads answer from the snapshot meanwhile.
    for op in [r#"{"op":"get","key":"theme.radius"}"#, r#"{"op":"status"}"#, r#"{"op":"list","prefix":"theme"}"#] {
        let t = Instant::now();
        let a = c.req(op);
        assert_eq!(a.get("ok"), Some(&Json::Bool(true)), "{op}");
        assert!(t.elapsed() < Duration::from_millis(500), "{op} waited {:?}", t.elapsed());
    }
    let a = setter.join().unwrap();
    assert_eq!(a.str("stored"), Some("fallback"));
    assert_eq!(c.req(r#"{"op":"get","key":"theme.radius"}"#).get("value"), Some(&Json::Int(9)));
    assert_eq!(c.req(r#"{"op":"status"}"#).str("place"), Some("fallback"));
}

#[test]
fn idle_and_stalled_clients_do_not_stop_the_daemon() {
    let env = Env::new("flood", true);
    let _d = start(&env);
    let idle: Vec<UnixStream> = (0..300).map(|_| UnixStream::connect(&env.paths.socket).unwrap()).collect();
    let mut half = UnixStream::connect(&env.paths.socket).unwrap();
    half.write_all(br#"{"op":"set","key":"theme.rad"#).unwrap();
    let mut c = Conn::new(&env);
    let t = Instant::now();
    let a = c.req(r#"{"op":"set","key":"theme.radius","value":3}"#);
    assert_eq!(a.get("ok"), Some(&Json::Bool(true)));
    assert!(t.elapsed() < Duration::from_secs(2), "waited {:?}", t.elapsed());
    assert_eq!(c.req(r#"{"op":"status"}"#).get("ok"), Some(&Json::Bool(true)));
    drop(idle);
    drop(half);
}
