//! The switch desktop.durchblick.programs with a stand-in settingsd: atspid registers the key when it is missing,
//! stays without a bus while it is off (C3 says so in German), starts the bus when it goes on and ends when it goes
//! off again.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "{what} did not happen within {secs} s");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn off_waits_on_runs_off_ends() {
    let dir = PathBuf::from(format!("/tmp/us-sw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (bus, sock, set) = (dir.join("a11y/bus"), dir.join("c3.sock"), dir.join("s.sock"));
    let on = Arc::new(AtomicBool::new(false));
    let registered = Arc::new(AtomicUsize::new(0));
    let l = UnixListener::bind(&set).unwrap();
    let (on2, reg2) = (on.clone(), registered.clone());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let answer = match req["op"].as_str() {
                Some("register") => {
                    assert_eq!(req["owner"], "atspid");
                    reg2.fetch_add(1, Ordering::SeqCst);
                    json!({"ok": true, "registered": 1})
                }
                Some("get") if reg2.load(Ordering::SeqCst) == 0 => json!({"ok": false, "error": "unknown_key"}),
                Some("get") => json!({"ok": true, "key": req["key"], "value": on2.load(Ordering::SeqCst)}),
                _ => json!({"ok": false}),
            };
            let _ = writeln!(&s, "{answer}");
        }
    });
    let mut serve = Command::new(env!("CARGO_BIN_EXE_atspid"))
        .args(["serve", "--look", "1", "--bus"])
        .arg(&bus)
        .arg("--socket")
        .arg(&sock)
        .arg("--settings")
        .arg(&set)
        .arg("--peer")
        .arg(std::env::current_exe().unwrap())
        .spawn()
        .unwrap();
    let ask = |op: &str, args: Value| atspid::client::ask(&sock, op, args).unwrap_or(Value::Null);
    wait_for("the C3 socket", 10, || sock.exists());
    wait_for("the key registered", 10, || registered.load(Ordering::SeqCst) == 1);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(ask("ping", Value::Null)["running"], false, "off: no bus");
    assert!(!bus.exists());
    let off = ask("tree", json!({"pid": 1}));
    assert!(off["error"].as_str().unwrap_or("").contains("ausgeschaltet"), "{off}");
    on.store(true, Ordering::SeqCst);
    wait_for("the bus", 10, || ask("ping", Value::Null)["running"] == true);
    assert!(bus.exists());
    on.store(false, Ordering::SeqCst);
    wait_for("atspid ending", 10, || serve.try_wait().unwrap().is_some());
    assert!(serve.wait().unwrap().success(), "switched off is a clean end");
    assert!(!bus.exists() && !sock.exists(), "its sockets are gone");
    let _ = std::fs::remove_dir_all(&dir);
}
