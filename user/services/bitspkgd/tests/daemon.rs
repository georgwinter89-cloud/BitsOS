//! bitspkgd on the host: the socket's ops on a scratch root (no mirror: R9), the peer rule (this test runs as a
//! user that is not root and not the compositor: it may only read), the keys registered with settingsd, and a
//! reader answering while a changing run holds the service.

use bitspkg::clean::{SockDir, start_settingsd};
use settingsd::json::{self, Json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("bitspkgd").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(root: &Path, sock: &Path, settings: &str, sources: &Path) -> Server {
    let c = Command::new(env!("CARGO_BIN_EXE_bitspkgd"))
        .args(["serve", "--stderr", "--no-mcp", "--no-timer", "--root"])
        .arg(root)
        .arg("--socket")
        .arg(sock)
        .args(["--settings", settings, "--sources"])
        .arg(sources)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < end, "bitspkgd did not listen");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server(c)
}

fn ask(sock: &Path, req: &str) -> (Vec<Json>, bool) {
    let mut lines = Vec::new();
    let ok = bitspkgd::client::call(sock, req, Duration::from_secs(20), &mut |l| lines.push(json::parse(l).unwrap())).unwrap();
    (lines, ok)
}

fn last(sock: &Path, req: &str) -> Json {
    ask(sock, req).0.pop().unwrap()
}

#[test]
fn ops_peer_rule_and_keys() {
    let d = tmp("ops");
    let sd = SockDir::new().unwrap();
    let ssock = sd.join("s.sock");
    start_settingsd(&d.join("settings"), &ssock).unwrap();
    let src = d.join("src.toml");
    std::fs::write(&src, "[[source]]\nname = \"fixture\"\nservers = [\"http://127.0.0.1:9/$repo\"]\nsig_db = \"optional\"\nkeyring = \"archlinux\"\n").unwrap();
    let sock = sd.join("pkg.sock");
    let root = d.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let _srv = start(&root, &sock, ssock.to_str().unwrap(), &src);

    let l = last(&sock, r#"{"op":"list"}"#);
    assert_eq!(l.get("ok").and_then(Json::as_bool), Some(true), "{l:?}");
    assert_eq!(l.get("programs"), Some(&Json::Arr(vec![])));
    let s = last(&sock, r#"{"op":"sources"}"#);
    assert_eq!(s.get("sources").and_then(|v| if let Json::Arr(a) = v { a.first()?.str("name") } else { None }), Some("fixture"), "{s:?}");
    // Nothing loaded: search and plan say so in German, nothing was fetched.
    let q = last(&sock, r#"{"op":"search","query":"jq"}"#);
    assert_eq!(q.str("error"), Some("refused"));
    assert!(q.str("message").unwrap().contains("keine Quelle geladen"), "{q:?}");
    let p = last(&sock, r#"{"op":"plan","action":"install","name":"jq"}"#);
    assert_eq!(p.str("error"), Some("refused"), "{p:?}");
    let r = last(&sock, r#"{"op":"plan","action":"remove","name":"jq"}"#);
    assert_eq!((r.get("installable").and_then(Json::as_bool), r.str("summary")), (Some(false), Some("jq: nicht installiert")), "{r:?}");
    // This test is neither root nor the compositor: the changing ops are refused, the reading ones answer.
    for op in [r#"{"op":"do","plan_id":"p1-1"}"#, r#"{"op":"switch","name":"jq","version":"1"}"#, r#"{"op":"rollback","name":"jq"}"#, r#"{"op":"refresh"}"#] {
        let e = last(&sock, op);
        if running_as_root() {
            // Root may change: each op gets past the peer rule to its own answer (no plan, nothing installed, the
            // mirror at port 9 refuses).
            let want: &[&str] = if op.contains("\"do\"") { &["unknown_plan"] } else if op.contains("refresh") { &["refused", ""] } else { &["refused"] };
            assert!(want.contains(&e.str("error").unwrap_or("")), "root may change: {op} {e:?}");
        } else {
            assert_eq!(e.str("error"), Some("read_only"), "{op}: {e:?}");
            assert!(e.str("message").unwrap().contains("Compositor"), "{e:?}");
        }
    }
    for (bad, code) in [("[1]", "bad_request"), ("nicht json", "bad_request"), (r#"{"op":"frob"}"#, "bad_request"), (r#"{"op":"plan","action":"purge","name":"jq"}"#, "bad_request")] {
        assert_eq!(last(&sock, bad).str("error"), Some(code), "{bad}");
    }
    let long = format!(r#"{{"op":"search","query":"{}"}}"#, "x".repeat(70 * 1024));
    assert_eq!(last(&sock, &long).str("error"), Some("bad_request"));
    let st = last(&sock, r#"{"op":"status"}"#);
    assert_eq!(st.get("busy"), Some(&Json::Null), "{st:?}");
    assert_eq!(st.get("features"), Some(&Json::Arr(vec![Json::Str("keep_data".into())])), "{st:?}");
    // keep_data (CR-PK-7a-1): a removal only, a boolean only.
    for bad in [r#"{"op":"plan","action":"remove","name":"jq","keep_data":"ja"}"#, r#"{"op":"plan","action":"install","name":"jq","keep_data":true}"#] {
        assert_eq!(last(&sock, bad).str("error"), Some("bad_request"), "{bad}");
    }
    let r = last(&sock, r#"{"op":"plan","action":"remove","name":"jq","keep_data":true}"#);
    assert_eq!(r.str("summary"), Some("jq: nicht installiert"), "{r:?}");
    // The keys of plan 1.10 are registered by the service (owner bitspkg).
    let s = bitspkg::settings::Settings { socket: ssock.clone() };
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let got = s.request(&Json::obj().with("op", "get").with("key", "pkg.optional.ask")).unwrap();
        if let Ok(j) = got {
            assert_eq!(j.get("value").and_then(Json::as_bool), Some(true));
            break;
        }
        assert!(Instant::now() < end, "pkg.optional.ask not registered");
        std::thread::sleep(Duration::from_millis(200));
    }
    for k in ["pkg.updates.check", "pkg.updates.install", "pkg.space.max_gb", "pkg.cpu_level", "pkg.sources.order"] {
        assert!(s.request(&Json::obj().with("op", "get").with("key", k)).unwrap().is_ok(), "{k}");
    }
}

/// Root runs the tests in some setups: the peer rule then lets it change (checked the other way round).
fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("Uid:").map(|v| v.split_whitespace().next() == Some("0")))).unwrap_or(false)
}

#[test]
fn a_reader_answers_while_a_run_holds_the_service() {
    let d = tmp("busy");
    let root = d.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let src = d.join("src.toml");
    std::fs::write(&src, "[[source]]\nname = \"fixture\"\nservers = [\"http://127.0.0.1:9/$repo\"]\n").unwrap();
    let cx = std::sync::Arc::new(bitspkgd::Ctx::new(root, Some(None), Some(src)));
    let c = cx.clone();
    let slow = std::thread::spawn(move || {
        bitspkgd::work::run(&c, "slow mirror", Duration::from_secs(30), Duration::from_secs(1), |_| std::thread::sleep(Duration::from_secs(3)), &mut |_| {})
    });
    let end = Instant::now() + Duration::from_secs(2);
    while cx.work.busy().is_none() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    let t = Instant::now();
    let mut got = Vec::new();
    bitspkgd::ops::handle(&cx, &|| bitspkgd::peer::Right::Read("test".into()), r#"{"op":"list"}"#, &mut |j| {
        got.push(j.clone());
        true
    });
    assert!(t.elapsed() < Duration::from_secs(1), "list waited for the run: {:?}", t.elapsed());
    assert_eq!(got.last().and_then(|j| j.get("ok")).and_then(Json::as_bool), Some(true));
    // The right is judged for each changing request: a peer that may change at first and lost its grant since is
    // refused at its next one (the compositor that gave fb0 away).
    let changes = std::sync::atomic::AtomicBool::new(true);
    let peer = || {
        if changes.swap(false, std::sync::atomic::Ordering::AcqRel) { bitspkgd::peer::Right::Change("bitscomp".into()) } else { bitspkgd::peer::Right::Read("Freigabe abgegeben".into()) }
    };
    let mut first = Vec::new();
    bitspkgd::ops::handle(&cx, &peer, r#"{"op":"do","plan_id":"p0-0"}"#, &mut |j| {
        first.push(j.clone());
        true
    });
    assert_eq!(first.last().and_then(|j| j.str("error")), Some("unknown_plan"), "{first:?}");
    let mut second = Vec::new();
    bitspkgd::ops::handle(&cx, &peer, r#"{"op":"do","plan_id":"p0-0"}"#, &mut |j| {
        second.push(j.clone());
        true
    });
    assert_eq!(second.last().and_then(|j| j.str("error")), Some("read_only"), "{second:?}");
    // A second run waits at most its wait, then says who is working.
    let e = bitspkgd::work::run(&cx, "second", Duration::from_secs(5), Duration::from_millis(300), |_| (), &mut |_| {}).unwrap_err();
    assert!(e.contains("arbeitet gerade an: slow mirror"), "{e}");
    assert!(slow.join().unwrap().is_ok());
    assert_eq!(cx.work.busy(), None);
    // Progress items reach the run's client in order.
    let mut seen = Vec::new();
    let r = bitspkgd::work::run(&cx, "phases", Duration::from_secs(5), Duration::from_secs(1), |_| {
        for p in bitspkg::progress::PHASES {
            bitspkg::cut::step(p);
        }
        7
    }, &mut |i| seen.push(i));
    assert_eq!(r, Ok(7));
    let phases: Vec<String> = seen.iter().map(|i| match i { bitspkg::progress::Item::Phase(p) => p.clone(), _ => String::new() }).collect();
    assert_eq!(phases, bitspkg::progress::PHASES.map(String::from));
}
