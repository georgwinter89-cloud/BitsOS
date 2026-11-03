//! Brave as brave-act runs it. Decision (AC-3): brave-act starts Brave itself, through the stick's launcher
//! (`brave -- --remote-debugging-pipe`: uid 1000, the tree, the profile, the desktop, as every start), with the
//! pipe on Brave's fds 3 and 4. A Brave that is already running without the pipe (started from the tile or the
//! shell) cannot be attached to: Chromium takes the pipe only at its start. Then open_url still hands its URL to
//! that Brave (the launcher's forward), the other actions say why they cannot. Brave ends when the pipe closes
//! (Chromium's rule): brave-act is the long-running owner of the Brave it started.
//!
//! The fds: std's Command puts the pipe ends on the child's stdin and stdout; a `sh` trampoline moves them to 3
//! and 4 (`exec 3<&0 4>&1`, dup2 without close-on-exec) and points stdin/stdout at /dev/null and the log, so no
//! unsafe pre_exec is needed. brave-act's own ends are close-on-exec, so Brave sees EOF when brave-act ends.
//!
//! The launcher runs in a process group of its own. Brave ends on the pipe's EOF; if it does not (hung), brave-act
//! sends SIGTERM to the launcher (its trap ends Brave cleanly and packs a tar profile), then SIGKILL to the group.

use crate::cdp::Conn;
use bitsact::events::{Events, Level, q};
use mcpreg::Json;
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const TRAMPOLINE: &str = r#"exec 3<&0 4>&1 </dev/null >>"$1" 2>&1; shift; exec "$@""#;
/// Brave's first answer on the pipe: a cold start in QEMU from a FAT stick takes about a minute (BR-3).
pub const START_LIMIT: Duration = Duration::from_secs(240);
/// How long Brave has to end by itself after its pipe closed, and then after SIGTERM (the launcher's own trap
/// waits 10 s before it kills Brave).
const END_GRACE: Duration = Duration::from_secs(5);
const TERM_GRACE: Duration = Duration::from_secs(15);

pub struct Settings {
    /// The launcher's command (`/bin/sh /boot/BITSOS2/bin/brave`: run by sh, the stick's FAT has no x bits).
    pub launcher: Vec<String>,
    /// The tree whose `brave` is the browser (to find a Brave that runs without brave-act).
    pub tree: PathBuf,
    /// Brave arguments after the launcher's own (tests: host resolver rules).
    pub extra: Vec<String>,
    /// The launcher's output (its `brave:` lines) and the trampoline's.
    pub log: PathBuf,
    /// Environment for the launcher (PATH; on the host also the BRAVE_* overrides of the tests).
    pub env: Vec<(String, String)>,
}

/// Starts are numbered: a failed start is answered to the calls that waited for it; a later call starts afresh.
enum State {
    Idle,
    Starting(u64),
    Up(Arc<Conn>),
    Failed(String, u64),
}

/// The launcher brave-act started (pid = its process group) and whether it has ended.
struct Launched {
    pid: u32,
    ended: Arc<AtomicBool>,
}

pub struct Browser {
    pub cfg: Settings,
    state: Mutex<State>,
    cv: Condvar,
    ev: Events,
    starts: AtomicU64,
    child: Mutex<Option<Launched>>,
}

pub enum Want<'a> {
    /// Brave as it starts (the start page or the restored tabs).
    Plain,
    /// A start opens this URL in its first tab.
    Url(&'a str),
}

pub enum Got {
    Up { conn: Arc<Conn>, started_now: bool },
    /// A Brave without the pipe runs (pid).
    Foreign(u32),
}

impl Browser {
    pub fn new(cfg: Settings, ev: Events) -> Arc<Browser> {
        Arc::new(Browser { cfg, state: Mutex::new(State::Idle), cv: Condvar::new(), ev, starts: AtomicU64::new(0), child: Mutex::new(None) })
    }

    /// The connection, starting Brave when none runs; waits until `deadline` for a start (the start goes on
    /// after it: the next call finds it up).
    pub fn get(self: &Arc<Browser>, want: Want<'_>, deadline: Instant) -> Result<Got, String> {
        let mut st = self.state.lock().map_err(|_| "state poisoned")?;
        let mut started_now = false;
        // The start this call waited for (or began): its failure is this call's answer.
        let mut waited: Option<u64> = None;
        loop {
            match &*st {
                State::Up(c) if c.alive() && !self.launcher_ended() => return Ok(Got::Up { conn: c.clone(), started_now }),
                State::Up(c) => {
                    // Brave's helpers may hold the pipe a moment after Brave itself ended: its launcher's end counts.
                    c.close("Brave's launcher ended");
                    self.ev.emit(Level::Info, "brave.gone", &format!("why={}", q(&c.why())));
                    *st = State::Idle;
                }
                State::Failed(why, g) if waited == Some(*g) => return Err(why.clone()),
                State::Failed(..) | State::Idle => {
                    // A start that failed before this call came is not its answer: it tries afresh (the old
                    // Failed stays readable for the calls that waited for it until this start replaces it).
                    if let Some(pid) = foreign(&self.cfg.tree) {
                        return Ok(Got::Foreign(pid));
                    }
                    let g = self.starts.fetch_add(1, Ordering::Relaxed) + 1;
                    let before = std::mem::replace(&mut *st, State::Starting(g));
                    waited = Some(g);
                    started_now = true;
                    let (b, url) = (self.clone(), match want { Want::Url(u) => Some(u.to_string()), Want::Plain => None });
                    let spawned = std::thread::Builder::new().name("brave-start".into()).spawn(move || {
                        let r = b.start(url.as_deref());
                        if let Ok(mut s) = b.state.lock() {
                            *s = match r {
                                Ok(c) => State::Up(c),
                                Err(e) => State::Failed(e, g),
                            };
                        }
                        b.cv.notify_all();
                    });
                    if let Err(e) = spawned {
                        *st = before;
                        return Err(format!("no thread to start Brave: {e}"));
                    }
                    continue;
                }
                State::Starting(g) => {
                    waited = Some(*g);
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err("Brave is still starting (a cold start takes up to a few minutes); call again".into());
                    }
                    st = self.cv.wait_timeout(st, left).map_err(|_| "state poisoned")?.0;
                }
            }
        }
    }

    fn start(&self, url: Option<&str>) -> Result<Arc<Conn>, String> {
        // A Brave of an earlier start whose pipe ended but that did not end itself holds the profile.
        self.stop_launched("an earlier start");
        let (cmd_r, cmd_w) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
        let (ev_r, ev_w) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
        let mut args: Vec<String> = url.map(|u| vec![u.to_string()]).unwrap_or_default();
        args.push("--".into());
        args.push("--remote-debugging-pipe".into());
        args.extend(self.cfg.extra.iter().cloned());
        let log_start = std::fs::metadata(&self.cfg.log).map(|m| m.len()).unwrap_or(0);
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(TRAMPOLINE).arg("brave-act").arg(&self.cfg.log).args(&self.cfg.launcher).args(&args);
        c.stdin(Stdio::from(cmd_r)).stdout(Stdio::from(ev_w)).stderr(Stdio::null()).process_group(0);
        for (k, v) in &self.cfg.env {
            c.env(k, v);
        }
        let child = c.spawn().map_err(|e| format!("cannot run {}: {e}", self.cfg.launcher.join(" ")))?;
        // The child holds the other ends now; ours are dropped with `c`.
        drop(c);
        let pid = child.id();
        self.ev.emit(Level::Info, "brave.start", &format!("launcher={} pid={pid} url={}", q(&self.cfg.launcher.join(" ")), q(url.unwrap_or(""))));
        let ended = reap(child, self.ev.clone(), "brave.ended");
        if let Ok(mut l) = self.child.lock() {
            *l = Some(Launched { pid, ended });
        }
        let conn = Conn::new(ev_r, cmd_w, self.ev.clone());
        let t0 = Instant::now();
        match conn.call("Browser.getVersion", Json::Obj(Vec::new()), None, t0 + START_LIMIT) {
            Ok(v) => {
                let product = v.get("product").and_then(Json::as_str).unwrap_or("?").to_string();
                self.ev.emit(Level::Info, "brave.up", &format!("pid={pid} product={} ms={}", q(&product), t0.elapsed().as_millis()));
                Ok(conn)
            }
            Err(e) => {
                conn.close(&format!("the start failed: {e}"));
                self.stop_launched("a failed start");
                let tail = log_tail(&self.cfg.log, log_start);
                self.ev.emit(Level::Warn, "brave.start_failed", &format!("why={} log={}", q(&e), q(&tail)));
                if tail.contains("goes to it") {
                    return Err(format!(
                        "a Brave without the DevTools pipe runs already (the launcher handed over to it: {tail}); close it, then brave-act starts its own"
                    ));
                }
                Err(format!("Brave did not come up with its DevTools pipe ({e}); the launcher said: {tail}"))
            }
        }
    }

    /// Runs the launcher without the pipe: hands `url` to a running Brave (Chromium's singleton).
    pub fn forward(&self, url: &str, deadline: Instant) -> Result<String, String> {
        let log_start = std::fs::metadata(&self.cfg.log).map(|m| m.len()).unwrap_or(0);
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(r#"exec </dev/null >>"$1" 2>&1; shift; exec "$@""#).arg("brave-act").arg(&self.cfg.log).args(&self.cfg.launcher).arg(url);
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for (k, v) in &self.cfg.env {
            c.env(k, v);
        }
        let mut child = c.spawn().map_err(|e| format!("cannot run {}: {e}", self.cfg.launcher.join(" ")))?;
        loop {
            if let Ok(Some(s)) = child.try_wait() {
                let tail = log_tail(&self.cfg.log, log_start);
                return if s.success() { Ok(tail) } else { Err(format!("the launcher ended with {s}: {tail}")) };
            }
            if Instant::now() >= deadline {
                // Not killed: it may be the start of a Brave after all; it ends by itself and is reaped then.
                reap(child, self.ev.clone(), "forward.ended");
                return Err("the launcher did not finish within the call's time".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn launcher_ended(&self) -> bool {
        self.child.lock().ok().is_some_and(|c| c.as_ref().is_some_and(|l| l.ended.load(Ordering::Acquire)))
    }

    /// brave-act's end: the pipe closes, then Brave gets the same fallback as after a failed start.
    pub fn shutdown(&self) {
        if let Ok(mut st) = self.state.lock()
            && let State::Up(c) = std::mem::replace(&mut *st, State::Idle)
        {
            c.close("brave-act ends");
        }
        self.stop_launched("brave-act's end");
    }

    /// Ends the launcher brave-act started, if it still runs: first its own end on the pipe's EOF, then SIGTERM to
    /// the launcher (its trap ends Brave cleanly), then SIGKILL to its process group. Bounded: at most
    /// END_GRACE + TERM_GRACE + 2 s.
    fn stop_launched(&self, why: &str) {
        let Some(l) = self.child.lock().ok().and_then(|mut c| c.take()) else { return };
        let gone = |t: Duration| {
            let end = Instant::now() + t;
            while !l.ended.load(Ordering::Acquire) {
                if Instant::now() >= end {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            true
        };
        if gone(END_GRACE) {
            return;
        }
        self.ev.emit(Level::Warn, "brave.term", &format!("pid={} why={}", l.pid, q(why)));
        signal("-TERM", &l.pid.to_string());
        if gone(TERM_GRACE) {
            return;
        }
        self.ev.emit(Level::Warn, "brave.kill", &format!("pgid={} why={}", l.pid, q(why)));
        signal("-KILL", &format!("-{}", l.pid));
        if !gone(Duration::from_secs(2)) {
            self.ev.emit(Level::Error, "brave.kill_failed", &format!("pgid={}", l.pid));
        }
    }
}

/// Waits for `child` on a thread of its own (no zombie stays behind); the flag says it ended.
fn reap(mut child: Child, ev: Events, what: &'static str) -> Arc<AtomicBool> {
    let ended = Arc::new(AtomicBool::new(false));
    let (e2, pid) = (ended.clone(), child.id());
    let r = std::thread::Builder::new().name("brave-wait".into()).spawn(move || {
        let code = child.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
        e2.store(true, Ordering::Release);
        ev.emit(Level::Info, what, &format!("pid={pid} status={}", q(&code)));
    });
    if let Err(e) = r {
        // No thread: the flag stays false, so stop_launched signals the launcher rather than leave it behind.
        ev_none(&e);
    }
    ended
}

fn ev_none(e: &std::io::Error) {
    eprintln!("brave-act: no thread to wait for the launcher: {e}");
}

/// kill(1) (BusyBox's in BitsOS, the host's in the tests): `-- -PGID` is a process group (kill(2) with pid < 0).
fn signal(sig: &str, target: &str) {
    let _ = Command::new("kill").args([sig, "--", target]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// The last lines the launcher wrote since `from` (one line, for answers and events).
fn log_tail(log: &Path, from: u64) -> String {
    let b = std::fs::read(log).unwrap_or_default();
    let b = b.get(from as usize..).unwrap_or(&b[..]);
    let t = String::from_utf8_lossy(b);
    let lines: Vec<&str> = t.lines().filter(|l| l.starts_with("brave:")).collect();
    let tail = lines[lines.len().saturating_sub(4)..].join(" | ");
    tail.chars().take(600).collect()
}

/// A browser process (not a helper: no `--type=`) of `tree`'s brave, if one runs. Chromium rewrites its argv
/// into one string with spaces (its process title), so spaces split words as NULs do (as the launcher's
/// `running`).
pub fn foreign(tree: &Path) -> Option<u32> {
    let exe = tree.join("brave");
    let exe = exe.to_string_lossy();
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else { continue };
        let cmd = String::from_utf8_lossy(&cmd);
        let mut words = cmd.split(['\0', ' ']).filter(|w| !w.is_empty());
        if words.next() != Some(&*exe) {
            continue;
        }
        if !words.any(|w| w.starts_with("--type=")) && pid != std::process::id() {
            return Some(pid);
        }
    }
    None
}
