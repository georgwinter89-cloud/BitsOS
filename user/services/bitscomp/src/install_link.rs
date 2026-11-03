//! MC-9b: bitscomp as the installer's confirmer. The protocol is bitsinstall's (user/services/bitsinstall/src/
//! confirm.rs, plan/notes/mc-MC-9a.md "Confirmer protocol"): one JSON object per line over the stream socket
//! `SOCKET`, which bitsinstall listens on; bitscomp connects and stays connected (reconnecting with a back-off when
//! bitsinstall restarts). A thread reads the lines and hands them to the main loop through a calloop channel; the
//! main loop writes its answers itself with a short send timeout (B4: a stalled installer is dropped, never waited
//! for). On V2 the peer must be root (running `/bin/bitsinstall` where its exe link can be read): a socket anyone
//! else made there is not taken.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

use crate::state::App;

pub const SOCKET: &str = "/run/bitsos/install.sock";
pub const INSTALLER: &str = "/bin/bitsinstall";
/// INT-W17 (MC-9b CR 5): `holder=<pid>` or `holder=none`, the kernel's word on who holds the installer gate.
const INSTALLER_HOLDER: &str = "/proc/bitsos/installer";

/// What a kernel `holder=` file says: `Some(Some(pid))`, `Some(None)` for `holder=none`, None for anything else.
fn holder_of(text: &str) -> Option<Option<u32>> {
    let v = text.trim().strip_prefix("holder=")?;
    if v == "none" {
        return Some(None);
    }
    v.parse::<u32>().ok().filter(|&p| p != 0).map(Some)
}
/// The longest line either side sends (confirm.rs LINE_MAX).
pub const LINE_MAX: usize = 64 * 1024;
/// The longest text drawn (confirm.rs TEXT_MAX; the installer already cuts to it).
pub const TEXT_MAX: usize = 4000;
const READ_WAIT: Duration = Duration::from_secs(1);
const WRITE_WAIT: Duration = Duration::from_millis(200);
const BACKOFF_MIN: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// A session this long counts as healthy even without a hello (the back-off starts over after it).
const HEALTHY_AFTER: Duration = Duration::from_secs(5);
/// The protocol version bitscomp speaks (confirm.rs `hello`).
pub const VERSION: u64 = 1;
/// What a dialog may last at most (mcp-wire installer DIALOG_MAX_MS); a longer ask is cut to it.
pub const DIALOG_MAX_MS: u64 = 240_000;

/// A standing grant as the installer reports it: kind and Unix end time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub kind: String,
    pub until: i64,
}

/// MC-10: a live full-access grant, and a paired client, as the installer reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessShown {
    pub client: String,
    pub name: String,
    pub until: i64,
    pub nvme: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientShown {
    pub client: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FromInstaller {
    Hello { version: u64, grants: Vec<Shown>, access: Vec<AccessShown>, clients: Vec<ClientShown> },
    /// MC-10: the question for full access (kind `access`).
    AskAccess { id: u64, title: String, text: String, timeout_ms: u64, client: String, name: String, hours: i64 },
    /// MC-10: full access now.
    Access { access: Vec<AccessShown>, clients: Vec<ClientShown> },
    Ask { id: u64, kind: String, title: String, text: String, timeout_ms: u64 },
    Cancel { id: u64, reason: String },
    Grants(Vec<Shown>),
    Refused { what: String, reason: String },
}

fn grants_of(j: Option<&Json>) -> Option<Vec<Shown>> {
    let a = j?.as_array()?;
    Some(
        a.iter()
            .filter_map(|g| Some(Shown { kind: g.get("kind")?.as_str()?.chars().take(16).collect(), until: g.get("until")?.as_i64()? }))
            .collect(),
    )
}

fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn name_of(j: &Json) -> String {
    j.get("name").and_then(Json::as_str).unwrap_or("?").chars().filter(|c| !c.is_control()).take(40).collect()
}
fn access_of(j: Option<&Json>) -> Vec<AccessShown> {
    j.and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|g| {
                    let client = g.get("client")?.as_str().filter(|c| hex64(c))?.to_string();
                    Some(AccessShown { client, name: name_of(g), until: g.get("until")?.as_i64()?, nvme: g.get("nvme").and_then(Json::as_bool).unwrap_or(false) })
                })
                .collect()
        })
        .unwrap_or_default()
}
fn clients_of(j: Option<&Json>) -> Vec<ClientShown> {
    j.and_then(Json::as_array)
        .map(|a| a.iter().filter_map(|c| Some(ClientShown { client: c.get("client")?.as_str().filter(|x| hex64(x))?.to_string(), name: name_of(c) })).collect())
        .unwrap_or_default()
}

/// One line of the installer; None for anything not understood (it is logged and ignored).
pub fn parse(line: &str) -> Option<FromInstaller> {
    if line.len() > LINE_MAX {
        return None;
    }
    let j = Json::parse(line).ok()?;
    let s = |k: &str| j.get(k).and_then(Json::as_str).map(String::from);
    Some(match j.get("type")?.as_str()? {
        "hello" => FromInstaller::Hello {
            version: j.get("version")?.as_u64()?,
            grants: grants_of(j.get("grants")).unwrap_or_default(),
            access: access_of(j.get("access")),
            clients: clients_of(j.get("clients")),
        },
        "access" => FromInstaller::Access { access: access_of(j.get("access")), clients: clients_of(j.get("clients")) },
        "ask" if j.get("kind").and_then(Json::as_str) == Some("access") => {
            let text = s("text")?;
            FromInstaller::AskAccess {
                id: j.get("id")?.as_u64()?,
                title: s("title")?.chars().take(200).collect(),
                text: text.chars().take(TEXT_MAX).collect(),
                timeout_ms: j.get("timeout_ms")?.as_u64()?.clamp(1, DIALOG_MAX_MS),
                client: s("client").filter(|c| hex64(c))?,
                name: name_of(&j),
                hours: j.get("hours").and_then(Json::as_i64).unwrap_or(24).clamp(1, 168),
            }
        }
        "ask" => {
            let text = s("text")?;
            // The installer keeps to TEXT_MAX; a longer text is cut here, never drawn past the box.
            let text = if text.chars().count() > TEXT_MAX { text.chars().take(TEXT_MAX).collect::<String>() + " …" } else { text };
            FromInstaller::Ask {
                id: j.get("id")?.as_u64()?,
                kind: s("kind")?.chars().take(16).collect(),
                title: s("title")?.chars().take(200).collect(),
                text,
                timeout_ms: j.get("timeout_ms")?.as_u64()?.clamp(1, DIALOG_MAX_MS),
            }
        }
        "cancel" => FromInstaller::Cancel { id: j.get("id")?.as_u64()?, reason: s("reason").unwrap_or_default() },
        "grants" => FromInstaller::Grants(grants_of(j.get("grants"))?),
        "refused" => FromInstaller::Refused { what: s("what").unwrap_or_default(), reason: s("reason").unwrap_or_default() },
        _ => return None,
    })
}

fn line(j: Json) -> String {
    let mut t = j.to_string();
    t.push('\n');
    t
}

/// The answer to `ask` `id`; `input` is "physical", "injected" or "none" (only "physical" can confirm).
pub fn answer(id: u64, yes: bool, input: &str) -> String {
    line(Json::obj(vec![("type", Json::str("answer")), ("id", Json::Num(id as f64)), ("yes", Json::Bool(yes)), ("input", Json::str(input))]))
}
/// A grant set at the real keyboard or mouse (the only way bitscomp sends one).
pub fn grant(kind: &str, until: i64) -> String {
    line(Json::obj(vec![("type", Json::str("grant")), ("kind", Json::str(kind)), ("until", Json::Num(until as f64)), ("input", Json::str("physical"))]))
}
pub fn revoke(kind: &str) -> String {
    line(Json::obj(vec![("type", Json::str("revoke")), ("kind", Json::str(kind))]))
}
/// MC-10: full access chosen in the access dialog (`id`: the question it answers; None: Georg's own).
pub fn access(id: Option<u64>, client: &str, hours: i64, nvme: bool, input: &str) -> String {
    let id = id.map_or(Json::Null, |i| Json::Num(i as f64));
    line(Json::obj(vec![("type", Json::str("access")), ("id", id), ("client", Json::str(client)), ("hours", Json::Num(hours as f64)), ("nvme", Json::Bool(nvme)), ("input", Json::str(input))]))
}

/// What the link thread hands the main loop; `u64` is the connection's number (a later one makes the older stale).
pub enum Msg {
    /// A new connection, its writing half and how its peer was accepted.
    Up(u64, UnixStream, String),
    Line(u64, Option<FromInstaller>, usize),
    Down(u64, String),
}

/// Starts the link thread for `socket` (`check`: the peer must be root running `/bin/bitsinstall`).
pub fn start(handle: &LoopHandle<'static, App>, socket: String, check: bool) -> Result<(), String> {
    let (tx, rx) = channel::channel::<Msg>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(m) = ev {
                app.install_msg(m);
            }
        })
        .map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("bitscomp-install".into())
        .spawn(move || run(&socket, check, &tx))
        .map(|_| ())
        .map_err(|e| format!("installer thread: {e}"))
}

/// The wait before the next connect: back to the minimum only after a healthy session (a hello in our version, or
/// one that lasted `HEALTHY_AFTER`), so an installer that takes the connection and drops it at once (refusing
/// bitscomp, restarting in a loop) is retried ever more slowly instead of every 200 ms.
fn next_backoff(cur: Duration, healthy: bool) -> Duration {
    if healthy { BACKOFF_MIN } else { (cur * 2).min(BACKOFF_MAX) }
}

fn run(socket: &str, check: bool, tx: &Sender<Msg>) {
    let mut backoff = BACKOFF_MIN;
    let mut last = String::new();
    let mut n = 0u64;
    loop {
        let mut healthy = false;
        let why = match UnixStream::connect(socket) {
            Err(e) => format!("not reachable at {socket}: {e}"),
            Ok(s) => match peer_ok(&s, check) {
                Err(e) => format!("peer refused: {e}"),
                Ok(how) => {
                    n += 1;
                    let began = std::time::Instant::now();
                    match session(s, n, how, tx) {
                        Err(Stop) => return,
                        Ok((why, hello)) => {
                            healthy = hello || began.elapsed() >= HEALTHY_AFTER;
                            if tx.send(Msg::Down(n, why.clone())).is_err() {
                                return;
                            }
                            if healthy { String::new() } else { format!("dropped the link at once: {why}") }
                        }
                    }
                }
            },
        };
        if healthy {
            last.clear();
        } else if why != last {
            // The same failure again is not written again (the journal keeps the first and every change).
            eprintln!("bitscomp: installer {why} (retrying)");
            last = why;
        }
        backoff = next_backoff(backoff, healthy);
        std::thread::sleep(backoff);
    }
}

struct Stop;

/// Ok(how the peer was accepted) or Err(why not).
fn peer_ok(s: &UnixStream, check: bool) -> Result<String, String> {
    if !check {
        return Ok("peer not checked (test socket)".into());
    }
    let (pid, uid) = crate::clients::peer_cred(s).ok_or("no SO_PEERCRED")?;
    if uid != 0 {
        return Err(format!("pid {pid} runs as uid {uid}, not root"));
    }
    // INT-W17 (MC-9b CR 5): the kernel names the process that holds its installer gate; when it does, that very
    // pid is the installer and no other root process. Without the file (Linux, a nested build) the checks below
    // decide, as before.
    match std::fs::read_to_string(INSTALLER_HOLDER) {
        Ok(t) => {
            return match holder_of(&t) {
                Some(Some(h)) if i64::from(h) == i64::from(pid) => Ok(format!("peer pid {pid} holds the kernel's installer gate")),
                Some(Some(h)) => Err(format!("pid {pid} is root, but pid {h} holds the kernel's installer gate")),
                Some(None) => Err(format!("pid {pid} is root, but no process holds the kernel's installer gate")),
                None => Err(format!("{INSTALLER_HOLDER}: not understood: {:?}", t.trim())),
            };
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{INSTALLER_HOLDER}: {e}")),
    }
    // bitsinstall runs with PR_SET_DUMPABLE 0, so bitscomp as uid 1000 may not read its exe link (EACCES, as
    // Linux's ptrace_may_access); root as the peer is what decides then (root is trusted with files anyway, MC-9a
    // open point 1). A readable link must name the installer. Which root process it is cannot be told from here
    // (plan/notes/wy-MC-9b.md, change request 2: the kernel names the installer gate's holder); the log says so.
    match std::fs::read_link(format!("/proc/{pid}/exe")) {
        Ok(exe) if exe != std::path::Path::new(INSTALLER) => Err(format!("pid {pid} runs {}, not {INSTALLER}", exe.display())),
        Ok(_) => Ok(format!("peer pid {pid} runs {INSTALLER}")),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Ok(format!("peer pid {pid} is root; its exe is not readable here, so root alone decided")),
        Err(e) => Err(format!("pid {pid}: {e}")),
    }
}

/// One connection: lines until it ends; Ok((why, a hello in our version came)) when it ended, Err(Stop) when the
/// main loop is gone. A hello in another version ends it: the lines would be misread.
fn session(mut s: UnixStream, n: u64, how: String, tx: &Sender<Msg>) -> Result<(String, bool), Stop> {
    let w = match s.try_clone() {
        Ok(w) => w,
        Err(e) => return Ok((format!("connection: {e}"), false)),
    };
    let _ = w.set_write_timeout(Some(WRITE_WAIT));
    let _ = s.set_read_timeout(Some(READ_WAIT));
    tx.send(Msg::Up(n, w, how)).map_err(|_| Stop)?;
    let mut hello = false;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let got = match s.read(&mut chunk) {
            Ok(0) => return Ok(("closed the connection".into(), hello)),
            Ok(k) => k,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => continue,
            Err(e) => return Ok((format!("read: {e}"), hello)),
        };
        buf.extend_from_slice(&chunk[..got]);
        while let Some(at) = buf.iter().position(|&b| b == b'\n') {
            let l: Vec<u8> = buf.drain(..=at).collect();
            let text = String::from_utf8_lossy(&l[..l.len() - 1]).into_owned();
            let m = parse(&text);
            if let Some(FromInstaller::Hello { version, .. }) = &m {
                if *version != VERSION {
                    return Ok((format!("speaks protocol version {version}, not {VERSION}"), false));
                }
                hello = true;
            }
            tx.send(Msg::Line(n, m, text.len())).map_err(|_| Stop)?;
        }
        if buf.len() > LINE_MAX {
            return Ok((format!("sent a line over {LINE_MAX} bytes"), hello));
        }
    }
}

/// Writes `line` to the installer; false when it cannot take it within the send timeout (the link is then shut).
pub fn send(w: &mut UnixStream, line: &str) -> bool {
    if w.write_all(line.as_bytes()).is_ok() {
        return true;
    }
    let _ = w.shutdown(std::net::Shutdown::Both);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_lines_of_the_kernel() {
        assert_eq!(holder_of("holder=412\n"), Some(Some(412)));
        assert_eq!(holder_of("holder=none\n"), Some(None));
        for bad in ["", "holder=", "holder=0", "holder=12 13", "pid=12", "holder=x"] {
            assert_eq!(holder_of(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_installer_lines_are_read_as_confirm_rs_writes_them() {
        // Lines as bitsinstall's confirm.rs writes them (hello, ask, cancel, grants, refused).
        let hello = r#"{"type":"hello","version":1,"grants":[{"kind":"install","until":1800000000}]}"#;
        assert_eq!(parse(hello), Some(FromInstaller::Hello { version: 1, grants: vec![Shown { kind: "install".into(), until: 1_800_000_000 }], access: vec![], clients: vec![] }));
        // MC-10: full access in hello, the access question, the access lines.
        let c = "ab".repeat(32);
        let h2 = format!(r#"{{"type":"hello","version":1,"grants":[],"access":[{{"client":"{c}","name":"bitsos-desk","until":5,"nvme":true}},{{"client":"x","until":1}}],"clients":[{{"client":"{c}","name":"bitsos-desk"}}]}}"#);
        let Some(FromInstaller::Hello { access, clients, .. }) = parse(&h2) else { panic!("hello") };
        assert_eq!(access, vec![AccessShown { client: c.clone(), name: "bitsos-desk".into(), until: 5, nvme: true }], "a bad client key is left out");
        assert_eq!(clients, vec![ClientShown { client: c.clone(), name: "bitsos-desk".into() }]);
        let q = format!(r#"{{"type":"ask","id":3,"kind":"access","title":"Vollzugriff","text":"T","timeout_ms":240000,"client":"{c}","name":"desk","hours":8}}"#);
        assert_eq!(parse(&q), Some(FromInstaller::AskAccess { id: 3, title: "Vollzugriff".into(), text: "T".into(), timeout_ms: 240_000, client: c.clone(), name: "desk".into(), hours: 8 }));
        assert_eq!(parse(r#"{"type":"ask","id":3,"kind":"access","title":"V","text":"T","timeout_ms":5,"client":"short"}"#), None, "an access question without a client key");
        assert!(matches!(parse(r#"{"type":"access","access":[],"clients":[]}"#), Some(FromInstaller::Access { .. })));
        assert_eq!(super::access(Some(3), &c, 24, false, "physical"), format!("{{\"type\":\"access\",\"id\":3,\"client\":\"{c}\",\"hours\":24,\"nvme\":false,\"input\":\"physical\"}}\n"));
        let ask = r#"{"type":"ask","id":9,"kind":"reload","title":"Kernel neu laden","text":"Zeile 1\nZeile \"2\"","timeout_ms":60000}"#;
        assert_eq!(
            parse(ask),
            Some(FromInstaller::Ask { id: 9, kind: "reload".into(), title: "Kernel neu laden".into(), text: "Zeile 1\nZeile \"2\"".into(), timeout_ms: 60_000 })
        );
        assert_eq!(parse(r#"{"type":"cancel","id":9,"reason":"timeout"}"#), Some(FromInstaller::Cancel { id: 9, reason: "timeout".into() }));
        assert_eq!(parse(r#"{"type":"grants","grants":[]}"#), Some(FromInstaller::Grants(vec![])));
        assert!(matches!(parse(r#"{"type":"refused","what":"grant","reason":"x"}"#), Some(FromInstaller::Refused { .. })));
        // A dialog never lasts longer than the installer's maximum, never 0 ms.
        let long = r#"{"type":"ask","id":1,"kind":"x","title":"t","text":"","timeout_ms":99999999}"#;
        assert!(matches!(parse(long), Some(FromInstaller::Ask { timeout_ms: DIALOG_MAX_MS, .. })));
        let zero = r#"{"type":"ask","id":1,"kind":"x","title":"t","text":"","timeout_ms":0}"#;
        assert!(matches!(parse(zero), Some(FromInstaller::Ask { timeout_ms: 1, .. })));
        // An over-long text is cut, never trusted to fit.
        let big = format!(r#"{{"type":"ask","id":1,"kind":"x","title":"t","text":"{}","timeout_ms":5}}"#, "ä".repeat(TEXT_MAX + 50));
        let Some(FromInstaller::Ask { text, .. }) = parse(&big) else { panic!("big ask") };
        assert_eq!(text.chars().count(), TEXT_MAX + 2);
        for bad in ["", "{", "[]", r#"{"type":"ask","id":-1}"#, r#"{"type":"frob"}"#, r#"{"type":"grants"}"#, r#"{"type":"cancel"}"#] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(parse(&" ".repeat(LINE_MAX + 1)), None);
    }

    fn run_session(peer: impl FnOnce(UnixStream) + Send + 'static) -> ((String, bool), Vec<Msg>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = channel::channel::<Msg>();
        let t = std::thread::spawn(move || peer(theirs));
        let r = session(ours, 1, "test".into(), &tx).ok().expect("main loop alive");
        t.join().unwrap();
        drop(tx);
        let mut got = Vec::new();
        while let Ok(m) = rx.recv() {
            got.push(m);
        }
        (r, got)
    }

    #[test]
    fn a_session_ends_on_an_over_long_line_and_on_another_version() {
        // An endless line (no newline): the session ends at LINE_MAX, nothing of it reaches the main loop.
        let ((why, hello), got) = run_session(|mut p| {
            let _ = p.write_all(&vec![b'x'; LINE_MAX + 8192]);
        });
        assert!(why.contains("over") && !hello, "{why}");
        assert!(matches!(got.as_slice(), [Msg::Up(1, _, _)]));
        // A hello in another version ends it before anything is read as ours.
        let ((why, hello), got) = run_session(|mut p| {
            let _ = p.write_all(b"{\"type\":\"hello\",\"version\":2,\"grants\":[]}\n{\"type\":\"ask\",\"id\":1,\"kind\":\"x\",\"title\":\"t\",\"text\":\"\",\"timeout_ms\":5}\n");
        });
        assert!(why.contains("version 2") && !hello, "{why}");
        assert_eq!(got.len(), 1);
        // Our version: healthy; a bad line in between is handed on as not understood.
        let ((why, hello), got) = run_session(|mut p| {
            let _ = p.write_all(b"{\"type\":\"hello\",\"version\":1,\"grants\":[]}\n{not json\n");
        });
        assert!(why.contains("closed") && hello, "{why}");
        assert!(matches!(got.as_slice(), [Msg::Up(..), Msg::Line(1, Some(FromInstaller::Hello { .. }), _), Msg::Line(1, None, 9)]));
    }

    #[test]
    fn the_back_off_starts_over_only_after_a_healthy_session() {
        let mut b = BACKOFF_MIN;
        for _ in 0..10 {
            b = next_backoff(b, false);
        }
        assert_eq!(b, BACKOFF_MAX, "an installer dropping the link at once is retried ever more slowly");
        assert_eq!(next_backoff(b, true), BACKOFF_MIN);
    }

    #[test]
    fn bitscomps_lines_are_what_confirm_rs_parses() {
        assert_eq!(answer(5, true, "physical"), "{\"type\":\"answer\",\"id\":5,\"yes\":true,\"input\":\"physical\"}\n");
        assert_eq!(answer(5, true, "injected"), "{\"type\":\"answer\",\"id\":5,\"yes\":true,\"input\":\"injected\"}\n");
        assert_eq!(grant("install", 1_800_000_000), "{\"type\":\"grant\",\"kind\":\"install\",\"until\":1800000000,\"input\":\"physical\"}\n");
        assert_eq!(revoke("all"), "{\"type\":\"revoke\",\"kind\":\"all\"}\n");
    }
}
