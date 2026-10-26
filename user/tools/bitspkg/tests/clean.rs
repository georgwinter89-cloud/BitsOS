//! PK-2: removal without leftovers. The round trip of clean.rs (also run on V2 by `bitspkg selftest`), and what
//! happens when settingsd is down before or during a removal: refused while nothing changed; after the commit the
//! trash entry waits, an install of the same name is refused, the next command once settingsd is back finishes.

mod common;

use bitspkg::clean::{Ledger, start_settingsd};
use bitspkg::hooks::{Hook, Hooks};
use bitspkg::manifest::Manifest;
use bitspkg::settings::Settings;
use bitspkg::snapshot;
use bitspkg::store::Store;
use bitspkg::{install, remove};
use common::*;
use settingsd::json::Json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// settingsd's bind sets the process umask for a moment (server.rs bind): tests here must not create files
/// on other threads meanwhile.
static ONE: Mutex<()> = Mutex::new(());

fn one() -> std::sync::MutexGuard<'static, ()> {
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn round_trip_leaves_nothing() {
    let _one = one();
    let d = tmp("clean");
    for line in bitspkg::clean::round_trip(&bitspkg::clean::work_in(&d)).unwrap() {
        eprintln!("ok   {line}");
    }
}

/// A hook that fails while `down` is set (settingsd stopped in the middle of a removal).
struct Flaky {
    down: Arc<AtomicBool>,
    inner: Ledger,
}

impl Hook for Flaky {
    fn what(&self) -> &'static str {
        "flaky"
    }
    fn needed(&self, ms: &[Manifest]) -> bool {
        self.inner.needed(ms)
    }
    fn install(&self, m: &Manifest) -> Result<(), String> {
        self.inner.install(m)
    }
    fn remove(&self, name: &str, ms: &[Manifest], keep: bool) -> Result<(), String> {
        if self.down.load(Ordering::Relaxed) {
            return Err("down".into());
        }
        self.inner.remove(name, ms, keep)
    }
}

fn settings_keys(s: &Settings) -> String {
    let a = s.request(&Json::obj().with("op", "list").with("prefix", "app")).unwrap().unwrap();
    a.get("keys").map(Json::to_text).unwrap_or_default()
}

#[test]
fn settingsd_down_before_the_removal_refuses_it() {
    let _one = one();
    let d = tmp("clean-down");
    let st = Store::new(&d.join("root"));
    let mut st = st;
    st.hooks = Hooks::standard(Some(d.join("nobody.sock")));
    let p = package(&d, "1.0");
    let r = install::install(&st, &p).unwrap();
    assert_eq!(r.hooks_failed, 1, "the settings registration fails and is counted");
    let before = snapshot::take(&d.join("root")).unwrap();
    let Err(e) = remove::remove(&st, "demo", false) else { panic!("refused") };
    assert!(e.contains("settingsd not reachable"), "{e}");
    assert!(snapshot::diff(&before, &snapshot::take(&d.join("root")).unwrap(), &[]).is_empty(), "nothing changed");
    assert_eq!(current_program(&d.join("root")).as_deref(), Some("echo demo 1.0"));
}

#[test]
fn a_hook_failing_after_the_commit_is_finished_later() {
    let _one = one();
    let d = tmp("clean-pending");
    let root = d.join("root");
    let (sd, sock) = (d.join("settings"), d.join("run/s.sock"));
    start_settingsd(&sd, &sock).unwrap();
    let down = Arc::new(AtomicBool::new(false));
    let table = Arc::new(Mutex::new(BTreeMap::new()));
    let mut st = Store::new(&root);
    st.hooks = Hooks::standard(Some(sock.clone()));
    st.hooks.list.truncate(1);
    st.hooks.list.push(Box::new(Flaky {
        down: down.clone(),
        inner: Ledger { what: "rules", table: table.clone(), of: |m| m.permissions.iter().map(|p| p.describe()).collect() },
    }));
    let s = Settings { socket: sock.clone() };
    let p = package(&d, "1.0");
    st.lock().unwrap();
    let seed = Json::obj().with("op", "set").with("key", "theme.accent").with("value", "#123456").with("source", "test");
    s.request(&seed).unwrap().unwrap();
    let before = snapshot::take(&d).unwrap();
    let keys_before = settings_keys(&s);
    install::install(&st, &p).unwrap();
    assert!(settings_keys(&s).contains("app.demo.homepage"));
    let set = Json::obj().with("op", "set").with("key", "app.demo.homepage").with("value", "https://example.org").with("source", "test");
    s.request(&set).unwrap().unwrap();
    std::fs::write(root.join("data/appdata/demo/profile"), "x").unwrap();
    down.store(true, Ordering::Relaxed);
    let Err(e) = remove::remove(&st, "demo", false) else { panic!("not finished") };
    assert!(e.contains("is removed, but"), "{e}");
    assert!(!root.join("apps/demo").exists() && !root.join("data/appdata/demo").exists());
    assert!(st.pending_removal("demo").unwrap());
    assert!(!table.lock().unwrap().is_empty(), "the rule is still there");
    let Err(e) = install::install(&st, &p) else { panic!("install waits for the removal") };
    assert!(e.contains("not finished"), "{e}");
    down.store(false, Ordering::Relaxed);
    remove::gc(&st).unwrap();
    assert!(!st.pending_removal("demo").unwrap());
    assert!(table.lock().unwrap().is_empty(), "the rule is gone");
    assert_eq!(settings_keys(&s), keys_before);
    let left = snapshot::diff(&before, &snapshot::take(&d).unwrap(), &["root/apps/.next-id"]);
    assert!(left.is_empty(), "leftovers: {left:#?}");
}

#[test]
fn keep_data_keeps_the_values_for_a_reinstall() {
    let _one = one();
    let d = tmp("clean-keep");
    let (sd, sock) = (d.join("settings"), d.join("run/s.sock"));
    start_settingsd(&sd, &sock).unwrap();
    let mut st = Store::new(&d.join("root"));
    st.hooks = Hooks::standard(Some(sock.clone()));
    let s = Settings { socket: sock };
    let p = package(&d, "1.0");
    install::install(&st, &p).unwrap();
    let set = Json::obj().with("op", "set").with("key", "app.demo.homepage").with("value", "https://example.org").with("source", "test");
    s.request(&set).unwrap().unwrap();
    remove::remove(&st, "demo", true).unwrap();
    assert!(!settings_keys(&s).contains("app.demo.homepage"), "unregistered");
    install::install(&st, &p).unwrap();
    let get = s.request(&Json::obj().with("op", "get").with("key", "app.demo.homepage")).unwrap().unwrap();
    assert_eq!(get.str("value"), Some("https://example.org"), "{}", get.to_text());
    remove::remove(&st, "demo", false).unwrap();
    assert!(!settings_keys(&s).contains("app.demo"));
}

fn run_with(root: &std::path::Path, sock: &std::path::Path, args: &[&str], cut: Option<u64>) -> (i32, String) {
    let mut c = std::process::Command::new(bin());
    c.arg("--root").arg(root).args(args).env("BITSPKG_SETTINGS", sock);
    match cut {
        Some(n) => c.env(bitspkg::cut::ENV, n.to_string()),
        None => c.env_remove(bitspkg::cut::ENV),
    };
    let o = c.output().unwrap();
    (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

/// A cut before each step of a removal of a package with settings (two versions, a value set): once the next
/// command has recovered, the files, settingsd's files and its keys are those before the install. And a cut
/// before each step of an install: finishing it leaves the key registered.
#[test]
fn cuts_during_install_and_removal_leave_no_settings_behind() {
    let _one = one();
    let d = tmp("clean-cut");
    let work = d.join("pkgs");
    std::fs::create_dir_all(&work).unwrap();
    let (p1, p2) = (package(&work, "1.0"), package(&work, "2.0"));
    let (p1, p2) = (p1.to_str().unwrap().to_string(), p2.to_str().unwrap().to_string());
    for (what, n_max) in [("remove", 200u64), ("install", 200)] {
        let mut n = 1;
        loop {
            let at = d.join(format!("{what}-{n}"));
            let (root, sock) = (at.join("root"), d.join(format!("run/{what}-{n}.sock")));
            start_settingsd(&at.join("settings"), &sock).unwrap();
            let s = Settings { socket: sock.clone() };
            let seed = Json::obj().with("op", "set").with("key", "theme.accent").with("value", "#123456").with("source", "test");
            s.request(&seed).unwrap().unwrap();
            assert_eq!(run_with(&root, &sock, &["gc"], None).0, 0);
            let before = snapshot::take(&at).unwrap();
            let keys_before = settings_keys(&s);
            let cmd: Vec<&str> = if what == "remove" {
                for p in [&p1, &p2] {
                    let (c, out) = run_with(&root, &sock, &["install", p], None);
                    assert_eq!(c, 0, "{out}");
                }
                let set = Json::obj().with("op", "set").with("key", "app.demo.homepage").with("value", "https://example.org").with("source", "test");
                s.request(&set).unwrap().unwrap();
                vec!["remove", "demo"]
            } else {
                vec!["install", &p1]
            };
            let (code, out) = run_with(&root, &sock, &cmd, Some(n));
            let (c, o) = run_with(&root, &sock, &["gc"], None);
            assert_eq!(c, 0, "{what} cut {n}: gc: {o}");
            let (c2, o2) = run_with(&root, &sock, &cmd, None);
            if what == "remove" {
                assert!(c2 == 0 || o2.contains("not installed"), "{what} cut {n}: rerun: {o2}");
                let left = snapshot::diff(&before, &snapshot::take(&at).unwrap(), &["root/apps/.next-id"]);
                assert!(left.is_empty(), "remove cut {n}: leftovers: {left:#?}");
                assert_eq!(settings_keys(&s), keys_before, "remove cut {n}");
            } else {
                assert_eq!(c2, 0, "install cut {n}: rerun: {o2}");
                assert!(settings_keys(&s).contains("app.demo.homepage"), "install cut {n}: key not registered");
            }
            if code == 0 {
                eprintln!("{what}: cut at each of {} steps", n - 1);
                break;
            }
            assert_eq!(code, bitspkg::cut::EXIT, "{what} step {n}: not a cut: {out}");
            make_writable(&at);
            let _ = std::fs::remove_dir_all(&at);
            n += 1;
            assert!(n < n_max, "{what}: no end");
        }
    }
}

/// `demo` at `version` whose one key is `app.demo.<key>`.
fn package_with_key(d: &std::path::Path, version: &str, key: &str) -> std::path::PathBuf {
    let t = tree(d, version);
    let spec = std::fs::read_to_string(t.join("package.toml")).unwrap().replace("app.demo.homepage", &format!("app.demo.{key}"));
    std::fs::write(t.join("package.toml"), spec).unwrap();
    let out = d.join(format!("pkg-{version}"));
    bitspkg::build::build(&t, None, Some(&out)).unwrap();
    out
}

fn value(s: &Settings, key: &str) -> Option<String> {
    let a = s.request(&Json::obj().with("op", "get").with("key", key)).unwrap().ok()?;
    a.str("value").map(str::to_string)
}

/// Switch and rollback register the keys of the version that became current and unregister the others (their
/// values kept while a version on disk defines them); an upgrade whose gc drops the only version that defined a key
/// forgets that key's value, so a later reboot and reinstall does not find it again.
#[test]
fn switch_and_rollback_follow_the_current_version_keys() {
    let _one = one();
    let d = tmp("clean-switch");
    let (sd, sock) = (d.join("settings"), d.join("run/s.sock"));
    start_settingsd(&sd, &sock).unwrap();
    let mut st = Store::new(&d.join("root"));
    st.hooks = Hooks::standard(Some(sock.clone()));
    let s = Settings { socket: sock };
    let (p1, p2, p3) = (package_with_key(&d, "1.0", "homepage"), package_with_key(&d, "2.0", "start"), package_with_key(&d, "3.0", "third"));
    install::install(&st, &p1).unwrap();
    let set = Json::obj().with("op", "set").with("key", "app.demo.homepage").with("value", "https://h1.example").with("source", "test");
    s.request(&set).unwrap().unwrap();
    install::install(&st, &p2).unwrap();
    let k = settings_keys(&s);
    assert!(k.contains("app.demo.start") && !k.contains("app.demo.homepage"), "after the upgrade: {k}");
    assert_eq!(install::rollback(&st, "demo").unwrap().2, 0);
    let k = settings_keys(&s);
    assert!(k.contains("app.demo.homepage") && !k.contains("app.demo.start"), "after the rollback: {k}");
    assert_eq!(value(&s, "app.demo.homepage").as_deref(), Some("https://h1.example"), "the value came back");
    assert_eq!(install::switch(&st, "demo", "2.0").unwrap().1, 0);
    let k = settings_keys(&s);
    assert!(k.contains("app.demo.start") && !k.contains("app.demo.homepage"), "after the switch: {k}");
    // 3.0 keeps 2.0 as previous; 1.0, the only version with `homepage`, goes and its value with it.
    install::install(&st, &p3).unwrap();
    assert!(!st.version_dir("demo", "1.0").exists());
    let k = settings_keys(&s);
    assert!(k.contains("app.demo.third") && !k.contains("app.demo.start") && !k.contains("app.demo.homepage"), "after 3.0: {k}");
    let sock2 = d.join("run/s2.sock");
    start_settingsd(&sd, &sock2).unwrap();
    st.hooks = Hooks::standard(Some(sock2.clone()));
    let s2 = Settings { socket: sock2 };
    remove::remove(&st, "demo", false).unwrap();
    install::install(&st, &p1).unwrap();
    assert_eq!(value(&s2, "app.demo.homepage").as_deref(), Some("about:blank"), "forgotten with 1.0");
    remove::remove(&st, "demo", false).unwrap();
}

/// Pending removals while settingsd hangs (accepts, never answers): one entry pays the timeout, the others wait for
/// a later command, and the command itself goes on (recover never fails it for one entry).
#[test]
fn recover_with_settingsd_hanging_is_bounded() {
    let _one = one();
    let d = tmp("clean-hang");
    let (sd, sock) = (d.join("settings"), d.join("run/s.sock"));
    start_settingsd(&sd, &sock).unwrap();
    let mut st = Store::new(&d.join("root"));
    st.hooks = Hooks::standard(Some(sock.clone()));
    install::install(&st, &package(&d, "1.0")).unwrap();
    // A second package with keys of its own: `demo` built under another name.
    let t = tree(&d.join("b"), "1.0");
    let spec = std::fs::read_to_string(t.join("package.toml")).unwrap().replace("name = \"demo\"", "name = \"demob\"").replace("app.demo.", "app.demob.");
    std::fs::write(t.join("package.toml"), spec).unwrap();
    bitspkg::build::build(&t, None, Some(&d.join("pkg-b"))).unwrap();
    install::install(&st, &d.join("pkg-b")).unwrap();
    // A socket that accepts and never answers.
    let hang = d.join("run/hang.sock");
    let l = std::os::unix::net::UnixListener::bind(&hang).unwrap();
    std::thread::spawn(move || {
        let mut keep = Vec::new();
        for c in l.incoming().flatten() {
            keep.push(c);
        }
    });
    st.hooks = Hooks::standard(Some(hang));
    // `ready` already times out once: the removals are refused before anything changes.
    let t0 = std::time::Instant::now();
    assert!(remove::remove(&st, "demo", false).is_err());
    assert!(t0.elapsed() < std::time::Duration::from_secs(25), "{:?}", t0.elapsed());
    // Removals that committed while settingsd was up but whose hooks hang now: made by a hook that fails once.
    st.hooks = Hooks::standard(Some(sock.clone()));
    let down = Arc::new(AtomicBool::new(true));
    st.hooks.list.push(Box::new(Flaky { down: down.clone(), inner: Ledger { what: "rules", table: Arc::new(Mutex::new(BTreeMap::new())), of: |_| Vec::new() } }));
    assert!(remove::remove(&st, "demo", false).is_err());
    assert!(remove::remove(&st, "demob", false).is_err());
    assert!(st.pending_removal("demo").unwrap() && st.pending_removal("demob").unwrap());
    let hang = d.join("run/hang.sock");
    st.hooks = Hooks::standard(Some(hang));
    let t0 = std::time::Instant::now();
    assert!(install::register(&st, None).is_ok(), "recover does not fail the command");
    // One 10 s answer timeout (settings.rs TIMEOUT), not one per pending entry (two here: 20 s).
    assert!(t0.elapsed() < std::time::Duration::from_secs(15), "one entry's timeout, not one per entry: {:?}", t0.elapsed());
    assert!(st.pending_removal("demo").unwrap() && st.pending_removal("demob").unwrap());
    st.hooks = Hooks::standard(Some(sock));
    remove::gc(&st).unwrap();
    assert!(!st.pending_removal("demo").unwrap() && !st.pending_removal("demob").unwrap());
}

#[test]
fn a_socket_path_too_long_for_sun_path_is_named() {
    let s = Settings { socket: std::path::PathBuf::from(format!("/tmp/{}/s.sock", "x".repeat(120))) };
    let e = s.request(&Json::obj().with("op", "status")).unwrap_err();
    assert!(e.contains("longer than a Unix socket address holds"), "{e}");
}
