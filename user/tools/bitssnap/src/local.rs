//! REWIND: the desktop's way to the snapshots (bitscomp's "Zurückspulen", uid 1000). `/dev/bitssnap` is root's,
//! so `bitssnap serve` (root, started by init) answers a local socket with exactly six operations, one JSON line
//! each way: `list`, `browse` (a snapshot's folders, read-only), `restore-path`, `restore-program`,
//! `restore-system` and `undo` (of a restore-system before the next start).
//!
//! Who may ask: root and the desktop's user (uid 1000), anybody else is refused before anything is looked at.
//! What may change something: a request with `"confirmed": true` from a peer whose yes is Georg's own, by the rule
//! settingsd uses for `confirmed` (settingsd::vouch::judge: the screen's compositor, a build of bitscomp holding
//! the grant, or root). Any other program of his user can list, browse and ask for a dry run (`"dry_run": true`,
//! which changes nothing), never restore. So a program MCP's `run` starts (uid 1000) gets no new way to restore
//! without him; MCP's own `restore` tool stays the one it was (safety class delete_overwrite, B3).
//!
//! For uid 1000 a snapshot is only as open as /data was then: `browse` and `restore-path` walk only folders the
//! user could list (r and x for him by owner, group or others), so a root-only folder's names stay hidden.
//! A restore-path of uid 1000 stays in its own folder (/data/home/1000) and touches only what it owns there.
//! Every restore and every refusal of one goes to the kernel log, the asker's words quoted and escaped.

use crate::log;
use mcpreg::Json;
use settingsd::peer::Peer;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SOCKET: &str = "/run/bitsos/bitssnap.sock";
/// The desktop's user (bitscomp runs as this uid, `drop_root::DESKTOP`).
pub const OWNER: u32 = 1000;
/// A request line is short: anything longer is not one of ours.
const LINE_MAX: u64 = 64 << 10;
/// Connections served at once per pool (the desktop needs one or two).
const OPEN_MAX: usize = 8;
static OPEN: AtomicUsize = AtomicUsize::new(0);
static OPEN_VOUCHED: AtomicUsize = AtomicUsize::new(0);
/// Connections accepted whose program is still being looked up (a moment each).
const SETTLING_MAX: usize = 16;
static SETTLING: AtomicUsize = AtomicUsize::new(0);
/// A request line arrives whole within this time, or the connection is closed (idle ones too).
const LINE_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    List,
    Browse,
    RestorePath,
    RestoreProgram,
    RestoreSystem,
    Undo,
}

impl Op {
    pub fn of(s: &str) -> Option<Op> {
        Some(match s {
            "list" => Op::List,
            "browse" => Op::Browse,
            "restore-path" => Op::RestorePath,
            "restore-program" => Op::RestoreProgram,
            "restore-system" => Op::RestoreSystem,
            "undo" => Op::Undo,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Op::List => "list",
            Op::Browse => "browse",
            Op::RestorePath => "restore-path",
            Op::RestoreProgram => "restore-program",
            Op::RestoreSystem => "restore-system",
            Op::Undo => "undo",
        }
    }
}

/// What the operations do, behind a trait so the rules above are tested without a pool. `uid` is the asker's
/// (0 sees everything).
pub trait Ops {
    fn list(&mut self) -> Result<Json, String>;
    fn browse(&mut self, id: u64, path: &str, uid: u32) -> Result<Json, String>;
    fn programs(&mut self, id: u64) -> Result<Json, String>;
    fn restore_path(&mut self, id: u64, path: &str, uid: u32, dry: bool) -> Result<Json, String>;
    fn restore_program(&mut self, id: u64, name: &str, dry: bool) -> Result<Json, String>;
    fn restore_system(&mut self, at: i64, dry: bool) -> Result<Json, String>;
    fn undo(&mut self) -> Result<Json, String>;
}

fn refuse(code: &str, detail: impl Into<String>) -> Json {
    Json::obj(vec![("ok", Json::Bool(false)), ("error", Json::str(code)), ("detail", Json::str(&detail.into()))])
}

fn with_ok(r: Result<Json, String>) -> Json {
    match r {
        Ok(Json::Obj(mut v)) => {
            v.insert(0, ("ok".into(), Json::Bool(true)));
            Json::Obj(v)
        }
        Ok(other) => Json::obj(vec![("ok", Json::Bool(true)), ("value", other)]),
        Err(e) => refuse("failed", e),
    }
}

/// A line for the kernel log. Audits (a confirmed change and how it ended) always go there; refusals, which any
/// program of the user can cause as often as it likes, only up to a rate (`say`).
#[derive(Debug, PartialEq, Eq)]
pub enum Said {
    Audit(String),
    Refusal(String),
    /// What a dry run looked at (REWIND stutter): rate-limited as refusals, a program could ask without end.
    Note(String),
}

/// A field the asker chose, as it goes into a log line: quoted with its control characters escaped (a `\n`
/// would start a record of its own on /dev/kmsg), and short.
pub fn q(s: &str) -> String {
    const MAX: usize = 96;
    match s.char_indices().nth(MAX) {
        Some((i, _)) => format!("{:?}…", &s[..i]),
        None => format!("{s:?}"),
    }
}

/// The desktop user's own folder below /data, the only place a restore-path of uid `uid` (not root) may touch.
pub fn yours(rel: &str, uid: u32) -> bool {
    let home = format!("home/{uid}");
    uid == 0 || rel == home || rel.strip_prefix(&home).is_some_and(|r| r.starts_with('/'))
}

/// The answer to one request line of `peer` and what goes into the log for it. `judge` says whether the peer's
/// `confirmed` is Georg's own yes (Ok: who it is, for the log).
pub fn answer(line: &str, peer: &Peer, judge: &dyn Fn(&Peer) -> Result<String, String>, ops: &mut dyn Ops) -> (Json, Vec<Said>) {
    let shown = format!("pid={} uid={} exe={}", peer.pid, peer.uid.map_or("-".into(), |u| u.to_string()), peer.exe.as_deref().map_or("-".into(), q));
    let uid = match peer.uid {
        Some(u) if u == 0 || u == OWNER => u,
        _ => {
            let said = Said::Refusal(format!("desktop socket: refused {shown}: only root and uid {OWNER} may ask"));
            return (refuse("not_owner", format!("only root and uid {OWNER} may ask")), vec![said]);
        }
    };
    let Ok(req @ Json::Obj(_)) = Json::parse(line.trim()) else { return (refuse("bad_request", "not a JSON object"), Vec::new()) };
    let Some(op) = req.get("op").and_then(Json::as_str).and_then(Op::of) else {
        return (refuse("bad_request", "op: list, browse, restore-path, restore-program, restore-system or undo"), Vec::new());
    };
    let id = req.get("snapshot").and_then(Json::as_u64);
    let dry = req.get("dry_run").and_then(Json::as_bool).unwrap_or(false);
    let changes = matches!(op, Op::RestorePath | Op::RestoreProgram | Op::RestoreSystem | Op::Undo) && !(dry && op != Op::Undo);
    // The arguments first: a malformed request is refused for what it is, whoever sends it, and before anything
    // of it reaches the log.
    let path = req.get("path").and_then(Json::as_str);
    let program = req.get("program").and_then(Json::as_str);
    let at = match req.get("at") {
        Some(j) => j.as_i64().filter(|&t| t > 0),
        None => None,
    };
    let missing = match op {
        Op::Browse if id.is_none() => Some("want snapshot (an id)".to_string()),
        Op::Browse if path.is_none() && req.get("programs").and_then(Json::as_bool) != Some(true) => Some("want path (below /data) or programs=true".into()),
        Op::RestorePath if id.is_none() || path.is_none() => Some("want snapshot (an id) and path (below /data)".into()),
        Op::RestoreProgram if id.is_none() || program.is_none() => Some("want snapshot (an id) and program (a package name)".into()),
        Op::RestoreSystem if at.is_none() => Some("want at (UTC seconds > 0)".into()),
        _ if path.is_some_and(|p| p.chars().any(char::is_control)) => Some("path: no control characters".into()),
        Op::Browse | Op::RestorePath => match path {
            Some(p) if op == Op::Browse && p.trim_end_matches('/') == crate::restore::DATA => None,
            Some(p) => crate::restore::below_data(p).err(),
            None => None,
        },
        Op::RestoreProgram => program.and_then(|n| bitspkg::names::check_name(n).err()),
        _ => None,
    };
    if let Some(m) = missing {
        return (refuse("bad_request", m), Vec::new());
    }
    let mut said = Vec::new();
    let what = match op {
        Op::RestorePath => format!("restore of {} from snapshot {}", q(path.unwrap_or("")), id.unwrap_or(0)),
        Op::RestoreProgram => format!("restore of program {} from snapshot {}", q(program.unwrap_or("")), id.unwrap_or(0)),
        Op::RestoreSystem => format!("everything as at {}", at.unwrap_or(0)),
        _ => "undo of the restore-at of this start".to_string(),
    };
    if op == Op::RestorePath && !path.and_then(|p| crate::restore::below_data(p).ok()).is_some_and(|rel| yours(&rel, uid)) {
        if changes {
            said.push(Said::Refusal(format!("desktop socket: {what} refused for {shown}: not in its own folder")));
        }
        return (refuse("not_yours", format!("uid {uid} restores only in its own folder /data/home/{uid}")), said);
    }
    if changes {
        if req.get("confirmed").and_then(Json::as_bool) != Some(true) {
            said.push(Said::Refusal(format!("desktop socket: {what} refused for {shown}: not confirmed")));
            return (refuse("needs_confirm", "a change needs \"confirmed\": true, given by Georg in the desktop"), said);
        }
        match judge(peer) {
            Ok(who) => said.push(Said::Audit(format!("desktop socket: {what} confirmed by {}", q(&who)))),
            Err(why) => {
                said.push(Said::Refusal(format!("desktop socket: {what} refused for {shown}: its confirmation does not count ({})", q(&why))));
                return (refuse("not_confirmer", format!("only the desktop's own confirmation counts ({why})")), said);
            }
        }
    }
    let (id, path, program, at) = (id.unwrap_or(0), path.unwrap_or(""), program.unwrap_or(""), at.unwrap_or(0));
    let r = match op {
        Op::List => ops.list(),
        Op::Browse if path.is_empty() => ops.programs(id),
        Op::Browse => ops.browse(id, path, uid),
        Op::RestorePath => ops.restore_path(id, path, uid, dry),
        Op::RestoreProgram => ops.restore_program(id, program, dry),
        Op::RestoreSystem => ops.restore_system(at, dry),
        Op::Undo => ops.undo(),
    };
    if changes {
        said.push(Said::Audit(match &r {
            Ok(_) => format!("desktop socket: {what}: done"),
            Err(e) => format!("desktop socket: {what}: failed: {}", q(e)),
        }));
    }
    (with_ok(r), said)
}

/// Refusal lines per minute that reach the log; the rest are counted in one line.
const REFUSALS_PER_MIN: u32 = 20;
/// (minute, lines of it logged, lines of it not logged).
static QUIET: Mutex<(u64, u32, u32)> = Mutex::new((0, 0, 0));
static T0: OnceLock<Instant> = OnceLock::new();

/// A /dev/kmsg record holds 240 bytes; a longer line would go on in a record whose start the asker chose.
fn clip(m: &str) -> String {
    const MAX: usize = 225;
    if m.len() <= MAX {
        return m.to_string();
    }
    let mut i = MAX;
    while !m.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &m[..i])
}

fn say(lines: &[Said]) {
    for l in lines {
        match l {
            Said::Audit(m) => log(&clip(m)),
            Said::Refusal(m) | Said::Note(m) => {
                let minute = T0.get_or_init(Instant::now).elapsed().as_secs() / 60;
                let Ok(mut qt) = QUIET.lock() else { continue };
                if qt.0 != minute {
                    if qt.2 > 0 {
                        log(&format!("desktop socket: {} more refusals in the minute before not logged", qt.2));
                    }
                    *qt = (minute, 0, 0);
                }
                if qt.1 < REFUSALS_PER_MIN {
                    qt.1 += 1;
                    log(&clip(m));
                } else {
                    qt.2 += 1;
                }
            }
        }
    }
}

/// Serves the socket at `path` from a thread of its own; each connection gets a thread (a restore copying a big
/// folder holds up nobody else's list).
pub fn start(path: &str) -> Result<(), String> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).map_err(|e| format!("{path}: {e}"))?;
    // Everybody may connect; who may ask is the peer check's (at accept, then `answer`).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).map_err(|e| format!("{path}: chmod: {e}"))?;
    let sysdir = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::PathBuf::from));
    std::thread::Builder::new()
        .name("bitssnap-desk".into())
        .spawn(move || {
            for s in l.incoming().flatten() {
                let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
                // The kernel's record of who connected, taken right at accept (settingsd's two steps): anybody but
                // root and the desktop's user is turned away before it holds a thread or a slot.
                let seen = settingsd::peer::seen(&s);
                if !matches!(seen.uid(), Some(0) | Some(OWNER)) {
                    let uid = seen.uid().map_or("-".into(), |u| u.to_string());
                    say(&[Said::Refusal(format!("desktop socket: refused pid={} uid={uid}: only root and uid {OWNER} may ask", seen.pid()))]);
                    let _ = (&s).write_all(format!("{}\n", refuse("not_owner", format!("only root and uid {OWNER} may ask"))).as_bytes());
                    continue;
                }
                if !take(&SETTLING, SETTLING_MAX) {
                    let _ = (&s).write_all(format!("{}\n", refuse("busy", "too many connections")).as_bytes());
                    continue;
                }
                let sysdir = sysdir.clone();
                let spawned = std::thread::Builder::new().name("bitssnap-desk-1".into()).spawn(move || {
                    let peer = seen.settle();
                    SETTLING.fetch_sub(1, Ordering::SeqCst);
                    let judge = |p: &Peer| settingsd::vouch::judge(p, sysdir.as_deref(), &settingsd::vouch::holder_file);
                    // A program that opens connections without end holds snapshots mounted (retention waits for
                    // them): a few at a time, the rest told so and closed. The compositor's (and root's) have a
                    // pool of their own, so no other program of the user can keep Georg's view out.
                    let pool = if judge(&peer).is_ok() { &OPEN_VOUCHED } else { &OPEN };
                    if !take(pool, OPEN_MAX) {
                        let _ = (&s).write_all(format!("{}\n", refuse("busy", "too many connections")).as_bytes());
                        return;
                    }
                    serve_one(s, peer, &judge);
                    pool.fetch_sub(1, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    SETTLING.fetch_sub(1, Ordering::SeqCst);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    log(&format!("desktop socket {path} ready (list, browse, restore-path, restore-program, restore-system, undo)"));
    Ok(())
}

/// One more of `n` when fewer than `max` are taken.
fn take(n: &AtomicUsize, max: usize) -> bool {
    if n.fetch_add(1, Ordering::SeqCst) >= max {
        n.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    true
}

/// The next request line, whole within `wait` from when the wait for it began (a byte now and then does not keep
/// the connection); None at the end, on a timeout or a read error, Err for a line too long.
fn next_line(s: &UnixStream, rd: &mut BufReader<UnixStream>, wait: Duration) -> Option<Result<Vec<u8>, ()>> {
    let start = Instant::now();
    let mut buf = Vec::new();
    loop {
        let left = wait.checked_sub(start.elapsed()).filter(|d| !d.is_zero())?;
        s.set_read_timeout(Some(left)).ok()?;
        let got = match rd.fill_buf() {
            Ok([]) | Err(_) => return None,
            Ok(b) => b,
        };
        if let Some(i) = got.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&got[..=i]);
            rd.consume(i + 1);
            return Some(if buf.len() as u64 > LINE_MAX { Err(()) } else { Ok(buf) });
        }
        let n = got.len();
        buf.extend_from_slice(got);
        rd.consume(n);
        if buf.len() as u64 > LINE_MAX {
            return Some(Err(()));
        }
    }
}

fn serve_one(s: UnixStream, peer: Peer, judge: &dyn Fn(&Peer) -> Result<String, String>) {
    let Ok(rd) = s.try_clone() else { return };
    let mut rd = BufReader::new(rd);
    let mut ops = crate::local_ops::DevOps;
    loop {
        let buf = match next_line(&s, &mut rd, LINE_WAIT) {
            None => return,
            Some(Err(())) => {
                let _ = (&s).write_all(format!("{}\n", refuse("bad_request", "line too long")).as_bytes());
                return;
            }
            Some(Ok(b)) => b,
        };
        let t0 = Instant::now();
        let (a, mut said) = answer(&String::from_utf8_lossy(&buf), &peer, judge, &mut ops);
        let seen = crate::local_ops::seen(&a);
        if seen > 0 {
            let path = a.get("path").or(a.get("program")).and_then(Json::as_str).unwrap_or("");
            said.push(Said::Note(format!("desktop socket: dry run of {} walked {seen} entries in {} ms", q(path), t0.elapsed().as_millis())));
        }
        say(&said);
        if (&s).write_all(format!("{a}\n").as_bytes()).is_err() {
            return;
        }
    }
}

/// `bitssnap ask JSON`: one request to the socket, the answer printed; Ok(true) when it says ok.
pub fn ask(socket: &str, line: &str) -> Result<(bool, String), String> {
    let s = UnixStream::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(600)));
    // A refusal at accept (another uid, busy) is written before our line is read and the socket closed: the
    // write then fails, the answer is still there to read.
    let wrote = (&s).write_all(format!("{}\n", line.trim()).as_bytes());
    let mut out = String::new();
    let read = BufReader::new(&s).take(16 << 20).read_line(&mut out);
    if out.is_empty() {
        wrote.map_err(|e| e.to_string())?;
        read.map_err(|e| e.to_string())?;
    }
    let ok = Json::parse(out.trim()).ok().and_then(|j| j.get("ok").and_then(Json::as_bool)) == Some(true);
    Ok((ok, out.trim_end().to_string()))
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;
