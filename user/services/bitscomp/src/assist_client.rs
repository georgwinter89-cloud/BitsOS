//! CL-3b: bitscomp's side of `assist serve`'s socket (`bitsos-assist.sock` in the runtime directory, which is
//! `$XDG_RUNTIME_DIR`, else /run/user/<uid>: `assist serve` looks by the same rule; JSON lines both ways).
//! One connection for the whole session, kept by two threads (B4: the main loop neither connects, reads nor
//! writes): the reader connects (again after a wait that doubles up to `MOST`, at once when `poke`d), hands every
//! line to the main loop through a calloop channel and says when the connection came and went; the writer sends
//! the model's request lines. Deadlines are the model's (`assist::Model::due`), not this file's. On the
//! session's own socket the peer is checked before a line goes either way (`assist_peer::Guard`): a program that
//! put its own listener there is a stranger, said as such, and the reader keeps looking for the real helper.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, SyncSender};

use crate::assist_peer::Guard;
use crate::input::Origin;
use crate::state::App;

/// One line of the assistant is at most this long (an answer arrives in pieces, a history in one line).
const LINE_MAX: u64 = 16 << 20;
const FIRST: Duration = Duration::from_millis(250);
const MOST: Duration = Duration::from_secs(8);
/// Lines read ahead of the main loop.
const QUEUE: usize = 256;
/// A write the assistant does not take within this is a dead connection.
const WRITE_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Up,
    /// No connection (any more): the reason for the log.
    Down(String),
    /// Somebody answers on the socket who is not the assistant's helper: why, for the log.
    Stranger(String),
    Line(Json),
    /// The clipboard's text for the input line (`assist_paste`), and whose key asked for it.
    Paste(String, Origin),
}

/// The socket in the session's runtime directory.
pub fn socket(rundir: &Path) -> PathBuf {
    rundir.join("bitsos-assist.sock")
}

type Shared = Arc<Mutex<Option<UnixStream>>>;

pub struct Client {
    lines: mpsc::Sender<String>,
    poke: mpsc::Sender<()>,
    /// For what else comes back to the main loop as `Msg` (a paste).
    pub tx: SyncSender<Msg>,
}

/// `guard`: who may answer (the session's own socket); None: a socket a test named, not checked.
pub fn start(handle: &LoopHandle<'static, App>, socket: PathBuf, guard: Option<Arc<Guard>>) -> Result<Client, String> {
    // Bounded: when the main loop is behind, the reader waits and the socket fills, instead of lines piling
    // up in bitscomp's memory.
    let (tx, rx) = channel::sync_channel::<Msg>(QUEUE);
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(m) = ev {
                app.assist_msg(m);
            }
        })
        .map_err(|e| format!("assist: {e}"))?;
    let (lines, line_rx) = mpsc::channel::<String>();
    let (poke, poke_rx) = mpsc::channel::<()>();
    let shared: Shared = Arc::new(Mutex::new(None));
    let (s2, t2) = (shared.clone(), tx.clone());
    std::thread::Builder::new()
        .name("bitscomp-assist".into())
        .spawn(move || read_loop(&socket, guard.as_deref(), &s2, &poke_rx, &mut |m| t2.send(m).is_ok()))
        .map_err(|e| format!("assist: no reader thread: {e}"))?;
    std::thread::Builder::new()
        .name("bitscomp-assist-w".into())
        .spawn(move || write_loop(&shared, &line_rx))
        .map_err(|e| format!("assist: no writer thread: {e}"))?;
    Ok(Client { lines, poke, tx })
}

impl Client {
    /// A client without threads or a socket, and where its request lines arrive.
    #[cfg(test)]
    pub fn detached() -> (Client, Receiver<String>) {
        let (tx, _) = channel::sync_channel::<Msg>(QUEUE);
        let (lines, line_rx) = mpsc::channel::<String>();
        let (poke, _) = mpsc::channel::<()>();
        (Client { lines, poke, tx }, line_rx)
    }

    pub fn send(&self, line: String) {
        let _ = self.lines.send(line);
    }

    /// Try to connect now (Georg uses the sidebar) instead of after the wait.
    pub fn poke(&self) {
        let _ = self.poke.send(());
    }
}

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, Option<UnixStream>> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// Connects, reads lines until the connection ends, and again; `out` false: the main loop is gone. What keeps
/// it from a connection (no socket, a stranger, no second handle) is said once per reason, then waited out.
pub fn read_loop(socket: &Path, guard: Option<&Guard>, shared: &Shared, poke: &Receiver<()>, out: &mut dyn FnMut(Msg) -> bool) {
    let mut wait = FIRST;
    let mut said: Option<Msg> = None;
    loop {
        let fail = match connect(socket, guard) {
            Ok((s, w)) => {
                *lock(shared) = Some(w);
                if !out(Msg::Up) {
                    return;
                }
                let why = read_lines(s, out);
                *lock(shared) = None;
                if let Some(g) = guard {
                    g.set_served(false);
                }
                let Some(why) = why else { return };
                wait = FIRST;
                // Said in any case: the model must hear that the connection it had is gone.
                said = None;
                Msg::Down(why)
            }
            Err(m) => m,
        };
        if said.as_ref() != Some(&fail) {
            if !out(fail.clone()) {
                return;
            }
            said = Some(fail);
        }
        match poke.recv_timeout(wait) {
            Ok(()) => wait = FIRST,
            Err(RecvTimeoutError::Timeout) => wait = (wait * 2).min(MOST),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A connection to an accepted peer and its second handle for the writer, or what to say instead.
fn connect(socket: &Path, guard: Option<&Guard>) -> Result<(UnixStream, UnixStream), Msg> {
    let s = UnixStream::connect(socket).map_err(|e| Msg::Down(format!("connect {}: {e}", socket.display())))?;
    if let Some(g) = guard {
        match g.check(&s) {
            Ok(how) => eprintln!("bitscomp: assist: peer accepted: {how}"),
            Err(why) => return Err(Msg::Stranger(why)),
        }
    }
    // Without a second handle nothing could be asked: no connection, and the next try waits like any other.
    let w = s.try_clone().map_err(|e| Msg::Down(format!("no second handle for the socket: {e}")))?;
    let _ = w.set_write_timeout(Some(WRITE_WAIT));
    if let Some(g) = guard {
        g.set_served(true);
    }
    Ok((s, w))
}

/// Lines until the end; why it ended, or None when the main loop is gone.
fn read_lines(s: UnixStream, out: &mut dyn FnMut(Msg) -> bool) -> Option<String> {
    let mut rd = BufReader::new(s);
    loop {
        let mut buf = Vec::new();
        match (&mut rd).take(LINE_MAX).read_until(b'\n', &mut buf) {
            Ok(0) => return Some("the assistant closed the connection".into()),
            Ok(_) if buf.last() != Some(&b'\n') && buf.len() as u64 >= LINE_MAX => return Some("a line of the assistant is too long".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Some(format!("read: {e}")),
        }
        let text = String::from_utf8_lossy(&buf);
        match Json::parse(text.trim()) {
            Ok(j @ Json::Obj(_)) => {
                if !out(Msg::Line(j)) {
                    return None;
                }
            }
            // One broken line is not the end of the conversation.
            _ => eprintln!("bitscomp: assist: a line that is no JSON object ({} bytes): dropped", buf.len()),
        }
    }
}

fn write_loop(shared: &Shared, lines: &Receiver<String>) {
    for l in lines {
        let Some(mut s) = lock(shared).as_ref().and_then(|s| s.try_clone().ok()) else {
            eprintln!("bitscomp: assist: not connected, a request was not sent");
            continue;
        };
        if let Err(e) = s.write_all(format!("{}\n", l.trim()).as_bytes()) {
            eprintln!("bitscomp: assist: the request was not taken ({e}): connection closed");
            // The reader sees the end and connects again.
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("bitscomp-assist-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The same table as `assist serve`'s own test (assist `serve/mod.rs`): the two programs must name one
    /// file. bitscomp hands its choice to the helper as XDG_RUNTIME_DIR (`rundir::select`), so the rows with
    /// the variable are what the helper sees whenever bitscomp started it.
    #[test]
    fn the_socket_is_in_the_runtime_directory_else_under_run_user() {
        let at = |env: Option<&str>, uid: u32| socket(&crate::rundir::choose(env.map(Into::into), uid, |_| true));
        assert_eq!(at(Some("/run/user/1000"), 1000), PathBuf::from("/run/user/1000/bitsos-assist.sock"));
        assert_eq!(at(Some("/tmp/bitscomp-1000"), 1000), PathBuf::from("/tmp/bitscomp-1000/bitsos-assist.sock"));
        assert_eq!(at(None, 1000), PathBuf::from("/run/user/1000/bitsos-assist.sock"));
        assert_eq!(at(Some(""), 7), PathBuf::from("/run/user/7/bitsos-assist.sock"));
    }

    #[test]
    fn a_missing_socket_is_said_once_and_a_late_one_is_found() {
        let d = dir("late");
        let sock = socket(&d);
        let shared: Shared = Arc::new(Mutex::new(None));
        let (poke, poke_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel::<Msg>();
        let (s2, p2) = (shared.clone(), sock.clone());
        let t = std::thread::spawn(move || read_loop(&p2, None, &s2, &poke_rx, &mut |m| tx.send(m).is_ok()));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::Down(_))));
        let l = UnixListener::bind(&sock).unwrap();
        poke.send(()).unwrap();
        let (mut c, _) = l.accept().unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::Up));
        // A request goes out through the shared half, lines come back, a broken one is dropped.
        let (lines, line_rx) = mpsc::channel::<String>();
        let s3 = shared.clone();
        let w = std::thread::spawn(move || write_loop(&s3, &line_rx));
        lines.send(r#"{"op":"status"}"#.into()).unwrap();
        let mut req = String::new();
        BufReader::new(c.try_clone().unwrap()).read_line(&mut req).unwrap();
        assert_eq!(req.trim(), r#"{"op":"status"}"#);
        c.write_all(b"kein json\n{\"ok\":true,\"ready\":true}\n").unwrap();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::Line(j)) if j.get("ready").is_some()));
        drop(c);
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::Down(_))));
        drop((lines, poke, rx));
        w.join().unwrap();
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_listener_that_is_not_the_helper_is_a_stranger_and_gets_no_line() {
        let d = dir("stranger");
        let sock = socket(&d);
        let l = UnixListener::bind(&sock).unwrap();
        // This test process listens: accepted only once the guard knows its pid as the helper's.
        let guard = Arc::new(Guard::new(Path::new("/bin/assist"), crate::drop_root::euid()));
        let shared: Shared = Arc::new(Mutex::new(None));
        let (poke, poke_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel::<Msg>();
        let (s2, p2, g2) = (shared.clone(), sock.clone(), guard.clone());
        let t = std::thread::spawn(move || read_loop(&p2, Some(&g2), &s2, &poke_rx, &mut |m| tx.send(m).is_ok()));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::Stranger(_))));
        assert!(lock(&shared).is_none() && !guard.served(), "nothing can be written to a stranger");
        // The same refusal is not said again; the real helper is found at the next try.
        poke.send(()).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        guard.set_helper(std::process::id());
        poke.send(()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::Up));
        assert!(guard.served());
        drop((poke, rx, l));
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
