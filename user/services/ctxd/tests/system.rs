//! CX-5: `ctxd serve --system` against a stand-in settingsd (the real one runs in the QEMU scenario
//! host/tests/cx/20-ctxd-v2.sh). Nothing is indexed until a folder is named; naming one restarts the service
//! in the same process and it indexes; a private folder never reaches the AI's door (`op mcp`); the data
//! folder is 0700 and the socket 0600; without the model the search says it ran on keywords only; with no
//! folder named an AI is offered no tools at all.

mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Values = Arc<Mutex<BTreeMap<String, Value>>>;

/// Answers `list` with what was registered (and set) and `register` with ok, as settingsd does.
fn fake_settingsd(path: &Path) -> Values {
    let values: Values = Arc::default();
    let l = UnixListener::bind(path).unwrap();
    let v = Arc::clone(&values);
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut line = String::new();
            if BufReader::new(&s).read_line(&mut line).is_err() {
                continue;
            }
            let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let answer = match req["op"].as_str() {
                Some("register") => {
                    let mut m = v.lock().unwrap();
                    for k in req["keys"].as_array().unwrap() {
                        m.entry(k["key"].as_str().unwrap().to_string()).or_insert_with(|| k["default"].clone());
                    }
                    json!({"ok": true, "registered": req["keys"].as_array().unwrap().len()})
                }
                Some("list") => {
                    let m = v.lock().unwrap();
                    let keys: Vec<Value> = m.iter().map(|(k, val)| json!({"key": k, "value": val})).collect();
                    json!({"ok": true, "keys": keys})
                }
                _ => json!({"ok": false, "error": "bad_request"}),
            };
            let _ = writeln!(s, "{answer}");
        }
    });
    values
}

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out: {what}");
}

fn status(sock: &Path) -> Value {
    sock_call(sock, "context_status", json!({})).map(|v| v["result"].clone()).unwrap_or(Value::Null)
}

#[test]
fn the_v2_service_follows_its_settings() {
    let d = tmp("system");
    let run = d.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let docs = d.join("Dokumente");
    let private = d.join("Privat");
    copy_dir(&eval_docs(), &docs);
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("geheim.txt"), "Geheimer Brief an den Steuerberater: Steuererklärung 2025 mit allen Konten").unwrap();
    let values = fake_settingsd(&d.join("settings.sock"));
    let data = d.join("appdata/ctxd");
    std::fs::create_dir_all(d.join("appdata")).unwrap();
    let mut child = Kill(Command::new(bin())
        .args(["serve", "--system"])
        .env("CTXD_TEST_DATA", &data)
        .env("CTXD_TEST_SETTINGS_SOCKET", d.join("settings.sock"))
        .env("XDG_RUNTIME_DIR", &run)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap());
    let sock = run.join("ctxd.sock");

    // Registered, no folder: a generation of nothing.
    wait_for("the first generation", 60, || status(&sock)["generation"].is_u64());
    assert!(values.lock().unwrap().contains_key("context.roots"), "the keys were registered");
    assert_eq!(status(&sock)["docs"], json!(0), "nothing named, nothing read: {}", status(&sock));
    assert_eq!(std::fs::metadata(&data).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777, 0o600);
    let empty = sock_call(&sock, "mcp", json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).unwrap();
    assert_eq!(empty["result"]["result"]["tools"], json!([]), "no folder, no tools for an AI: {empty}");

    // Georg names two folders, one of them private: the service starts anew (same pid) and reads both.
    let pid = child.0.id();
    {
        let mut m = values.lock().unwrap();
        m.insert("context.roots".into(), json!(docs.display().to_string()));
        m.insert("context.private_roots".into(), json!(private.display().to_string()));
    }
    wait_for("both folders indexed", 120, || status(&sock)["docs"].as_u64() == Some(152));
    assert!(child.0.try_wait().unwrap().is_none(), "restarted in place, pid {pid} still runs");
    let events = std::fs::read_to_string(data.join("log/events.jsonl")).unwrap();
    assert!(events.contains("\"service.restart\"") && events.contains("\"embedder.model_missing\""), "{events}");

    // The desktop's search sees both, the AI's door never the private folder.
    let desk = sock_call(&sock, "context_search", json!({"query": "Steuererklärung 2025", "limit": 20})).unwrap();
    let paths: Vec<String> = desk["result"]["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap().to_string()).collect();
    assert!(paths.iter().any(|p| p.contains("geheim.txt")), "the desktop sees the private folder: {paths:?}");
    assert_eq!(desk["result"]["partial"], json!(true), "no model: keywords only, and the answer says so");
    for (name, args) in [
        ("context_search", json!({"query": "Steuererklärung 2025 Steuerberater", "limit": 500})),
        ("workspace_open", json!({"query": "Steuererklärung 2025"})),
        ("context_status", json!({})),
    ] {
        let rpc = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": name, "arguments": args}});
        let a = sock_call(&sock, "mcp", rpc).unwrap();
        let text = a.to_string();
        assert_eq!(a["ok"], json!(true), "{text}");
        assert!(!text.contains("geheim") && !text.contains("Geheimer") && !text.contains(&private.display().to_string()), "{name}: {text}");
    }
    let list = sock_call(&sock, "mcp", json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).unwrap();
    assert!(list["result"]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "context_search"));

    // Switched off: nothing is read any more.
    values.lock().unwrap().insert("context.enabled".into(), json!(false));
    wait_for("the index emptied", 60, || status(&sock)["docs"].as_u64() == Some(0));
    child.0.kill().unwrap();
}

/// The service ends with the test, also when an assertion fails.
struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Checker 12.11.: `--system` reaches the running service only through its socket. A command that would open
/// the index itself (index, eval, replay, verify, extract) is refused and leaves the service's folder alone.
#[test]
fn system_commands_that_would_open_the_index_are_refused() {
    let d = tmp("system-cli");
    let data = d.join("appdata/ctxd");
    for cmd in ["index", "eval", "replay", "verify", "extract"] {
        let out = Command::new(bin()).args(["--system", cmd, "x"]).env("CTXD_TEST_DATA", &data).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("geht nicht mit --system"), "{cmd}: {err}");
        assert!(!d.join("appdata").exists(), "{cmd} made the service's folder");
    }
}
