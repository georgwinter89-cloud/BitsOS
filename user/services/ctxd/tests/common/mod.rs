//! Shared helpers of the integration tests: temp folders under the target dir, the built binary, a served
//! ctxd with its socket and MCP port (ports from the CX range 29500-29999), plain HTTP and socket clients.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ctxd"))
}

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

pub fn eval_docs() -> PathBuf {
    repo().join("host/cx-eval/docs")
}

pub fn fixtures() -> PathBuf {
    repo().join("host/tests/cx/fixtures")
}

pub fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cx-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

pub fn model_available() -> bool {
    let tp = Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party");
    tp.join("models/multilingual-e5-small-q8_0.gguf").is_file() && tp.join("build/llama.cpp/bin/llama-server").is_file()
}

/// True when the test must skip for lack of the model. The skip is not silent: it is appended to
/// `<target>/tmp/cx-skips.txt`, which host/tests/cx/10-ctxd.sh prints; `CTXD_REQUIRE_MODEL=1` makes it fail.
pub fn skip_without_model(test: &str) -> bool {
    if model_available() {
        return false;
    }
    assert!(
        std::env::var_os("CTXD_REQUIRE_MODEL").is_none(),
        "{test}: CTXD_REQUIRE_MODEL is set but third_party/ is missing"
    );
    let line = format!("SKIP {test}: model or llama-server not built (user/services/ctxd/third_party/)\n");
    eprint!("{line}");
    let f = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cx-skips.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
        let _ = f.write_all(line.as_bytes());
    }
    true
}

/// A `ctxd serve` child, killed (SIGKILL) on drop.
pub struct Served {
    pub child: Child,
    pub sock: PathBuf,
    pub port: u16,
    pub dir: PathBuf,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn write_config(dir: &Path, port: u16, roots: &[(&Path, bool)], extra: &str) -> PathBuf {
    let mut c = format!("[context]\nsocket = \"{}\"\nroots = [", dir.join("ctxd.sock").display());
    for (p, private) in roots {
        c.push_str(&format!("{{ path = \"{}\", private = {} }}, ", p.display(), private));
    }
    c.push_str(&format!("]\n{extra}\n[context.mcp]\nport = {port}\n"));
    let f = dir.join("config.toml");
    std::fs::write(&f, c).unwrap();
    f
}

pub fn serve(dir: &Path, cfg: &Path, embedder: &str, port: u16) -> Served {
    let child = Command::new(bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--data",
            dir.join("data").to_str().unwrap(),
            "--embedder",
            embedder,
            "serve",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let s = Served { child, sock: dir.join("ctxd.sock"), port, dir: dir.to_path_buf() };
    // Ready = the first generation is committed.
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        if let Ok(v) = sock_call(&s.sock, "context_status", json!({}))
            && v["result"]["generation"].is_u64()
        {
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("ctxd serve did not index in time");
}

pub fn sock_line(sock: &Path, line: &[u8]) -> Value {
    let s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut w = s.try_clone().unwrap();
    w.write_all(line).unwrap();
    w.write_all(b"\n").unwrap();
    let mut r = BufReader::new(s);
    let mut l = String::new();
    r.read_line(&mut l).unwrap();
    serde_json::from_str(&l).unwrap()
}

pub fn sock_call(sock: &Path, op: &str, args: Value) -> Result<Value, String> {
    let s = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut w = s.try_clone().unwrap();
    writeln!(w, "{}", json!({"id": 1, "op": op, "args": args})).map_err(|e| e.to_string())?;
    let mut l = String::new();
    BufReader::new(s).read_line(&mut l).map_err(|e| e.to_string())?;
    serde_json::from_str(&l).map_err(|e| e.to_string())
}

/// Raw HTTP POST; returns (status, body).
pub fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default();
    (status, body)
}

/// One MCP tool call; returns the whole raw body (for leak checks) and the parsed result.
pub fn mcp_tool(port: u16, name: &str, args: Value) -> (String, Value) {
    let req = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let (st, body) = http(port, "POST", "/mcp", &[("Content-Type", "application/json")], &req.to_string());
    assert_eq!(st, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    (body, v["result"].clone())
}
