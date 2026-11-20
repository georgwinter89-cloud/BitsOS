//! The bell: where a helper program, and the programs it starts, put a question to the assistant (HELPER.md,
//! "Asking the owner"). A Unix socket made anew for every answer, in a folder only this user can enter, with
//! a name nobody can guess. Its name is not what protects it (a program of the same user can read it from
//! another's command line): the kernel says who rings, and only the helper started for this answer and what
//! descends from it is heard. Everybody else is turned away and written to the log.
//!
//! One question per connection: a line in, a line out, closed. The answer loop takes the questions one at a
//! time, in the order they arrive; a second one waits while Georg decides the first. Nothing a ringer does
//! costs more than one thread for a bounded time: it has [`ASK_WAIT`] to send its line, and [`OPEN_MAX`]
//! connections are open at most.

use super::ask::{Ask, LINE_MAX, Reply, Why};
use super::pipes::Msg;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a ringer has to send its question once it connected.
const ASK_WAIT: Duration = Duration::from_secs(10);
/// How long the answer may wait for a ringer that does not take it.
const ANSWER_WAIT: Duration = Duration::from_secs(5);
/// Connections open at once. A helper's model asks one question at a time, a few when it calls tools side
/// by side; this only keeps a helper gone wrong from making threads without end.
const OPEN_MAX: usize = 64;
/// How often the accept loop looks whether the bell was closed, and a waiting ringer whether it is still
/// there.
const TICK: Duration = Duration::from_millis(20);
/// How far up a ringer's ancestors are followed to the helper.
const DEPTH: usize = 64;

/// A question that waits for its answer.
pub struct Ring {
    pub ask: Ask,
    pub(super) answer: Sender<Reply>,
    /// The ringer went away before the answer.
    pub(super) gone: Arc<AtomicBool>,
}

impl Ring {
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }
}

/// Who rang, as the kernel recorded it at `connect`: (pid, uid).
pub type Seen = dyn Fn(&UnixStream) -> Option<(u32, u32)> + Send + Sync;
/// The parent of a process, or None when it is gone.
pub type Parent = dyn Fn(u32) -> Option<u32> + Send + Sync;

pub fn seen(s: &UnixStream) -> Option<(u32, u32)> {
    settingsd::peer::Socket::cred(s)
}

/// The fourth field of `/proc/<pid>/stat`. The second is the program's name in brackets and may hold
/// anything, a bracket too: the fields are counted from the last one.
pub fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// True when `pid` is `head` or descends from it. A process whose parent ended belongs to nobody any more
/// and is not of the family: the bell stays shut rather than guess.
pub fn in_family(pid: u32, head: u32, parent: &Parent) -> bool {
    let mut at = pid;
    for _ in 0..DEPTH {
        if at == head {
            return true;
        }
        match parent(at) {
            Some(p) if p > 1 => at = p,
            _ => return false,
        }
    }
    false
}

/// The fifth field of `/proc/<pid>/stat`: the process group (counted from the name's last bracket, as above).
pub fn group(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(2)?.parse().ok()
}

/// A folder for a socket of one answer that only this user can enter, with a name from the kernel's random
/// numbers: `<prefix>-<random>` in the runtime folder.
pub(super) fn private_dir(prefix: &str) -> std::io::Result<PathBuf> {
    use ring::rand::SecureRandom;
    let mut r = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut r).map_err(|_| std::io::Error::other("no random numbers"))?;
    let name: String = r.iter().map(|b| format!("{b:02x}")).collect();
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir()).unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("{prefix}-{name}"));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    // Whatever the umask said.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub struct Bell {
    pub path: PathBuf,
    dir: PathBuf,
    listener: Mutex<Option<UnixListener>>,
    closed: Arc<AtomicBool>,
}

impl Bell {
    /// Makes the bell. Nobody is heard yet: connections wait until [`Bell::hear`].
    pub fn open() -> std::io::Result<Bell> {
        let dir = private_dir("assist-bell")?;
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).inspect_err(|_| drop(std::fs::remove_dir_all(&dir)))?;
        Ok(Bell { path, dir, listener: Mutex::new(Some(listener)), closed: Arc::new(AtomicBool::new(false)) })
    }

    /// Hears the helper `head` and its descendants from now on; their questions go to `tx`.
    pub(super) fn hear(&self, head: u32, own_uid: u32, tx: SyncSender<Msg>, seen: Box<Seen>, parent: Box<Parent>) -> std::io::Result<()> {
        let Some(listener) = self.listener.lock().ok().and_then(|mut l| l.take()) else { return Ok(()) };
        listener.set_nonblocking(true)?;
        let closed = self.closed.clone();
        std::thread::Builder::new().name("helper-bell".into()).spawn(move || {
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
                    Some(_) if open.load(Ordering::SeqCst) >= OPEN_MAX => Some(format!("more than {OPEN_MAX} questions at once")),
                    Some(_) => None,
                };
                if let Some(why) = turned_away {
                    if tx.send(Msg::Bell(format!("bell: turned away: {why}"))).is_err() {
                        return;
                    }
                    continue;
                }
                open.fetch_add(1, Ordering::SeqCst);
                let (tx, open, closed) = (tx.clone(), open.clone(), closed.clone());
                let started = std::thread::Builder::new().name("helper-ring".into()).spawn({
                    let open = open.clone();
                    move || {
                        one(s, &tx, &closed);
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

impl Drop for Bell {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// One line of at most [`LINE_MAX`] bytes. None: none came in time, or it was longer.
fn read_line(s: &mut UnixStream) -> Option<String> {
    let mut line = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return (!line.is_empty()).then(|| String::from_utf8_lossy(&line).into_owned()),
            Ok(n) => {
                if let Some(end) = chunk[..n].iter().position(|b| *b == b'\n') {
                    line.extend_from_slice(&chunk[..end]);
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
                line.extend_from_slice(&chunk[..n]);
                if line.len() > LINE_MAX {
                    return None;
                }
            }
        }
    }
}

fn say(s: &mut UnixStream, reply: Reply, id: &str) {
    let _ = s.set_nonblocking(false);
    let _ = s.set_write_timeout(Some(ANSWER_WAIT));
    let _ = s.write_all(format!("{}\n", reply.line(id)).as_bytes());
}

/// Waits for the answer loop's word while watching whether the ringer is still there. None: it left. A
/// ringer that closed only its writing side after its line (as `nc -U` and many scripts do) is still there
/// and gets its answer; whatever else it sends is not read.
fn wait(s: &UnixStream, answer: &Receiver<Reply>, closed: &AtomicBool) -> Option<Reply> {
    loop {
        match answer.recv_timeout(TICK) {
            Ok(reply) => return Some(reply),
            // The answer loop dropped the question unanswered: the answer is over.
            Err(RecvTimeoutError::Disconnected) => return Some(Reply::No(Why::Stopped)),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if closed.load(Ordering::SeqCst) {
            return Some(Reply::No(Why::Stopped));
        }
        if crate::term::sys::hung_up(s.as_raw_fd()) {
            return None;
        }
    }
}

fn one(mut s: UnixStream, tx: &SyncSender<Msg>, closed: &AtomicBool) {
    let _ = s.set_read_timeout(Some(ASK_WAIT));
    let Some(ask) = read_line(&mut s).as_deref().and_then(Ask::parse) else {
        say(&mut s, Reply::No(Why::Stopped), "");
        return;
    };
    let id = ask.id.clone();
    let (answer, heard) = channel();
    let gone = Arc::new(AtomicBool::new(false));
    if tx.send(Msg::Ring(Ring { ask, answer, gone: gone.clone() })).is_err() {
        say(&mut s, Reply::No(Why::Stopped), &id);
        return;
    }
    match wait(&s, &heard, closed) {
        Some(reply) => say(&mut s, reply, &id),
        None => gone.store(true, Ordering::SeqCst),
    }
}

/// The other side: rings at `bell` with one question and waits for the answer, however long Georg takes.
/// `check` looks at the connection before anything is written (who listens there). Err: the bell could not
/// be reached, is not the right one, or gave no answer (never a yes).
pub fn ring_at(bell: &Path, ask: &Ask, check: &dyn Fn(&UnixStream) -> Result<(), String>) -> Result<Reply, String> {
    let mut s = UnixStream::connect(bell).map_err(|e| format!("{}: {e}", bell.display()))?;
    check(&s)?;
    s.write_all(format!("{}\n", ask.line()).as_bytes()).map_err(|e| e.to_string())?;
    let mut line = Vec::new();
    s.take(LINE_MAX as u64).read_to_end(&mut line).map_err(|e| e.to_string())?;
    if line.is_empty() {
        return Err("the bell closed without an answer".into());
    }
    Ok(Reply::parse(&String::from_utf8_lossy(&line)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_family_is_the_helper_and_what_descends_from_it() {
        // 100 (the helper) -> 110 -> 111; 200 -> 210 is another family; 300 lost its parent.
        let parent: Box<Parent> = Box::new(|pid| match pid {
            111 => Some(110),
            110 => Some(100),
            100 => Some(50),
            50 => Some(1),
            210 => Some(200),
            200 => Some(1),
            300 => Some(1),
            7 => Some(7),
            _ => None,
        });
        for (pid, inside) in [(100, true), (110, true), (111, true), (50, false), (210, false), (200, false), (300, false), (999, false), (1, false), (7, false)] {
            assert_eq!(in_family(pid, 100, &*parent), inside, "{pid}");
        }
    }

    #[test]
    fn the_parent_of_this_process_is_read_from_proc() {
        let me = std::process::id();
        let mine = parent(me).expect("this process has a parent");
        assert!(mine >= 1 && parent(u32::MAX - 7).is_none());
        assert!(in_family(me, me, &parent) && in_family(me, mine, &parent) && !in_family(mine, me, &parent));
    }
}
