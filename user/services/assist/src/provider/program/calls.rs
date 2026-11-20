//! The tool socket (HELPER.md version 4, "assist's tools for the helper's model"): where a helper's model calls
//! the tools assist offers for this answer, the same tools an HTTP provider gets. A Unix socket made anew for every
//! answer in a folder only this user can enter, gone with the answer.
//!
//! Its name protects nothing; the kernel says who connects, and only a process of the helper's own process group
//! that descends from the helper is served (a command the model starts in a session of its own is neither).
//! Each line is one request: `{"id","list":true}` is answered here with the offered tools; `{"id","name","input"}`
//! goes to the answer loop, which runs it as the engine runs an HTTP provider's tool call, one at a time, in the
//! order the calls arrive. Once the answer has ended every call is refused and nothing runs.

use super::bell::{Parent, Seen, in_family, private_dir};
use super::pipes::Msg;
use crate::json::{self, Json};
use crate::tools::ToolAnswer;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The longest request line. A tool's input is small (a setting's value, a step's text of at most 2000
/// characters); this only bounds a helper gone wrong.
pub const LINE_MAX: usize = 1 << 20;
/// Connections open at once: a bridge opens one per call, a few side by side.
const OPEN_MAX: usize = 16;
/// How long a connection may stay silent between two requests before it is closed.
const IDLE: Duration = Duration::from_secs(600);
const TICK: Duration = Duration::from_millis(20);
/// What a call that comes after the answer gets.
pub const ENDED: &str = "the answer has ended; nothing ran";

/// Who may use the socket: the helper's process group (`group` gives a process's group; None: it is gone).
pub type Group = dyn Fn(u32) -> Option<u32> + Send + Sync;

/// A call's result, and since version 5 the pictures it brought (a window, the screen).
pub struct Answered {
    pub answer: ToolAnswer,
    pub pictures: Vec<crate::conv::Image>,
}

impl From<ToolAnswer> for Answered {
    fn from(answer: ToolAnswer) -> Answered {
        Answered { answer, pictures: Vec::new() }
    }
}

/// A call that waits for its result.
pub struct Call {
    pub name: String,
    pub input: Json,
    pub(super) answer: Sender<Answered>,
    /// The caller went away before the result.
    pub(super) gone: Arc<AtomicBool>,
}

impl Call {
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }
}

pub struct Calls {
    pub path: PathBuf,
    dir: PathBuf,
    listener: Mutex<Option<UnixListener>>,
    closed: Arc<AtomicBool>,
}

impl Calls {
    pub fn open() -> std::io::Result<Calls> {
        let dir = private_dir("assist-tools")?;
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).inspect_err(|_| drop(std::fs::remove_dir_all(&dir)))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).inspect_err(|_| drop(std::fs::remove_dir_all(&dir)))?;
        Ok(Calls { path, dir, listener: Mutex::new(Some(listener)), closed: Arc::new(AtomicBool::new(false)) })
    }

    /// Serves the helper `head` (also its process group's number) from now on. `tools` is the offered list as
    /// the request carries it; calls go to `tx`.
    pub(super) fn hear(&self, head: u32, own_uid: u32, tools: Json, tx: SyncSender<Msg>, seen: Box<Seen>, parent: Box<Parent>, group: Box<Group>) -> std::io::Result<()> {
        let Some(listener) = self.listener.lock().ok().and_then(|mut l| l.take()) else { return Ok(()) };
        listener.set_nonblocking(true)?;
        let closed = self.closed.clone();
        let tools = Arc::new(tools);
        std::thread::Builder::new().name("helper-tools".into()).spawn(move || {
            let open = Arc::new(AtomicUsize::new(0));
            while !closed.load(Ordering::SeqCst) {
                let s = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(_) => {
                        std::thread::sleep(TICK);
                        continue;
                    }
                };
                let turned_away = match seen(&s) {
                    None => Some("the kernel names nobody for the connection".to_string()),
                    Some((pid, uid)) if uid != own_uid => Some(format!("pid {pid} runs as uid {uid}")),
                    Some((pid, _)) if !in_family(pid, head, &*parent) => Some(format!("pid {pid} does not descend from the helper (pid {head})")),
                    Some((pid, _)) if group(pid) != Some(head) => Some(format!("pid {pid} is not in the helper's process group {head}")),
                    Some(_) if open.load(Ordering::SeqCst) >= OPEN_MAX => Some(format!("more than {OPEN_MAX} connections at once")),
                    Some(_) => None,
                };
                if let Some(why) = turned_away {
                    if tx.send(Msg::Bell(format!("tool socket: turned away: {why}"))).is_err() {
                        return;
                    }
                    continue;
                }
                open.fetch_add(1, Ordering::SeqCst);
                let (tx, open, closed, tools) = (tx.clone(), open.clone(), closed.clone(), tools.clone());
                let started = std::thread::Builder::new().name("helper-call".into()).spawn({
                    let open = open.clone();
                    move || {
                        serve(s, &tx, &closed, &tools);
                        open.fetch_sub(1, Ordering::SeqCst);
                    }
                });
                if started.is_err() {
                    open.fetch_sub(1, Ordering::SeqCst);
                }
            }
        })?;
        Ok(())
    }
}

impl Drop for Calls {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// One request line: Ok(None) at the end of the connection, Err for a line that is too long or no text.
fn next_line(r: &mut impl BufRead) -> Result<Option<String>, ()> {
    let mut line = Vec::new();
    let n = r.take(LINE_MAX as u64 + 1).read_until(b'\n', &mut line).map_err(|_| ())?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > LINE_MAX {
        return Err(());
    }
    String::from_utf8(line).map(Some).map_err(|_| ())
}

fn reply(id: &str, a: &Answered) -> String {
    let j = Json::obj().with("id", id);
    let j = match &a.answer {
        Ok(result) => j.with("result", result.as_str()),
        Err(error) => j.with("error", error.as_str()),
    };
    if a.pictures.is_empty() {
        return j.to_text();
    }
    let pics: Vec<Json> = a.pictures.iter().map(|p| Json::obj().with("media_type", p.media_type.as_str()).with("data", p.base64.as_str())).collect();
    j.with("images", pics).to_text()
}

/// Requests on one connection, each answered before the next is read.
fn serve(s: UnixStream, tx: &SyncSender<Msg>, closed: &AtomicBool, tools: &Json) {
    let _ = s.set_read_timeout(Some(IDLE));
    let Ok(mut out) = s.try_clone() else { return };
    let mut input = BufReader::new(s);
    loop {
        let line = match next_line(&mut input) {
            Ok(Some(l)) => l,
            Ok(None) => return,
            Err(()) => {
                let _ = writeln!(out, "{}", reply("", &Answered::from(Err(format!("a request line of at most {LINE_MAX} bytes of JSON was expected")))));
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(j) = json::parse(line.trim()) else {
            let _ = writeln!(out, "{}", reply("", &Answered::from(Err("the request is not JSON".to_string()))));
            continue;
        };
        let id = j.str("id").map(str::to_string).or_else(|| j.get("id").and_then(Json::as_i64).map(|n| n.to_string())).unwrap_or_default();
        let text = if j.get("list").and_then(Json::as_bool) == Some(true) {
            Json::obj().with("id", id.as_str()).with("tools", tools.clone()).to_text()
        } else {
            reply(&id, &one(&j, &out, tx, closed))
        };
        if writeln!(out, "{text}").and_then(|_| out.flush()).is_err() {
            return;
        }
    }
}

/// One call through the answer loop. A caller that hangs up while it waits takes the call back: when it has
/// not started yet, it does not run.
fn one(j: &Json, s: &UnixStream, tx: &SyncSender<Msg>, closed: &AtomicBool) -> Answered {
    if closed.load(Ordering::SeqCst) {
        return Err(ENDED.to_string()).into();
    }
    let Some(name) = j.str("name").filter(|n| !n.is_empty()) else { return Err("a call needs the tool's name".to_string()).into() };
    let input = match j.get("input") {
        None | Some(Json::Null) => Json::obj(),
        Some(i) => i.clone(),
    };
    let (answer, heard) = channel();
    let gone = Arc::new(AtomicBool::new(false));
    if tx.send(Msg::Call(Call { name: name.to_string(), input, answer, gone: gone.clone() })).is_err() {
        return Err(ENDED.to_string()).into();
    }
    wait(s, &heard, closed, &gone)
}

fn wait(s: &UnixStream, heard: &Receiver<Answered>, closed: &AtomicBool, gone: &AtomicBool) -> Answered {
    loop {
        match heard.recv_timeout(TICK) {
            Ok(a) => return a,
            Err(RecvTimeoutError::Disconnected) => return Err(ENDED.to_string()).into(),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if closed.load(Ordering::SeqCst) {
            // The answer loop may still hold the call: what it says is no longer heard.
            return Err(ENDED.to_string()).into();
        }
        if crate::term::sys::hung_up(s.as_raw_fd()) {
            gone.store(true, Ordering::SeqCst);
            return Err("the caller went away".to_string()).into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::bell::group;
    use std::os::unix::process::CommandExt;

    #[test]
    fn the_group_of_a_process_is_read_from_proc() {
        let mut child = std::process::Command::new("sleep").arg("5").process_group(0).spawn().unwrap();
        let pid = child.id();
        assert_eq!(group(pid), Some(pid), "a group of its own");
        assert_ne!(group(std::process::id()), Some(pid));
        assert!(group(u32::MAX - 7).is_none());
        let _ = child.kill();
        let _ = child.wait();
    }
}
