//! brave-act end to end on the host: the real binary against a fake kernel registry and a fake settingsd (Unix
//! sockets), the staged Brave tree headless (tests/host-launcher.sh; no window), the AC-3 test site
//! (host/tests/ac/site) from a small HTTP server on 127.0.0.1 by the name ac3.test (every other name NOTFOUND: no
//! internet). Every action once; the change actions refused without permission and with an allow set over MCP,
//! then allowed (Georg's shell); file: URLs refused; a hanging page against timeout_ms; a page larger than an
//! answer; Brave killed and started again by the next call. Needs the stage (bash
//! user/ports/brave-launcher/scripts/stage-v2.sh): without it the test fails.

use mcpreg::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn site() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../host/tests/ac/site")
}

/// HTTP/1.0 GET of the site's files (`?query` ignored); .bin as application/octet-stream as BusyBox httpd sends it.
fn http(l: TcpListener) {
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = s;
                let mut req = Vec::new();
                let mut b = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&b[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&req).lines().next().unwrap_or("").to_string();
                let path = line.split(' ').nth(1).unwrap_or("/").split('?').next().unwrap_or("/").trim_start_matches('/').to_string();
                let path = if path.is_empty() { "index.html".to_string() } else { path };
                if path == "hang" {
                    // Never answers: the call's timeout_ms must end the wait.
                    std::thread::sleep(Duration::from_secs(600));
                    return;
                }
                let made = (path == "big.html").then(|| format!("<html><head><title>AC3 big</title></head><body><p>{}</p></body></html>", "語".repeat(1_800_000)).into_bytes());
                let (code, ty, body) = match made.map(Ok).unwrap_or_else(|| std::fs::read(site().join(&path))) {
                    Ok(b) if !path.contains("..") => ("200 OK", if path.ends_with(".html") { "text/html; charset=utf-8" } else { "application/octet-stream" }, b),
                    _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
                };
                let _ = s.write_all(format!("HTTP/1.0 {code}\r\nContent-Type: {ty}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes());
                let _ = s.write_all(&body);
            });
        }
    });
}

/// `src`: the journal's source of every key's newest change (`history`).
fn settingsd(path: PathBuf, values: Arc<Mutex<Vec<(String, String)>>>, src: Arc<Mutex<String>>) {
    let l = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut w = s.try_clone().unwrap();
            let mut line = String::new();
            if BufReader::new(s).read_line(&mut line).is_err() {
                continue;
            }
            let j = Json::parse(line.trim()).unwrap();
            let mut v = values.lock().unwrap();
            let a = match j.get("op").and_then(Json::as_str) {
                Some("register") => {
                    for k in j.get("keys").and_then(Json::as_array).unwrap_or(&[]) {
                        let key = k.get("key").and_then(Json::as_str).unwrap().to_string();
                        if !v.iter().any(|(n, _)| *n == key) {
                            v.push((key, "deny".into()));
                        }
                    }
                    r#"{"ok":true}"#.to_string()
                }
                Some("get") => {
                    let key = j.get("key").and_then(Json::as_str).unwrap_or("");
                    match v.iter().find(|(n, _)| n == key) {
                        Some((_, val)) => format!(r#"{{"ok":true,"value":"{val}"}}"#),
                        None => r#"{"ok":false,"error":"unknown_key","message":"?"}"#.into(),
                    }
                }
                Some("history") => {
                    let key = j.get("prefix").and_then(Json::as_str).unwrap_or("");
                    format!(r#"{{"ok":true,"entries":[{{"key":"{key}","source":"{}","op":"set"}}]}}"#, src.lock().unwrap())
                }
                _ => r#"{"ok":false,"error":"bad_request","message":"?"}"#.into(),
            };
            let _ = w.write_all(format!("{a}\n").as_bytes());
        }
    });
}

struct Kernel {
    w: UnixStream,
    r: BufReader<UnixStream>,
    id: u64,
}

struct Answer {
    err: bool,
    text: String,
    png: Option<Vec<u8>>,
}

impl Kernel {
    fn call(&mut self, name: &str, args: &str, timeout_ms: u64) -> Answer {
        self.id += 1;
        let id = self.id;
        let m = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args},"_meta":{{"progressToken":{id},"timeout_ms":{timeout_ms}}}}}}}"#);
        self.w.write_all(format!("{m}\n").as_bytes()).unwrap();
        loop {
            let mut l = String::new();
            assert!(self.r.read_line(&mut l).unwrap() > 0, "brave-act closed its connection");
            let j = Json::parse(l.trim()).unwrap();
            if j.get("method").is_some() || j.get("id").and_then(Json::as_u64) != Some(id) {
                continue;
            }
            if let Some(e) = j.get("error") {
                return Answer { err: true, text: format!("rpc {}", e.get("message").and_then(Json::as_str).unwrap_or("")), png: None };
            }
            let r = j.get("result").unwrap();
            let c = r.get("content").and_then(Json::as_array).unwrap_or(&[]);
            let text = c.iter().filter_map(|i| i.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n");
            let png = c.iter().find(|i| i.get("type").and_then(Json::as_str) == Some("image")).and_then(|i| i.get("data")).and_then(Json::as_str).map(|d| bitsact::base64::decode(d).unwrap());
            let a = Answer { err: r.get("isError").and_then(Json::as_bool) == Some(true), text, png };
            println!("{name} {args} -> err={} {}", a.err, a.text.chars().take(300).collect::<String>());
            return a;
        }
    }
    fn ok(&mut self, name: &str, args: &str, timeout_ms: u64) -> Json {
        let a = self.call(name, args, timeout_ms);
        assert!(!a.err, "{name}: {}", a.text);
        Json::parse(&a.text).unwrap()
    }
}

/// The staged Brave's browser process (no --type=), as browser::foreign finds it.
fn brave_pid(stage: &str) -> Option<u32> {
    let exe = format!("{stage}/brave");
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
        let cmd = std::fs::read(e.path().join("cmdline")).ok()?;
        let cmd = String::from_utf8_lossy(&cmd).into_owned();
        let mut w = cmd.split(['\0', ' ']).filter(|w| !w.is_empty());
        (w.next() == Some(exe.as_str()) && !w.any(|x| x.starts_with("--type="))).then_some(pid)
    })
}

struct Stop(Child);
impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn s<'a>(j: &'a Json, k: &str) -> &'a str {
    j.get(k).and_then(Json::as_str).unwrap_or("")
}

#[test]
fn every_action_on_headless_brave() {
    let stage = std::env::var("BRAVE_STAGE").unwrap_or_else(|_| "/var/tmp/claude-v2/stage/brave".into());
    assert!(Path::new(&stage).join("brave").is_file(), "no staged Brave in {stage}: stage it (no test is skipped, Georg 30.10.)");
    let dir = std::env::temp_dir().join(format!("brave-act-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    http(l);
    let base = format!("http://ac3.test:{port}");
    let (ksock, ssock) = (dir.join("mcp.sock"), dir.join("settings.sock"));
    let values = Arc::new(Mutex::new(Vec::new()));
    let src = Arc::new(Mutex::new("register".to_string()));
    settingsd(ssock.clone(), values.clone(), src.clone());
    let kl = UnixListener::bind(&ksock).unwrap();
    let launcher = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/host-launcher.sh");
    let child = Command::new(env!("CARGO_BIN_EXE_brave-act"))
        .args(["--launcher", &format!("/bin/sh {}", launcher.display()), "--tree", "/nonexistent-ac3-tree"])
        .args(["--download-dir", &dir.join("dl").display().to_string(), "--log", &dir.join("launcher.log").display().to_string()])
        .args(["--mcp-socket", &ksock.display().to_string(), "--settings-socket", &ssock.display().to_string()])
        .args(["--brave-arg", "--host-resolver-rules=MAP ac3.test 127.0.0.1, MAP * ~NOTFOUND"])
        .env("BRAVE_STAGE", &stage)
        .env("AC3_HOME", dir.join("home"))
        .spawn()
        .unwrap();
    let _stop = Stop(child);
    let (sk, _) = kl.accept().unwrap();
    let mut k = Kernel { w: sk.try_clone().unwrap(), r: BufReader::new(sk), id: 100 };
    let mut l = String::new();
    k.r.read_line(&mut l).unwrap();
    let reg = Json::parse(l.trim()).unwrap();
    let names: Vec<String> = reg.get("params").and_then(|p| p.get("tools")).and_then(Json::as_array).unwrap().iter().map(|t| s(t, "name").to_string()).collect();
    assert_eq!(names, ["brave_open_url", "brave_tabs", "brave_read_page", "brave_screenshot_tab", "brave_click_link", "brave_fill_form", "brave_download"]);
    let list = names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(",");
    k.w.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"registered\":[{list}]}}}}\n").as_bytes()).unwrap();

    let t0 = Instant::now();
    let o = k.ok("brave_open_url", &format!(r#"{{"url":"{base}/index.html"}}"#), 120_000);
    println!("Brave started and loaded the page in {:.1} s", t0.elapsed().as_secs_f64());
    assert_eq!(s(&o, "title"), "AC3 index");
    let tab = s(&o, "tab").to_string();
    let o2 = k.ok("brave_open_url", &format!(r#"{{"url":"{base}/second.html"}}"#), 30_000);
    assert_eq!(s(&o2, "title"), "AC3 second");
    assert!(k.call("brave_open_url", r#"{"url":"chrome://settings"}"#, 5000).text.contains("not an http"));
    // file: would read any file of Brave's user through read_page without a permission.
    assert!(k.call("brave_open_url", r#"{"url":"file:///etc/passwd"}"#, 5000).text.contains("not an http"));

    // A page that never answers: timeout_ms ends the call, and brave-act goes on.
    let t1 = Instant::now();
    let h = k.call("brave_open_url", &format!(r#"{{"url":"{base}/hang"}}"#), 3000);
    assert!(h.err && (h.text.contains("in time") || h.text.contains("timeout_ms")), "{}", h.text);
    assert!(t1.elapsed() < Duration::from_secs(10), "the hanging page held the call {:?}", t1.elapsed());

    // A page larger than an answer: read_page cuts it (truncated), the answer stays below the kernel's 4 MiB.
    let big = k.ok("brave_open_url", &format!(r#"{{"url":"{base}/big.html"}}"#), 60_000);
    let b = k.call("brave_read_page", &format!(r#"{{"tab":"{}","max_chars":1000000}}"#, s(&big, "tab")), 30_000);
    assert!(!b.err && b.text.len() < 4 << 20, "{} bytes: {}", b.text.len(), b.text.chars().take(200).collect::<String>());
    let bj = Json::parse(&b.text).unwrap();
    assert_eq!(bj.get("truncated").and_then(Json::as_bool), Some(true));
    assert!(s(&bj, "text").starts_with("語語語"));

    let tabs = k.ok("brave_tabs", "{}", 10_000);
    let ids: Vec<&str> = tabs.get("tabs").and_then(Json::as_array).unwrap().iter().map(|t| s(t, "tab")).collect();
    assert!(ids.contains(&tab.as_str()) && ids.contains(&s(&o2, "tab")), "{tabs}");

    let r = k.ok("brave_read_page", &format!(r#"{{"tab":"{tab}"}}"#), 10_000);
    assert!(s(&r, "text").contains("Hello from the AC-3 test site: Grüße"), "{r}");
    assert!(r.to_string().contains("second.html"), "links listed: {r}");

    let a = k.call("brave_screenshot_tab", &format!(r#"{{"tab":"{tab}"}}"#), 20_000);
    assert!(!a.err, "{}", a.text);
    let png = a.png.expect("a PNG");
    assert_eq!(&png[1..4], b"PNG");
    let info = Json::parse(&a.text).unwrap();
    assert!(info.get("width").and_then(Json::as_u64).unwrap_or(0) >= 640, "{info}");
    std::fs::write(dir.join("shot.png"), &png).unwrap();

    let c = k.ok("brave_click_link", &format!(r#"{{"tab":"{tab}","text":"zweite"}}"#), 30_000);
    assert_eq!(s(&c, "title"), "AC3 second");
    let back = k.ok("brave_click_link", &format!(r#"{{"tab":"{tab}","href":"index.html"}}"#), 30_000);
    assert_eq!(s(&back, "title"), "AC3 index");
    assert!(k.call("brave_click_link", &format!(r#"{{"tab":"{tab}","text":"Skript"}}"#), 10_000).text.contains("only http"));
    assert!(k.call("brave_click_link", &format!(r#"{{"tab":"{tab}","text":"nirgends"}}"#), 10_000).text.contains("no link fits"));

    // The change actions: refused while their keys are deny (registered by brave-act), then allowed.
    let fill = format!(r#"{{"tab":"{tab}","fields":{{"who":"Claude","agree":true,"color":"blau"}},"submit":true}}"#);
    let a = k.call("brave_fill_form", &fill, 30_000);
    assert!(a.err && a.text.contains("claude.actions.brave_fill_form"), "{}", a.text);
    let dl = format!(r#"{{"url":"{base}/data.bin"}}"#);
    let a = k.call("brave_download", &dl, 30_000);
    assert!(a.err && a.text.contains("claude.actions.brave_download"), "{}", a.text);
    assert!(!dir.join("dl").exists(), "nothing downloaded while refused");
    // An allow that an MCP client set itself (settings_set; the journal says source mcp) does not count.
    *src.lock().unwrap() = "mcp".into();
    for (_, v) in values.lock().unwrap().iter_mut() {
        *v = "allow".into();
    }
    let a = k.call("brave_download", &dl, 30_000);
    assert!(a.err && a.text.contains("over MCP"), "{}", a.text);
    assert!(!dir.join("dl").exists(), "nothing downloaded on a self-granted allow");
    // Georg's allow (his shell: source cli).
    *src.lock().unwrap() = "cli".into();
    let pw = format!(r#"{{"tab":"{tab}","fields":{{"who":"x","pw":"secret"}}}}"#);
    let a = k.call("brave_fill_form", &pw, 10_000);
    assert!(a.err && a.text.contains("password"), "{}", a.text);
    let f = k.ok("brave_fill_form", &fill, 30_000);
    assert_eq!(s(&f, "title"), "AC3 result who=Claude agree=yes color=b", "{f}");
    let d = k.ok("brave_download", &dl, 60_000);
    let path = s(&d, "path").to_string();
    assert_eq!(std::fs::read(&path).unwrap(), std::fs::read(site().join("data.bin")).unwrap(), "{path}");
    assert!(path.ends_with("/data.bin"), "{path}");
    let d2 = k.ok("brave_download", &dl, 60_000);
    assert!(s(&d2, "path").ends_with("/data (1).bin"), "{d2}");
    let a = k.call("brave_download", &format!(r#"{{"url":"{base}/second.html"}}"#), 60_000);
    assert!(a.err && a.text.contains("did not start a download"), "{}", a.text);
    let left: Vec<_> = std::fs::read_dir(dir.join("dl")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left.len(), 2, "only the two files, no guid leftovers: {left:?}");
    let a = k.call("brave_download", r#"{"url":"http://ac3.test/x.bin","dir":"/etc"}"#, 10_000);
    assert!(a.err && a.text.contains("outside"), "{}", a.text);

    // Brave killed: the next call finds the pipe ended and starts Brave again.
    let pid = brave_pid(&stage).expect("Brave's browser process");
    assert!(Command::new("kill").args(["-KILL", &pid.to_string()]).status().unwrap().success());
    let end = Instant::now() + Duration::from_secs(20);
    // Gone from /proc once brave-act's waiter reaped it (the launcher's end is what brave-act goes by).
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < end, "Brave {pid} did not end on SIGKILL");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(200));
    let again = k.ok("brave_open_url", &format!(r#"{{"url":"{base}/second.html"}}"#), 120_000);
    assert_eq!(s(&again, "title"), "AC3 second");
    let pid2 = brave_pid(&stage).expect("a new Brave");
    assert_ne!(pid, pid2);

    // Brave ends when brave-act's pipe closes; brave-act ends when the kernel closes the registry connection.
    drop(k);
    let mut stop = _stop;
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Some(st)) = stop.0.try_wait() {
            println!("brave-act ended: {st}");
            break;
        }
        assert!(Instant::now() < end, "brave-act did not end after the kernel closed the connection");
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
