//! CL-S: a secret belongs to the program that registered it. Who asks is what the kernel says about the
//! connection, never the `owner` a request names. First with peers made up here, then with two real programs
//! on the daemon's socket (this test binary and the `settings` command).

mod common;
use common::*;
use settingsd::json::{Json, parse};
use settingsd::key::KeyDef;
use settingsd::peer::{self, Peer, Seen};
use settingsd::proto::{self, Reply};
use settingsd::registry::Registry;
use settingsd::server::{Shared, serve_by};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A value no answer to a stranger, no event and no journal may hold.
const SECRET: &str = "sk-cls-NEVER-SHOWN-4e1b";

fn def(key: &str, owner: &str, safety: &str) -> KeyDef {
    let j = format!(
        r#"{{"key":"{key}","schema":{{"type":"text","max_bytes":200}},"default":"","text":{{"de":"S","en":"S"}},"safety":"{safety}"}}"#
    );
    KeyDef::from_json(&parse(&j).unwrap(), owner).unwrap()
}

/// A package's own program, outside the system's directory.
fn vault() -> Peer {
    Peer::program(1000, "/data/pkgs/vault/bin/vault")
}

/// Another program Georg runs: a game, a browser's helper.
fn stranger() -> Peer {
    Peer::program(1000, "/data/home/1000/dl/game")
}

fn clear(r: &Registry, key: &str, owner: &str, p: &Peer) -> Result<Json, &'static str> {
    r.get(key, Some(owner), p).map(|j| j.get("value").cloned().unwrap()).map_err(|e| e.code)
}

/// A registry with `vault.pass` registered by the vault program and set to [`SECRET`].
fn with_vault(env: &Env) -> (Registry, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let (mut r, ev) = env.open();
    r.register("vault", vec![def("vault.pass", "vault", "secret")], &vault()).unwrap();
    r.set("vault.pass", &s(SECRET), &WHO).unwrap();
    (r, ev)
}

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn only_the_owners_program_reads_a_secret() {
    let env = Env::new("peer_read", true);
    let (r, ev) = with_vault(&env);
    assert_eq!(clear(&r, "vault.pass", "vault", &vault()), Ok(s(SECRET)));
    // Naming the owner is not being the owner.
    assert_eq!(clear(&r, "vault.pass", "vault", &stranger()), Err("not_owner"));
    assert_eq!(clear(&r, "vault.pass", "game", &stranger()), Err("not_owner"));
    // Nor is a caller without a program (the MCP bridge), nor the same program run by another user.
    assert_eq!(clear(&r, "vault.pass", "vault", &peer::INSIDE), Err("not_owner"));
    assert_eq!(clear(&r, "vault.pass", "vault", &Peer::program(1001, "/data/pkgs/vault/bin/vault")), Err("not_owner"));
    // Root reads the files anyway.
    assert_eq!(clear(&r, "vault.pass", "vault", &Peer::program(0, "/data/pkgs/vault/bin/vault")), Ok(s(SECRET)));
    assert_eq!(value(&r, "vault.pass"), s("***"));
    let d = r.describe("vault.pass").unwrap().to_text();
    assert!(d.contains(r#""owner_program":"/data/pkgs/vault/bin/vault""#) && !d.contains(SECRET), "{d}");

    let log = ev.lock().unwrap().join("\n");
    let want = "settingsd owner.refused what=get key=vault.pass owner=vault pid=0 uid=1000 exe=/data/home/1000/dl/game";
    assert!(log.contains(want), "{log}");
    assert_eq!(log.matches("owner.refused").count(), 4, "{log}");
    assert!(!log.contains(SECRET), "{log}");
}

#[test]
fn a_stranger_cannot_take_a_secret_key_or_its_owners_name() {
    let env = Env::new("peer_take", true);
    let (mut r, ev) = with_vault(&env);
    let reg = |r: &mut Registry, owner: &str, key: &str, safety: &str, p: &Peer| {
        r.register(owner, vec![def(key, owner, safety)], p).map(|_| ()).map_err(|e| e.code)
    };
    // The same key name, under the owner's name or another, as a secret or as an ordinary key.
    assert_eq!(reg(&mut r, "vault", "vault.pass", "secret", &stranger()), Err("not_owner"));
    assert_eq!(reg(&mut r, "vault", "vault.pass", "normal", &stranger()), Err("not_owner"));
    assert_eq!(reg(&mut r, "game", "vault.pass", "normal", &stranger()), Err("not_owner"));
    // Any key under the owner's name, and taking the owner's keys away.
    assert_eq!(reg(&mut r, "vault", "vault.other", "normal", &stranger()), Err("not_owner"));
    assert_eq!(r.unregister("vault", "vault", true, &stranger()).unwrap_err().code, "not_owner");
    assert_eq!(clear(&r, "vault.pass", "vault", &vault()), Ok(s(SECRET)));
    assert_eq!(ev.lock().unwrap().iter().filter(|l| l.contains("owner.refused what=register")).count(), 4);
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("owner.refused what=unregister key=- owner=vault")));

    // Writing: the owner's program, or a change Georg confirmed; never the stranger's or a model's own word.
    let shared = Shared::new(r);
    let ask = |p: &Peer, line: &str| match proto::handle(&shared, p, line) {
        Reply::Line(l) => parse(&l).unwrap(),
        Reply::Watch(_) => panic!("not a watch"),
    };
    let set = |v: &str, more: &str| format!(r#"{{"op":"set","key":"vault.pass","value":"{v}"{more}}}"#);
    assert_eq!(ask(&stranger(), &set("mine", "")).str("error"), Some("not_owner"));
    assert_eq!(ask(&peer::INSIDE, &set("mine", "")).str("error"), Some("not_owner"));
    assert_eq!(ask(&stranger(), r#"{"op":"reset","key":"vault.pass"}"#).str("error"), Some("not_owner"));
    let get = r#"{"op":"get","key":"vault.pass","owner":"vault"}"#;
    assert_eq!(ask(&vault(), get).get("value"), Some(&s(SECRET)));
    assert_eq!(ask(&vault(), &set("new-by-owner", "")).get("ok"), Some(&Json::Bool(true)));
    // INT-CL-4: "confirmed" is Georg's own yes only from the session's compositor (or root), see tests/vouch.rs.
    assert_eq!(ask(&stranger(), &set("typed-by-a-game", r#","confirmed":true"#)).str("error"), Some("not_confirmer"));
    assert_eq!(ask(&vault(), get).get("value"), Some(&s("new-by-owner")));
    assert_eq!(ask(&prog("bitscomp"), &set("typed-by-georg", r#","confirmed":true"#)).get("ok"), Some(&Json::Bool(true)));
    assert_eq!(ask(&stranger(), get).str("error"), Some("not_owner"));
    assert_eq!(ask(&vault(), get).get("value"), Some(&s("typed-by-georg")));
    // An ordinary key is as open as before.
    assert_eq!(ask(&stranger(), r#"{"op":"set","key":"theme.scale","value":125}"#).get("ok"), Some(&Json::Bool(true)));
}

#[test]
fn the_owner_is_remembered_across_a_restart() {
    let env = Env::new("peer_restart", true);
    let (r, _) = with_vault(&env);
    drop(r);
    let list = env.stick_file("settings/owners.list");
    assert_eq!(list, "{\"key\":\"vault.pass\",\"owner\":\"vault\",\"uid\":1000,\"exe\":\"/data/pkgs/vault/bin/vault\"}\n");

    let (mut r, ev) = env.open();
    // The owner has not come back yet: the key is not registered, its value waits in the file.
    assert_eq!(r.get("vault.pass", None, &stranger()).unwrap_err().code, "unknown_key");
    for (owner, safety) in [("vault", "secret"), ("vault", "normal"), ("game", "secret"), ("game", "normal")] {
        let e = r.register(owner, vec![def("vault.pass", owner, safety)], &stranger()).unwrap_err();
        assert_eq!(e.code, "not_owner", "{owner} {safety}");
    }
    assert_eq!(ev.lock().unwrap().iter().filter(|l| l.contains("owner.refused what=register")).count(), 4);
    r.register("vault", vec![def("vault.pass", "vault", "secret")], &vault()).unwrap();
    assert_eq!(clear(&r, "vault.pass", "vault", &vault()), Ok(s(SECRET)));
    assert_eq!(clear(&r, "vault.pass", "vault", &stranger()), Err("not_owner"));

    // Only when the owner takes key and value away is the name free again.
    r.unregister("vault", "vault", true, &vault()).unwrap();
    assert!(!env.paths.stick.join("settings/owners.list").exists());
    r.register("game", vec![def("vault.pass", "game", "secret")], &stranger()).unwrap();
    assert_eq!(clear(&r, "vault.pass", "game", &stranger()), Ok(s("")));
}

#[test]
fn a_reused_pid_does_not_inherit() {
    let env = Env::new("peer_pid", true);
    let (mut r, _) = env.open();
    let owner = Peer { pid: 4242, ..vault() };
    r.register("vault", vec![def("vault.pass", "vault", "secret")], &owner).unwrap();
    r.set("vault.pass", &s(SECRET), &WHO).unwrap();
    assert_eq!(clear(&r, "vault.pass", "vault", &owner), Ok(s(SECRET)));
    // The owner exits; its pid is given to another program, which connects.
    let reused = Peer { pid: 4242, ..stranger() };
    assert_eq!(clear(&r, "vault.pass", "vault", &reused), Err("not_owner"));
    assert_eq!(r.register("vault", vec![def("vault.x", "vault", "normal")], &reused).unwrap_err().code, "not_owner");
    // The owner's program under a new pid is the owner.
    assert_eq!(clear(&r, "vault.pass", "vault", &Peer { pid: 77, ..vault() }), Ok(s(SECRET)));
    // No pid is ever kept: nothing to inherit. (A pid that changes hands while it is looked up: src/peer.rs.)
    assert!(!env.stick_file("settings/owners.list").contains("4242"));
}

/// A program on the socket that settingsd could not tell (it left, or its file was replaced, before it was
/// looked at) gets nothing either, but is told so by a code of its own, and the log names no intruder.
#[test]
fn a_program_that_could_not_be_told_is_told_so() {
    let env = Env::new("peer_untold", true);
    let (mut r, ev) = with_vault(&env);
    let gone = Peer { pid: 9, ..stranger() };
    let gone = Peer { exe: None, ..gone };
    assert_eq!(clear(&r, "vault.pass", "vault", &gone), Err("peer_unknown"));
    let reg = |r: &mut Registry, owner: &str, key: &str, safety: &str| {
        r.register(owner, vec![def(key, owner, safety)], &gone).map(|_| ()).map_err(|e| e.code)
    };
    assert_eq!(reg(&mut r, "vault", "vault.other", "normal"), Err("peer_unknown"));
    assert_eq!(reg(&mut r, "safe", "safe.pass", "secret"), Err("peer_unknown"));
    assert_eq!(reg(&mut r, "assist", "ai.provider.t.key", "secret"), Err("peer_unknown"));
    assert_eq!(r.unregister("vault", "vault", true, &gone).unwrap_err().code, "peer_unknown");
    assert_eq!(r.check_secret_write("vault.pass", &gone, false).unwrap_err().code, "peer_unknown");
    // Where the answer is no whoever asks, it stays the plain one.
    assert_eq!(clear(&r, "vault.pass", "game", &gone), Err("not_owner"));
    assert_eq!(reg(&mut r, "game", "vault.pass", "normal"), Err("not_owner"));
    assert_eq!(reg(&mut r, "game", "ai.provider.t.key", "secret"), Err("not_owner"));
    // An ordinary key of its own needs no telling.
    assert_eq!(reg(&mut r, "safe", "safe.colour", "normal"), Ok(()));
    assert_eq!(clear(&r, "vault.pass", "vault", &vault()), Ok(s(SECRET)));

    let log = ev.lock().unwrap().join("\n");
    assert!(log.contains("settingsd peer.unknown what=get key=vault.pass owner=vault pid=9 uid=1000 exe=-"), "{log}");
    assert_eq!(log.matches("peer.unknown").count(), 6, "{log}");
    assert_eq!(log.matches("owner.refused").count(), 3, "{log}");
    assert!(!log.contains(SECRET), "{log}");
}

static LOOKUPS: AtomicUsize = AtomicUsize::new(0);
static STICK_ANSWERS: AtomicBool = AtomicBool::new(false);

/// The first client's lookup hangs, as a walk over its program's path does on a stick that does not answer.
fn first_hangs(seen: Seen) -> Peer {
    if LOOKUPS.fetch_add(1, Ordering::SeqCst) == 0 {
        while !STICK_ANSWERS.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    seen.settle()
}

/// B4: telling one client's program may wait for a disk; nobody else waits with it.
#[test]
fn a_lookup_that_hangs_holds_up_nobody_else() {
    let env = Env::new("peer_hang", true);
    let (r, _) = env.open();
    let listener = UnixListener::bind(&env.paths.socket).unwrap();
    let shared = std::sync::Arc::new(Shared::new(r));
    std::thread::spawn(move || serve_by(listener, shared, first_hangs));
    let ask = || {
        let mut w = UnixStream::connect(&env.paths.socket).unwrap();
        writeln!(w, r#"{{"op":"status"}}"#).unwrap();
        w
    };
    let answered = |w: &UnixStream, ms: u64| {
        w.set_read_timeout(Some(Duration::from_millis(ms))).unwrap();
        let mut a = String::new();
        BufReader::new(w).read_line(&mut a).is_ok() && a.contains(r#""ok":true"#)
    };
    let first = ask();
    assert!(!answered(&first, 300), "the first client's lookup did not hang");
    let me = std::env::current_exe().unwrap();
    for _ in 0..3 {
        // Later clients are served, and told: this test binary is the program on the socket.
        let mut w = ask();
        assert!(answered(&w, 10_000), "a client was held up behind another one's lookup");
        writeln!(w, "{}", register("hang", "hang.pass", "secret")).unwrap();
        assert!(answered(&w, 10_000));
    }
    assert!(env.stick_file("settings/owners.list").contains(me.to_str().unwrap()));
    STICK_ANSWERS.store(true, Ordering::SeqCst);
    assert!(answered(&first, 10_000), "the first client got no answer after its lookup came back");
}

#[test]
fn secrets_in_a_shared_namespace_are_for_system_programs() {
    let env = Env::new("peer_shared", true);
    let (mut r, ev) = env.open();
    let key = "ai.provider.x.key";
    // Not a stranger under any name, not a caller without a program, and not another program from the
    // system's directory either: `nc` and the `settings` command say whatever their caller types.
    let tries = [
        ("game", stranger()),
        ("assist", stranger()),
        ("assist", peer::INSIDE.clone()),
        ("nc", prog("nc")),
        ("assist", prog("settings")),
        ("settings", prog("settings")),
    ];
    for (owner, p) in &tries {
        assert_eq!(r.register(owner, vec![def(key, owner, "secret")], p).unwrap_err().code, "not_owner", "{owner}");
    }
    assert_eq!(ev.lock().unwrap().iter().filter(|l| l.contains("owner.refused what=register key=ai.provider.x.key")).count(), 6);
    // Nor does anyone else register these keys as ordinary ones (a stored key would show in clear).
    assert_eq!(r.register("game", vec![def(key, "game", "normal")], &stranger()).unwrap_err().code, "not_owner");
    // Elsewhere a shared namespace has no secrets at all, not even assist's.
    assert_eq!(r.register("bitscomp", vec![def("theme.pass", "bitscomp", "secret")], &prog("bitscomp")).unwrap_err().code, "not_owner");
    assert_eq!(r.register("assist", vec![def("ai.other_key", "assist", "secret")], &prog("assist")).unwrap_err().code, "not_owner");
    // The assistant's name and its cloud notice (CL-3s): whoever makes these keys chooses their default (a
    // name of any length for the system prompt, the notice off), so nobody but assist's program makes them.
    for key in ["ai.cloud_notice", "ai.assistant.name", "ai.assistant"] {
        for (owner, p) in [("game", stranger()), ("assist", stranger()), ("settings", prog("settings"))] {
            assert_eq!(r.register(owner, vec![def(key, owner, "normal")], &p).unwrap_err().code, "not_owner", "{key} {owner}");
        }
    }
    // Since CL-5s settingsd brings both keys itself: not even assist's program makes them again.
    for key in ["ai.cloud_notice", "ai.assistant.name"] {
        assert_eq!(r.register("assist", vec![def(key, "assist", "normal")], &prog("assist")).unwrap_err().code, "not_owner", "{key}");
        assert_eq!(r.describe(key).unwrap().get("key").and_then(|k| k.str("owner").map(str::to_string)).as_deref(), Some("settingsd"));
    }
    // An ordinary key there is as open as before, and a secret in a namespace of one's own too.
    r.register("game", vec![def("ai.game_hint", "game", "normal")], &stranger()).unwrap();
    r.register("game", vec![def("game.pass", "game", "secret")], &stranger()).unwrap();
    r.register("assist", vec![def(key, "assist", "secret")], &prog("assist")).unwrap();
    r.set(key, &s(SECRET), &WHO).unwrap();
    assert_eq!(clear(&r, key, "assist", &prog("assist")), Ok(s(SECRET)));
    assert_eq!(clear(&r, key, "assist", &stranger()), Err("not_owner"));
}

/// A provider key stored before owners were recorded (the Mini-PC's ai.toml from CL-3a), or whose record is
/// lost: nobody but assist's program gets to register the key, so nobody else sees the value.
#[test]
fn a_stored_key_without_a_record_is_still_only_assists() {
    let env = Env::new("peer_legacy", true);
    let key = "ai.provider.x.key";
    let (mut r, _) = env.open();
    r.register("assist", vec![def(key, "assist", "secret")], &prog("assist")).unwrap();
    r.set(key, &s(SECRET), &WHO).unwrap();
    drop(r);
    std::fs::remove_file(env.paths.stick.join("settings/owners.list")).unwrap();
    assert!(env.stick_file("settings/ai.toml").contains(SECRET));

    let (mut r, _) = env.open();
    for (owner, safety, p) in [("game", "normal", stranger()), ("assist", "normal", stranger()), ("settings", "secret", prog("settings"))] {
        assert_eq!(r.register(owner, vec![def(key, owner, safety)], &p).unwrap_err().code, "not_owner", "{owner} {safety}");
    }
    r.register("assist", vec![def(key, "assist", "secret")], &prog("assist")).unwrap();
    assert_eq!(clear(&r, key, "assist", &prog("assist")), Ok(s(SECRET)));
    assert_eq!(clear(&r, key, "assist", &stranger()), Err("not_owner"));
    assert!(env.stick_file("settings/owners.list").contains(key));
}

#[test]
fn a_record_root_made_becomes_the_first_users() {
    let env = Env::new("peer_root", true);
    let (mut r, _) = env.open();
    let assist = |uid| Peer::program(uid, &format!("{BIN}/assist"));
    let key = "ai.provider.x.key";
    // At the console (root): every user of the program reads, a stranger does not.
    r.register("assist", vec![def(key, "assist", "secret")], &assist(0)).unwrap();
    r.set(key, &s(SECRET), &WHO).unwrap();
    assert_eq!(clear(&r, key, "assist", &assist(1000)), Ok(s(SECRET)));
    assert_eq!(clear(&r, key, "assist", &stranger()), Err("not_owner"));
    // From the desktop: the record is that user's from now on.
    r.register("assist", vec![def(key, "assist", "secret")], &assist(1000)).unwrap();
    assert!(env.stick_file("settings/owners.list").contains(r#""uid":1000"#));
    assert_eq!(clear(&r, key, "assist", &assist(1001)), Err("not_owner"));
    assert_eq!(clear(&r, key, "assist", &assist(1000)), Ok(s(SECRET)));
    // Root still passes and does not take the record back.
    r.register("assist", vec![def(key, "assist", "secret")], &assist(0)).unwrap();
    assert_eq!(clear(&r, key, "assist", &assist(0)), Ok(s(SECRET)));
    assert!(env.stick_file("settings/owners.list").contains(r#""uid":1000"#));
}

#[test]
fn a_key_registered_while_the_stick_was_away_goes_back_to_its_owner() {
    let env = Env::new("peer_resync", true);
    let (r, _) = with_vault(&env);
    drop(r);
    let away = env.root.join("stick.away");
    std::fs::rename(&env.paths.stick, &away).unwrap();
    // settingsd starts before the stick is there (as at every boot): it knows no record yet.
    let (mut r, ev) = env.open();
    r.register("game", vec![def("vault.pass", "game", "normal")], &stranger()).unwrap();
    r.set("vault.pass", &s("planted"), &WHO).unwrap();
    std::fs::rename(&away, &env.paths.stick).unwrap();
    assert!(r.tick_now());
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("owner.reclaimed key=vault.pass owner=vault")), "{:?}", ev.lock().unwrap());
    // The stranger's registration is gone and never showed the stored value; the value is the owner's own.
    assert_eq!(r.get("vault.pass", None, &stranger()).unwrap_err().code, "unknown_key");
    assert!(env.stick_file("settings/vault.toml").contains(SECRET));
    assert_eq!(r.register("game", vec![def("vault.pass", "game", "normal")], &stranger()).unwrap_err().code, "not_owner");
    assert_eq!(r.register("game", vec![def("vault.else", "game", "normal")], &stranger()).unwrap_err().code, "not_owner");
    r.register("vault", vec![def("vault.pass", "vault", "secret")], &vault()).unwrap();
    assert_eq!(clear(&r, "vault.pass", "vault", &vault()), Ok(s(SECRET)));
}

#[test]
fn the_files_are_for_settingsd_alone() {
    let env = Env::new("peer_modes", true);
    let (r, ev) = with_vault(&env);
    let dir = env.paths.stick.join("settings");
    assert_eq!(mode(&dir), 0o700);
    for f in ["vault.toml", "journal.log", "owners.list"] {
        assert_eq!(mode(&dir.join(f)), 0o600, "{f}");
    }
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("secrets.readable")));
    drop(r);
    // A file system that shows the files open to others (the stick's FAT today) is reported at load.
    std::fs::set_permissions(dir.join("vault.toml"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let (_r, ev) = env.open();
    let log = ev.lock().unwrap().join("\n");
    assert!(log.contains("settingsd secrets.readable dir=") && log.contains("others=044 secrets=1"), "{log}");

    // Without a stick: the fallback directory and its files.
    let env = Env::new("peer_modes_fb", false);
    let (_r, _) = with_vault(&env);
    assert_eq!(mode(&env.paths.fallback), 0o700);
    for f in ["vault.toml", "journal.log", "owners.list"] {
        assert_eq!(mode(&env.paths.fallback.join(f)), 0o600, "{f}");
    }
}

// The real daemon and two real programs.

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
    let log = std::fs::OpenOptions::new().create(true).append(true).open(env.root.join("daemon.err")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_settingsd"))
        .args(["serve", "--no-mcp", "--deadline-ms", "1000", "--stick"])
        .arg(&p.stick)
        .arg("--fallback")
        .arg(&p.fallback)
        .arg("--sys")
        .arg(&p.sys)
        .arg("--socket")
        .arg(&p.socket)
        .stderr(log)
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&p.socket).is_err() {
        assert!(Instant::now() < end, "settingsd did not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child }
}

/// One request from this test binary.
fn me(env: &Env, line: &str) -> Json {
    let mut w = UnixStream::connect(&env.paths.socket).unwrap();
    w.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    writeln!(w, "{line}").unwrap();
    let mut a = String::new();
    BufReader::new(w).read_line(&mut a).unwrap();
    parse(a.trim()).unwrap_or_else(|e| panic!("{e}: {a:?}"))
}

/// One request from the `settings` command of this build: another program.
fn cli(env: &Env, line: &str) -> Json {
    let out = Command::new(env!("CARGO_BIN_EXE_settings"))
        .arg("--socket")
        .arg(&env.paths.socket)
        .args(["--json", "raw", line])
        .output()
        .unwrap();
    let a = String::from_utf8_lossy(&out.stdout).into_owned();
    parse(a.trim()).unwrap_or_else(|e| panic!("{e}: {a:?} {}", String::from_utf8_lossy(&out.stderr)))
}

fn register(owner: &str, key: &str, safety: &str) -> String {
    format!(
        r#"{{"op":"register","owner":"{owner}","keys":[{{"key":"{key}","schema":{{"type":"text","max_bytes":200}},"default":"","text":{{"de":"S","en":"S"}},"safety":"{safety}"}}]}}"#
    )
}

#[test]
fn over_the_socket_the_kernel_says_who_asks() {
    let env = Env::new("peer_socket", true);
    let d = start(&env);
    let ok = Some(&Json::Bool(true));
    let get = r#"{"op":"get","key":"vault.pass","owner":"vault"}"#;
    let set = format!(r#"{{"op":"set","key":"vault.pass","value":"{SECRET}"}}"#);

    assert_eq!(me(&env, &register("vault", "vault.pass", "secret")).get("ok"), ok);
    assert_eq!(me(&env, &set).get("ok"), ok);
    assert_eq!(me(&env, get).get("value"), Some(&s(SECRET)));

    // The other program names the owner, and tries everything else.
    let a = cli(&env, get);
    assert_eq!(a.str("error"), Some("not_owner"), "{}", a.to_text());
    assert!(!a.to_text().contains(SECRET));
    assert_eq!(cli(&env, r#"{"op":"get","key":"vault.pass"}"#).get("value"), Some(&s("***")));
    assert_eq!(cli(&env, &register("vault", "vault.pass", "secret")).str("error"), Some("not_owner"));
    assert_eq!(cli(&env, &register("other", "vault.pass", "normal")).str("error"), Some("not_owner"));
    assert_eq!(cli(&env, r#"{"op":"unregister","owner":"vault","prefix":"vault","forget":true}"#).str("error"), Some("not_owner"));
    assert_eq!(cli(&env, r#"{"op":"set","key":"vault.pass","value":"x"}"#).str("error"), Some("not_owner"));

    // The shared namespace `ai` holds secrets of `assist` alone: not this test binary's, and not those of
    // the `settings` command, though it lies next to settingsd.
    assert_eq!(me(&env, &register("assist", "ai.provider.t.key", "secret")).str("error"), Some("not_owner"));
    assert_eq!(cli(&env, &register("settings", "ai.provider.t.key", "secret")).str("error"), Some("not_owner"));
    assert_eq!(cli(&env, &register("assist", "ai.provider.t.key", "secret")).str("error"), Some("not_owner"));

    // settingsd starts again: before the owner is back, the key and its stored value are nobody else's.
    drop(d);
    let _d = start(&env);
    assert_eq!(cli(&env, &register("vault", "vault.pass", "secret")).str("error"), Some("not_owner"));
    assert_eq!(cli(&env, &register("other", "vault.pass", "normal")).str("error"), Some("not_owner"));
    assert_eq!(me(&env, get).str("error"), Some("unknown_key"));
    assert_eq!(me(&env, &register("vault", "vault.pass", "secret")).get("ok"), ok);
    assert_eq!(me(&env, get).get("value"), Some(&s(SECRET)));
    assert_eq!(cli(&env, get).str("error"), Some("not_owner"));

    let log = std::fs::read_to_string(env.root.join("daemon.err")).unwrap();
    let cli_exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_settings")).unwrap();
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&env.root).unwrap());
    let refused = log.lines().find(|l| l.contains("owner.refused what=get key=vault.pass owner=vault")).expect(&log);
    assert!(refused.contains(&format!("uid={uid} exe={}", cli_exe.display())), "{refused}");
    assert!(log.matches("owner.refused").count() >= 11, "{log}");
    assert!(!log.contains(SECRET), "{log}");
}
