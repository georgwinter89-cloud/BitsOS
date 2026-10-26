//! `cargo xtask abi-diff`: the ABI differential test (AB-3; `spec/rust-ecosystem.md` §4.3 "Linux is the
//! oracle"). Every program under `user/abi-tests` is built once for x86_64-unknown-linux-musl (static), run
//! on this CachyOS host and inside V2 (a QEMU boot with the programs in its initramfs, started through the
//! `run` MCP tool over a QEMU host forward in the lane's port range), and the two transcripts (how it ended,
//! stdout, stderr) are compared. The same binary runs on both sides, so every difference is the kernel's.
//!
//! Programs: `user/abi-tests/c/*.c` (one program each, `abi-<stem>`, built with clang and the musl that ships
//! with Rust's musl target) and the binaries of the Cargo package `user/abi-tests/rust` (built with the
//! stable toolchain of `user/`). How each runs: `user/abi-tests/cases.txt` (argv and stdin per case); a
//! program without a line there runs once with no arguments and empty stdin. Format: see that file.
//!
//! Output: one line per case (`same`, `DIFF` or `KNOWN`), the first differing lines, and both transcripts in
//! `target/xtask/abi-diff/<case>.{linux,v2}.txt`; afterwards V2's `syscall enosys` events (numbers V2 did not
//! implement that the programs called). A case listed in `user/abi-tests/expected/*.known` as
//! `<program> [args]: <owner>: <why>` that differs prints `KNOWN` and does not count (SC-5 CR7b, INT-A2a); a
//! listed case that is the same says so (the line can go). Exit status 0 only when every other case is the same.
//!
//! AB-8: dynamic glibc programs (`user/abi-tests/dyn`, CachyOS's `ls` and `cat`) and their library set, see
//! `abi_dyn.rs`; the libraries never run as cases.

#[path = "abi_dyn.rs"]
mod abi_dyn;

use crate::image::{self, BuildOpts, ImageOpts};
use crate::qemu::{self, QemuOpts};
use crate::util::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: cargo xtask abi-diff [FILTER] [--host-only] [--port P] [--timeout S] [--smp N]
  FILTER       run only cases whose name starts with it
  --host-only  build and run on CachyOS only, print the transcripts (for writing programs)
  --port P     host port for the QEMU forward to V2's MCP port (default: lane range start + 110)
  --timeout S  QEMU lifetime limit in seconds (default 900)
  --smp N      CPUs of the V2 guest (default 2)
  --remote HOST:PORT --token T
               no QEMU: diff against a running V2 whose image holds the programs in /bin";

/// Fixed environment on both sides (the host run clears its own).
const ENV: [&str; 3] = ["PATH=/bin", "HOME=/", "ABI_TEST=1"];
const GUEST_MCP_PORT: u16 = 8765;
const TOKEN: &str = "abi-diff-token-7f3a91";
const NAME: &str = "abi-diff";
/// One program's limit on either side.
const RUN_LIMIT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    /// Unique name (program, `#n` when it has several lines).
    pub name: String,
    pub program: String,
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
}

/// How a run ended plus what it wrote: the thing that is compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    pub end: String,
    pub stdout: String,
    pub stderr: String,
}

impl Transcript {
    pub fn text(&self) -> String {
        format!("end: {}\n--- stdout ---\n{}--- stderr ---\n{}", self.end, ensure_nl(&self.stdout), ensure_nl(&self.stderr))
    }
}

fn ensure_nl(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') { s.to_string() } else { format!("{s}\n") }
}

// ------------------------------------------------------------------------------------------------- cases

/// Splits a line into words: spaces separate, double quotes group, `\"`, `\\`, `\n`, `\t` inside quotes.
pub fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quoted = false;
    let mut it = line.chars();
    while let Some(ch) = it.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                have = true;
            }
            '\\' if quoted => match it.next() {
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some(c @ ('"' | '\\')) => cur.push(c),
                Some(c) => return Err(format!("unknown escape \\{c}")),
                None => return Err("line ends in an escape".into()),
            },
            c if c.is_whitespace() && !quoted => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if quoted {
        return Err("unterminated quote".into());
    }
    if have {
        out.push(cur);
    }
    Ok(out)
}

/// `cases.txt`: `<program> [args...] [stdin=<word>]`, `#` comments. Programs without a line get one default
/// case.
pub fn parse_cases(text: &str, programs: &[String]) -> Result<Vec<Case>, String> {
    let mut v: Vec<Case> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut w = words(line).map_err(|e| format!("cases.txt:{}: {e}", i + 1))?;
        let program = w.remove(0);
        if !programs.contains(&program) {
            if program.starts_with("dyn-") {
                // AB-8: the dynamic programs need the host's gcc; without them their lines are left out.
                continue;
            }
            return Err(format!("cases.txt:{}: no program '{program}' in user/abi-tests", i + 1));
        }
        let mut stdin = Vec::new();
        let mut args = Vec::new();
        for x in w {
            match x.strip_prefix("stdin=") {
                Some(s) => stdin = s.as_bytes().to_vec(),
                None => args.push(x),
            }
        }
        let n = v.iter().filter(|c| c.program == program).count();
        let name = if n == 0 { program.clone() } else { format!("{program}#{}", n + 1) };
        let mut argv = vec![program.clone()];
        argv.extend(args);
        v.push(Case { name, program, argv, stdin });
    }
    for p in programs {
        if !abi_dyn::is_lib(p) && !v.iter().any(|c| &c.program == p) {
            v.push(Case { name: p.clone(), program: p.clone(), argv: vec![p.clone()], stdin: Vec::new() });
        }
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(v)
}

/// The known differences of whole cases: `<program> [args]: <owner>: <why>` lines of
/// `user/abi-tests/expected/*.known` (other lines there are line prefixes the ktests use; they name no case).
fn known_cases(root: &Path) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let Ok(dir) = std::fs::read_dir(root.join("user/abi-tests/expected")) else { return v };
    for e in dir.flatten() {
        if e.path().extension().is_none_or(|x| x != "known") {
            continue;
        }
        for line in std::fs::read_to_string(e.path()).unwrap_or_default().lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || !line.starts_with("abi-") {
                continue;
            }
            if let Some((case, why)) = line.split_once(": ") {
                v.push((case.trim().to_string(), why.trim().to_string()));
            }
        }
    }
    v
}

// ------------------------------------------------------------------------------------------------- build

/// UL-1's musl sysroot for `v2-cc`: `$V2_SYSROOT`, else `user/sysroot/out/sysroot`, built with
/// `user/sysroot/build.sh` when it is missing (log in target/xtask/abi-diff/sysroot-build.log).
pub(crate) fn sysroot(root: &Path) -> Result<PathBuf, String> {
    let s = std::env::var_os("V2_SYSROOT").map(PathBuf::from).unwrap_or_else(|| root.join("user/sysroot/out/sysroot"));
    if s.join("usr/lib/libc.a").is_file() {
        return Ok(s);
    }
    let log = xdir(root).join("abi-diff/sysroot-build.log");
    eprintln!("xtask: building UL-1's musl sysroot (user/sysroot/build.sh, log {})", log.display());
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let f = std::fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    let st = Command::new(root.join("user/sysroot/build.sh"))
        .current_dir(root)
        .stdout(f.try_clone().map_err(|e| e.to_string())?)
        .stderr(f)
        .status()
        .map_err(|e| format!("user/sysroot/build.sh: {e}"))?;
    if !st.success() || !s.join("usr/lib/libc.a").is_file() {
        return Err(format!("no musl sysroot at {} (build.sh: {st}; see {}; or set V2_SYSROOT)", s.display(), log.display()));
    }
    Ok(s)
}

/// Builds every program; returns (name, file). (pub: the desk may put them into every image.)
pub fn build_programs(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let dir = root.join("user/abi-tests");
    let out = xdir(root).join("abi-diff/bin");
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut progs = Vec::new();
    let cdir = dir.join("c");
    let mut srcs: Vec<PathBuf> = std::fs::read_dir(&cdir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "c")).collect()).unwrap_or_default();
    srcs.sort();
    if !srcs.is_empty() {
        let sysroot = sysroot(root)?;
        for s in srcs {
            let name = format!("abi-{}", s.file_stem().unwrap_or_default().to_string_lossy());
            let exe = out.join(&name);
            let mut c = Command::new(root.join("user/sysroot/bin/v2-cc"));
            c.env("V2_SYSROOT", &sysroot).args(["-O2", "-Wall", "-Werror", "-std=gnu11"]).arg(format!("-I{}", cdir.display())).arg("-o").arg(&exe).arg(&s);
            run_status(&mut c, &format!("building {name}"))?;
            progs.push((name, exe));
        }
    }
    let rdir = dir.join("rust");
    if rdir.join("Cargo.toml").is_file() {
        let tdir = root.join("target/abi-tests");
        // From inside user/ so its stable toolchain file applies; the package is a workspace of its own.
        let mut c = Command::new("cargo");
        c.current_dir(&rdir)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["build", "--release", "--target", "x86_64-unknown-linux-musl", "--target-dir"])
            .arg(&tdir);
        run_status(&mut c, "building user/abi-tests/rust")?;
        let rel = tdir.join("x86_64-unknown-linux-musl/release");
        let mut names: Vec<String> = std::fs::read_dir(rdir.join("src/bin"))
            .map(|rd| rd.flatten().filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string())).collect())
            .unwrap_or_default();
        names.sort();
        for n in names {
            let p = rel.join(&n);
            if !p.is_file() {
                return Err(format!("{} was not built", p.display()));
            }
            progs.push((n, p));
        }
    }
    if progs.is_empty() {
        return Err(format!("no programs in {}", dir.display()));
    }
    progs.extend(abi_dyn::build(root, &out));
    Ok(progs)
}

// ------------------------------------------------------------------------------------------------- host

fn describe(st: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        _ => "unknown".into(),
    }
}

fn run_host(exe: &Path, c: &Case) -> Result<Transcript, String> {
    use std::os::unix::process::CommandExt;
    // stdin as V2's `run` gives it: a pipe that already holds every byte and whose writer has closed (written
    // before the start, so the program never sees a half-written pipe; cases keep their input small).
    if c.stdin.len() > 32 * 1024 {
        return Err(format!("{}: stdin above 32 KiB does not fit a pipe written before the start", c.name));
    }
    let (rd, mut wr) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
    wr.write_all(&c.stdin).map_err(|e| format!("stdin pipe: {e}"))?;
    drop(wr);
    let mut cmd = Command::new(exe);
    cmd.arg0(&c.argv[0]).args(&c.argv[1..]).env_clear().current_dir("/").stdin(rd).stdout(Stdio::piped()).stderr(Stdio::piped());
    for e in ENV {
        let (k, v) = e.split_once('=').unwrap_or((e, ""));
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
    let mut so = child.stdout.take().ok_or("no stdout pipe")?;
    let mut se = child.stderr.take().ok_or("no stderr pipe")?;
    let ro = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v);
        v
    });
    let re = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v);
        v
    });
    let start = Instant::now();
    let st = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if start.elapsed() > RUN_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} did not end within {} s on the host", c.name, RUN_LIMIT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = ro.join().unwrap_or_default();
    let err = re.join().unwrap_or_default();
    Ok(Transcript { end: describe(st), stdout: String::from_utf8_lossy(&out).into_owned(), stderr: String::from_utf8_lossy(&err).into_owned() })
}

// ------------------------------------------------------------------------------------------------- JSON

/// The JSON this client needs: values of MCP answers, and strings for requests.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(v) => v.iter().find(|(n, _)| n == k).map(|(_, x)| x),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self { Some(s) } else { None }
    }
    pub fn parse(s: &str) -> Result<Json, String> {
        let b = s.as_bytes();
        let mut i = 0;
        let v = value(b, &mut i, 0)?;
        skip_ws(b, &mut i);
        if i != b.len() {
            return Err(format!("trailing bytes at {i}"));
        }
        Ok(v)
    }
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && b[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn value(b: &[u8], i: &mut usize, depth: usize) -> Result<Json, String> {
    if depth > 64 {
        return Err("nested too deep".into());
    }
    skip_ws(b, i);
    match b.get(*i) {
        None => Err("unexpected end".into()),
        Some(b'{') => {
            *i += 1;
            let mut v = Vec::new();
            skip_ws(b, i);
            if b.get(*i) == Some(&b'}') {
                *i += 1;
                return Ok(Json::Obj(v));
            }
            loop {
                skip_ws(b, i);
                let Json::Str(k) = value(b, i, depth + 1)? else { return Err("object key".into()) };
                skip_ws(b, i);
                if b.get(*i) != Some(&b':') {
                    return Err(format!("':' expected at {i}"));
                }
                *i += 1;
                v.push((k, value(b, i, depth + 1)?));
                skip_ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b'}') => {
                        *i += 1;
                        return Ok(Json::Obj(v));
                    }
                    _ => return Err(format!("',' or '}}' expected at {i}")),
                }
            }
        }
        Some(b'[') => {
            *i += 1;
            let mut v = Vec::new();
            skip_ws(b, i);
            if b.get(*i) == Some(&b']') {
                *i += 1;
                return Ok(Json::Arr(v));
            }
            loop {
                v.push(value(b, i, depth + 1)?);
                skip_ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b']') => {
                        *i += 1;
                        return Ok(Json::Arr(v));
                    }
                    _ => return Err(format!("',' or ']' expected at {i}")),
                }
            }
        }
        Some(b'"') => {
            *i += 1;
            let mut s: Vec<u8> = Vec::new();
            loop {
                match b.get(*i) {
                    None => return Err("unterminated string".into()),
                    Some(b'"') => {
                        *i += 1;
                        return String::from_utf8(s).map(Json::Str).map_err(|_| "string is not UTF-8".into());
                    }
                    Some(b'\\') => {
                        *i += 1;
                        let e = *b.get(*i).ok_or("escape at the end")?;
                        *i += 1;
                        let c = match e {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'/' => '/',
                            b'b' => '\u{8}',
                            b'f' => '\u{c}',
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            b'u' => {
                                let hex = |i: &mut usize| -> Result<u32, String> {
                                    let h = b.get(*i..*i + 4).ok_or("short \\u")?;
                                    *i += 4;
                                    u32::from_str_radix(std::str::from_utf8(h).map_err(|_| "bad \\u")?, 16).map_err(|_| "bad \\u".to_string())
                                };
                                let mut cp = hex(i)?;
                                if (0xD800..0xDC00).contains(&cp) && b.get(*i..*i + 2) == Some(b"\\u") {
                                    *i += 2;
                                    let lo = hex(i)?;
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                                }
                                char::from_u32(cp).unwrap_or('\u{FFFD}')
                            }
                            _ => return Err(format!("bad escape at {i}")),
                        };
                        let mut buf = [0u8; 4];
                        s.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                    Some(&c) => {
                        s.push(c);
                        *i += 1;
                    }
                }
            }
        }
        Some(b't') if b[*i..].starts_with(b"true") => {
            *i += 4;
            Ok(Json::Bool(true))
        }
        Some(b'f') if b[*i..].starts_with(b"false") => {
            *i += 5;
            Ok(Json::Bool(false))
        }
        Some(b'n') if b[*i..].starts_with(b"null") => {
            *i += 4;
            Ok(Json::Null)
        }
        Some(_) => {
            let st = *i;
            while *i < b.len() && matches!(b[*i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                *i += 1;
            }
            std::str::from_utf8(&b[st..*i]).ok().and_then(|t| t.parse().ok()).map(Json::Num).ok_or_else(|| format!("bad value at {st}"))
        }
    }
}

/// A JSON string literal.
pub fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn jarr(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| jstr(s)).collect::<Vec<_>>().join(","))
}

// ------------------------------------------------------------------------------------------------- MCP

/// Where V2's MCP server is: the QEMU forward on 127.0.0.1, or `--remote`.
#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    token: String,
}

struct Mcp {
    t: Target,
    conn: Option<TcpStream>,
    id: u64,
}

impl Mcp {
    fn post(&mut self, body: &str) -> Result<String, String> {
        for attempt in 0..2 {
            if self.conn.is_none() {
                let s = TcpStream::connect((self.t.host.as_str(), self.t.port)).map_err(|e| format!("connect: {e}"))?;
                s.set_read_timeout(Some(Duration::from_secs(120))).map_err(|e| e.to_string())?;
                self.conn = Some(s);
            }
            let s = self.conn.as_mut().ok_or("no connection")?;
            let req = format!(
                "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                self.t.host,
                self.t.token,
                body.len()
            );
            match s.write_all(req.as_bytes()).map_err(|e| e.to_string()).and_then(|_| read_response(s)) {
                Ok((status, close, text)) => {
                    if close {
                        self.conn = None;
                    }
                    if status != 200 {
                        return Err(format!("HTTP {status}: {text}"));
                    }
                    return Ok(text);
                }
                Err(e) => {
                    self.conn = None;
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        Err("unreachable".into())
    }

    fn rpc(&mut self, method: &str, params: &str) -> Result<Json, String> {
        self.id += 1;
        let body = format!(r#"{{"jsonrpc":"2.0","id":{},"method":{},"params":{params}}}"#, self.id, jstr(method));
        let t = self.post(&body)?;
        let j = Json::parse(&t).map_err(|e| format!("{method}: bad JSON ({e}): {t}"))?;
        if let Some(e) = j.get("error") {
            return Err(format!("{method}: {e:?}"));
        }
        j.get("result").cloned().ok_or_else(|| format!("{method}: no result: {t}"))
    }

    /// tools/call: (isError, the text items joined).
    fn tool(&mut self, name: &str, args: &str) -> Result<(bool, String), String> {
        let r = self.rpc("tools/call", &format!(r#"{{"name":{},"arguments":{args}}}"#, jstr(name)))?;
        let text = match r.get("content") {
            Some(Json::Arr(items)) => items.iter().filter_map(|i| i.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        };
        Ok((r.get("isError") == Some(&Json::Bool(true)), text))
    }
}

/// Reads one HTTP/1.1 response with Content-Length: (status, server closes, body).
fn read_response(s: &mut TcpStream) -> Result<(u16, bool, String), String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 1 << 20 {
            return Err("HTTP head too long".into());
        }
        let n = s.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status: u16 = head.split_whitespace().nth(1).and_then(|x| x.parse().ok()).ok_or_else(|| format!("bad status line: {head}"))?;
    let mut len = 0usize;
    let mut close = false;
    for l in head.lines().skip(1) {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().map_err(|_| "bad Content-Length")?,
                "connection" => close = v.trim().eq_ignore_ascii_case("close"),
                _ => {}
            }
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).map_err(|e| format!("read body: {e}"))?;
        if n == 0 {
            return Err("connection closed in the body".into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok((status, close, String::from_utf8_lossy(&body).into_owned()))
}

/// The `run` tool's answer: "pid N ended: exit 0" / "... signal 6 (SIGABRT), code 134", an optional message
/// line, then "--- stdout (<n> bytes...) ---" and "--- stderr (...) ---" sections.
pub fn parse_run_answer(text: &str) -> Result<Transcript, String> {
    let head = text.lines().next().unwrap_or("");
    let end = head.split_once(" ended: ").map(|(_, r)| r).ok_or_else(|| format!("no end in: {head}"))?;
    let end = end.split(',').next().unwrap_or(end).split(" (").next().unwrap_or(end).trim().to_string();
    let section = |name: &str, from: usize| -> Result<(String, usize), String> {
        let marker = format!("--- {name} (");
        let at = text[from..].find(&marker).map(|p| p + from).ok_or_else(|| format!("no {name} section"))?;
        let rest = &text[at + marker.len()..];
        let n: usize = rest.split(' ').next().and_then(|x| x.parse().ok()).ok_or_else(|| format!("no {name} length"))?;
        let body_at = at + marker.len() + rest.find(") ---\n").ok_or_else(|| format!("bad {name} marker"))? + 6;
        let body = text.get(body_at..).unwrap_or("");
        // The tool adds a newline when the output had none: the byte count says where it really ends.
        let take = n.min(body.len());
        let cut = (0..=take).rev().find(|&k| body.is_char_boundary(k)).unwrap_or(0);
        Ok((body[..cut].to_string(), body_at + cut))
    };
    let (stdout, after) = section("stdout", 0)?;
    let (stderr, _) = section("stderr", after)?;
    Ok(Transcript { end, stdout, stderr })
}

fn run_v2(m: &mut Mcp, c: &Case) -> Result<Transcript, String> {
    let stdin = String::from_utf8_lossy(&c.stdin).into_owned();
    let env: Vec<String> = ENV.iter().map(|s| s.to_string()).collect();
    let args = format!(r#"{{"path":{},"argv":{},"env":{},"stdin":{},"timeout_ms":{}}}"#, jstr(&format!("/bin/{}", c.program)), jarr(&c.argv), jarr(&env), jstr(&stdin), RUN_LIMIT.as_millis());
    let (err, text) = m.tool("run", &args)?;
    if err {
        return Err(format!("run {}: {text}", c.name));
    }
    parse_run_answer(&text)
}

// ------------------------------------------------------------------------------------------------- diff

/// The first differing line of two texts with two lines of context on each side, or None when equal.
pub fn first_diff(a: &str, b: &str) -> Option<String> {
    if a == b {
        return None;
    }
    let la: Vec<&str> = a.lines().collect();
    let lb: Vec<&str> = b.lines().collect();
    let i = (0..la.len().max(lb.len())).find(|&i| la.get(i) != lb.get(i)).unwrap_or(0);
    let from = i.saturating_sub(2);
    let mut s = String::new();
    for (tag, l) in [("linux", &la), ("v2   ", &lb)] {
        for (k, line) in l.iter().enumerate().skip(from).take(5) {
            s.push_str(&format!("    {tag} {}{:>4}: {line}\n", if k == i { '>' } else { ' ' }, k + 1));
        }
        if l.len() <= i {
            s.push_str(&format!("    {tag} >{:>4}: (no line)\n", i + 1));
        }
    }
    Some(s)
}

// ------------------------------------------------------------------------------------------------- V2 boot

struct Vm {
    root: PathBuf,
    lane: String,
    thread: Option<std::thread::JoinHandle<Result<qemu::RunResult, String>>>,
}

impl Vm {
    /// Stops only the QEMU this run started (its pid file and its `-name`).
    fn stop(&mut self) {
        let pidfile = xdir(&self.root).join(format!("qemu-{NAME}.pid"));
        if let Ok(p) = std::fs::read_to_string(&pidfile) {
            let pid = p.trim().to_string();
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let ours = String::from_utf8_lossy(&cmdline).replace('\0', " ").contains(&format!("-name v2-{}-{NAME} ", self.lane));
            if ours {
                let _ = Command::new("kill").arg(&pid).status();
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.stop();
    }
}

fn boot(root: &Path, progs: &[(String, PathBuf)], port: u16, timeout: u64, smp: u32) -> Result<(Vm, PathBuf), String> {
    let mut art = image::build(root, BuildOpts::default())?;
    for (n, p) in progs {
        // INT-A1: images carry the abi programs themselves once the sysroot exists (AB-3 CR3); the same file is
        // fine, another program of that name is not.
        match art.programs.iter().find(|(m, _)| m == n) {
            Some((_, q)) if q == p => continue,
            Some(_) => return Err(format!("/bin/{n} exists in the image already; rename the abi test")),
            None => art.programs.push((n.clone(), p.clone())),
        }
    }
    let io = ImageOpts {
        name: NAME.into(),
        generation: 1,
        config_extra: vec![format!("token={TOKEN}"), format!("mcp_port={GUEST_MCP_PORT}"), "loglevel=info".into()],
        build: BuildOpts::default(),
    };
    image::make_image_from(root, &io, &art)?;
    let log = xdir(root).join("abi-diff/serial.log");
    let mut q = QemuOpts::new(NAME);
    q.net = "virtio".into();
    q.hostfwd = vec![(port, GUEST_MCP_PORT)];
    q.timeout = timeout;
    q.smp = smp;
    q.echo = false;
    q.serial_log = Some(log.clone());
    let r = root.to_path_buf();
    let thread = std::thread::spawn(move || qemu::run(&r, &q));
    Ok((Vm { root: root.to_path_buf(), lane: lane(root), thread: Some(thread) }, log))
}

/// Waits for V2's MCP server (`initialize` answered); stops early when this run's QEMU ended.
fn connect(t: &Target, mut vm: Option<&mut Vm>, wait: Duration) -> Result<Mcp, String> {
    let start = Instant::now();
    let mut last = String::new();
    loop {
        if let Some(vm) = vm.as_deref_mut()
            && vm.thread.as_ref().is_some_and(|t| t.is_finished())
        {
            let r = vm.thread.take().map(|t| t.join());
            return Err(format!("QEMU ended before MCP answered ({:?}); last error: {last}", r.map(|x| x.map(|y| y.map(|z| (z.exit, z.timed_out))))));
        }
        let mut m = Mcp { t: t.clone(), conn: None, id: 0 };
        match m.rpc("initialize", r#"{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"xtask-abi-diff","version":"1"}}"#) {
            Ok(_) => return Ok(m),
            Err(e) => last = e,
        }
        if start.elapsed() > wait {
            return Err(format!("no MCP answer on {}:{} within {} s: {last}", t.host, t.port, wait.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ------------------------------------------------------------------------------------------------- command

pub fn cmd_abi_diff(root: &Path, mut a: Args) -> Result<(), String> {
    if a.flag("--help") || a.flag("-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let host_only = a.flag("--host-only");
    // An already running V2 (the desk's Mini-PC round; its image must hold the programs in /bin).
    let remote = a.value("--remote")?;
    let token = a.value("--token")?;
    let remote = match remote {
        None => None,
        Some(r) => {
            let (h, p) = r.rsplit_once(':').ok_or("--remote wants HOST:PORT")?;
            let port: u16 = p.parse().map_err(|_| format!("--remote {r}: bad port"))?;
            let token = token.ok_or("--remote needs --token (the MCP token of that V2)")?;
            Some(Target { host: h.to_string(), port, token })
        }
    };
    let lane = lane(root);
    let (lo, hi) = lane_range(&lane).ok_or_else(|| format!("lane '{lane}' has no port range"))?;
    let port = a.num::<u16>("--port")?.unwrap_or(lo + 110);
    if !(lo..=hi).contains(&port) {
        return Err(format!("--port {port} is outside lane {lane}'s range {lo}-{hi}"));
    }
    let timeout = a.num::<u64>("--timeout")?.unwrap_or(900);
    let smp = a.num::<u32>("--smp")?.unwrap_or(2);
    let filter = a.positionals()?.first().cloned().unwrap_or_default();

    let progs = build_programs(root)?;
    let names: Vec<String> = progs.iter().map(|(n, _)| n.clone()).collect();
    let text = std::fs::read_to_string(root.join("user/abi-tests/cases.txt")).unwrap_or_default();
    let cases: Vec<Case> = parse_cases(&text, &names)?.into_iter().filter(|c| c.name.starts_with(&filter)).collect();
    if cases.is_empty() {
        return Err(format!("no case matches '{filter}'"));
    }
    let exe = |p: &str| progs.iter().find(|(n, _)| n == p).map(|(_, f)| f.clone()).ok_or_else(|| format!("no program {p}"));
    let outdir = xdir(root).join("abi-diff");
    std::fs::create_dir_all(&outdir).map_err(|e| e.to_string())?;

    let mut host = Vec::new();
    for c in &cases {
        let t = run_host(&exe(&c.program)?, c)?;
        std::fs::write(outdir.join(format!("{}.linux.txt", c.name)), t.text()).map_err(|e| e.to_string())?;
        if host_only {
            println!("=== {} ({})\n{}", c.name, c.argv.join(" "), t.text());
        }
        host.push(t);
    }
    if host_only {
        println!("abi-diff: {} cases run on the host only", cases.len());
        return Ok(());
    }

    let (mut vm, log, mut m) = match remote {
        Some(t) => {
            println!("abi-diff: V2 at {}:{} (the programs must be in its /bin)", t.host, t.port);
            (None, None, connect(&t, None, Duration::from_secs(30))?)
        }
        None => {
            if TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok() {
                return Err(format!("port {port} already has a listener (a VM left from an earlier run?); stop it by its pid first"));
            }
            let (mut vm, log) = boot(root, &progs, port, timeout, smp)?;
            let t = Target { host: "127.0.0.1".into(), port, token: TOKEN.into() };
            let m = connect(&t, Some(&mut vm), Duration::from_secs(240))?;
            (Some(vm), Some(log), m)
        }
    };
    if cases.iter().any(|c| c.program.starts_with("dyn-")) {
        abi_dyn::install_libs(&mut m, &progs)?;
    }
    let known = known_cases(root);
    let mut diffs = Vec::new();
    let mut known_diffs = 0;
    for (c, h) in cases.iter().zip(&host) {
        let v = match run_v2(&mut m, c) {
            Ok(v) => v,
            Err(e) => Transcript { end: format!("run failed: {e}"), stdout: String::new(), stderr: String::new() },
        };
        std::fs::write(outdir.join(format!("{}.v2.txt", c.name)), v.text()).map_err(|e| e.to_string())?;
        let listed = known.iter().find(|(k, _)| *k == c.argv.join(" "));
        match (first_diff(&h.text(), &v.text()), listed) {
            (None, None) => println!("same  {} ({})", c.name, h.end),
            (None, Some((k, _))) => println!("same  {} ({}; listed as known '{k}': the line can go)", c.name, h.end),
            (Some(d), Some((_, why))) => {
                println!("KNOWN {} ({why})\n{d}", c.name);
                known_diffs += 1;
            }
            (Some(d), None) => {
                println!("DIFF  {} (linux: {}, v2: {})\n{d}", c.name, h.end, v.end);
                diffs.push(c.name.clone());
            }
        }
    }
    // What V2 did not implement that the programs called (the to-do list of R6).
    if let Ok((_, t)) = m.tool("log", r#"{"subsystem":"syscall","name":"enosys","max":500}"#) {
        let lines: Vec<&str> = t.lines().filter(|l| l.contains("enosys")).collect();
        if lines.is_empty() {
            println!("v2: no syscall answered ENOSYS");
        } else {
            println!("v2: ENOSYS answers (each number once):");
            for l in lines {
                println!("  {l}");
            }
        }
    }
    if let Some(vm) = vm.as_mut() {
        vm.stop();
    }
    println!("transcripts: {}{}", outdir.display(), log.map(|l| format!("  serial log: {}", l.display())).unwrap_or_default());
    if diffs.is_empty() {
        println!("abi-diff: green ({} cases the same on CachyOS and V2, {known_diffs} known differences)", cases.len() - known_diffs);
        Ok(())
    } else {
        Err(format!("abi-diff: {} of {} cases differ: {}", diffs.len(), cases.len(), diffs.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_cases() {
        assert_eq!(words(r#"a "b c" "" d\e"#).unwrap(), ["a", "b c", "", r"d\e"]);
        assert_eq!(words(r#"stdin="x\ny""#).unwrap(), ["stdin=x\ny"]);
        assert!(words(r#""open"#).is_err());
        let progs = vec!["abi-a".to_string(), "abi-b".to_string()];
        let c = parse_cases("# c\nabi-a one \"two words\" stdin=\"in\\n\"\nabi-a\n", &progs).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], Case { name: "abi-a".into(), program: "abi-a".into(), argv: vec!["abi-a".into(), "one".into(), "two words".into()], stdin: b"in\n".to_vec() });
        assert_eq!(c[1].name, "abi-a#2");
        assert_eq!(c[2], Case { name: "abi-b".into(), program: "abi-b".into(), argv: vec!["abi-b".into()], stdin: vec![] });
        assert!(parse_cases("abi-x\n", &progs).is_err());
    }

    #[test]
    fn json_round_trip() {
        let j = Json::parse(r#"{"a":[1,-2.5e1,true,false,null],"s":"x\"\\\né😀","o":{}}"#).unwrap();
        assert_eq!(j.get("s").and_then(Json::as_str), Some("x\"\\\né😀"));
        assert_eq!(j.get("a"), Some(&Json::Arr(vec![Json::Num(1.0), Json::Num(-25.0), Json::Bool(true), Json::Bool(false), Json::Null])));
        assert_eq!(Json::parse(&jstr("q\"\u{1}\t")).unwrap(), Json::Str("q\"\u{1}\t".into()));
        for bad in ["", "{", "[1,", r#"{"a" 1}"#, r#""\x""#, "1 2", &"[".repeat(100)] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn run_answers() {
        let t = parse_run_answer("pid 7 ended: exit 3\n--- stdout (3 bytes) ---\nhi\n--- stderr (0 bytes) ---\n").unwrap();
        assert_eq!(t, Transcript { end: "exit 3".into(), stdout: "hi\n".into(), stderr: String::new() });
        // No newline at the end of the output: the tool adds one, the count removes it again.
        let t = parse_run_answer("pid 8 ended: signal 6 (SIGABRT), code 134\n--- stdout (2 bytes) ---\nab\n--- stderr (4 bytes) ---\nerr!\n").unwrap();
        assert_eq!(t, Transcript { end: "signal 6".into(), stdout: "ab".into(), stderr: "err!".into() });
        let t = parse_run_answer("pid 9 ended: signal 11 (SIGSEGV), code 139\nProgram x (pid 9, thread 3) ended: ERROR: Page fault\n--- stdout (0 bytes) ---\n--- stderr (0 bytes) ---\n").unwrap();
        assert_eq!(t.end, "signal 11");
        assert!(parse_run_answer("Still running (call 3)").is_err());
    }

    #[test]
    fn diffs() {
        assert_eq!(first_diff("a\nb\n", "a\nb\n"), None);
        let d = first_diff("a\nb\nc\n", "a\nX\nc\n").unwrap();
        assert!(d.contains("linux >   2: b") && d.contains("v2    >   2: X"), "{d}");
        let d = first_diff("a\n", "a\nb\n").unwrap();
        assert!(d.contains("(no line)"), "{d}");
    }
}
