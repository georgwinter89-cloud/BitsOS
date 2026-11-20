//! Brave's remote debugging protocol on a pipe (Chromium's `--remote-debugging-pipe`, as brave-act uses it):
//! Brave reads commands on its fd 3 and writes answers and events on its fd 4, each message one JSON text ended
//! by a NUL byte. A `sh` trampoline moves the child's stdin/stdout to 3 and 4 (no unsafe pre_exec). One reader
//! thread hands every message to the caller's queue; the caller waits for an answer by id or an event by name, in
//! short slices, asking `stop` between them (input during a long wait ends it at once).

use mcpreg::Json;
use std::io::{BufRead, BufReader, Read, Write};
use crate::group::{Group, Kids};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const TRAMPOLINE: &str = r#"exec 3<&0 4>&1 </dev/null >>"$1" 2>&1; shift; exec "$@""#;
/// Longest message taken (a screenshot is never asked for; page answers are small).
const MAX_MSG: u64 = 16 << 20;
/// The longest a wait goes without asking `stop`.
const SLICE: Duration = Duration::from_millis(250);

pub struct Browser {
    /// Ended with its process group when the browser is dropped.
    pub child: Group,
    w: ChildStdin,
    rx: Receiver<Json>,
    next: u64,
    /// Events that came while waiting for an answer.
    kept: Vec<Json>,
}

impl Browser {
    /// Starts `cmd` with the pipe on its fds 3 and 4, its output appended to `log`, in a process group of its own.
    pub fn start(cmd: &[String], log: &std::path::Path, kids: &Kids) -> Result<Browser, String> {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(TRAMPOLINE).arg("sh").arg(log).args(cmd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = Group::spawn(&mut c, kids).map_err(|e| format!("{}: {e}", cmd.first().map_or("", String::as_str)))?;
        let (w, r) = (child.child().stdin.take().ok_or("no pipe")?, child.child().stdout.take().ok_or("no pipe")?);
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("bitsbench-cdp".into())
            .spawn(move || {
                let mut r = BufReader::new(r);
                loop {
                    let mut msg = Vec::new();
                    match (&mut r).take(MAX_MSG).read_until(0, &mut msg) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    if msg.last() == Some(&0) {
                        msg.pop();
                    } else if msg.len() as u64 >= MAX_MSG {
                        return;
                    }
                    if let Ok(j) = Json::parse(&String::from_utf8_lossy(&msg)) {
                        if tx.send(j).is_err() {
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Browser { child, w, rx, next: 1, kept: Vec::new() })
    }

    /// The next message by `deadline`, asking `stop` at least every SLICE.
    fn recv(&self, what: &str, deadline: Instant, stop: &dyn Fn() -> bool) -> Result<Json, String> {
        loop {
            if stop() {
                return Err(format!("{what}: stopped"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{what}: not in time"));
            }
            match self.rx.recv_timeout(left.min(SLICE)) {
                Ok(m) => return Ok(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(format!("{what}: the browser closed its pipe")),
            }
        }
    }

    /// Sends a command; Ok(result) or Err(why) by `deadline` (or as soon as `stop` says so).
    pub fn call(&mut self, method: &str, params: Json, session: Option<&str>, deadline: Instant, stop: &dyn Fn() -> bool) -> Result<Json, String> {
        let id = self.next;
        self.next += 1;
        let mut pairs = vec![("id", Json::Num(id as f64)), ("method", Json::str(method)), ("params", params)];
        if let Some(s) = session {
            pairs.push(("sessionId", Json::str(s)));
        }
        let mut text = Json::obj(pairs).to_string().into_bytes();
        text.push(0);
        self.w.write_all(&text).and_then(|_| self.w.flush()).map_err(|e| format!("{method}: {e}"))?;
        loop {
            let m = self.recv(method, deadline, stop)?;
            if m.get("id").and_then(Json::as_u64) == Some(id) {
                if let Some(e) = m.get("error") {
                    return Err(format!("{method}: {e}"));
                }
                return Ok(m.get("result").cloned().unwrap_or(Json::Null));
            }
            if m.get("method").is_some() {
                self.kept.push(m);
                self.kept.drain(..self.kept.len().saturating_sub(256));
            }
        }
    }

    /// Waits for event `method` (one that came earlier counts) by `deadline` (or as soon as `stop` says so).
    pub fn event(&mut self, method: &str, deadline: Instant, stop: &dyn Fn() -> bool) -> Result<Json, String> {
        if let Some(i) = self.kept.iter().position(|m| m.get("method").and_then(Json::as_str) == Some(method)) {
            return Ok(self.kept.remove(i));
        }
        loop {
            let m = self.recv(method, deadline, stop)?;
            if m.get("method").and_then(Json::as_str) == Some(method) {
                return Ok(m);
            }
        }
    }

    pub fn forget_events(&mut self) {
        self.kept.clear();
        while self.rx.try_recv().is_ok() {}
    }

    /// Asks Brave to close, then ends its process group if it does not within `grace`.
    pub fn close(mut self, grace: Duration) {
        let _ = self.call("Browser.close", Json::obj(vec![]), None, Instant::now() + Duration::from_secs(2), &|| false);
        drop(self.w);
        self.child.end(grace);
    }
}
