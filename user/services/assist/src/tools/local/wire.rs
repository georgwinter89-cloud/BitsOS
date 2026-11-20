//! One request line to the local tools socket and its one answer line. Every wait here ends at the caller's
//! deadline or when the caller cancels (Esc), and the answer is bounded by the caller's budget: a door that
//! hangs, never reads or sends without end costs one call, not the chat.

use crate::net::Ctl;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How often a wait looks at the deadline and at Esc.
const TICK: Duration = Duration::from_millis(50);
const CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// No socket at the path, or nobody listens on it: a kernel without the local tools.
    Absent(String),
    /// Nothing came by the deadline.
    Hang,
    Cancelled,
    /// More than the budget.
    TooBig,
    /// The other side closed without an answer line.
    Closed,
    /// The socket named from outside is the one it must never be (`To::never`).
    Door,
    Io(String),
}

/// Where a request goes.
#[derive(Clone, Copy)]
pub struct To<'a> {
    pub path: &'a Path,
    /// For a socket named from outside: the socket it must not be (the kernel's door), by whatever name,
    /// link or second mount it is reached.
    pub never: Option<&'a Path>,
}

/// The file at `path`, held without any I/O on it (O_PATH), and refused when it is the file at `never`. The
/// connect then goes through this very descriptor, so no link or directory swapped between the look and the
/// connect leads to `never` after all.
fn hold(path: &Path, never: &Path) -> Result<File, Fail> {
    let held = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_PATH).open(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => Fail::Absent(e.to_string()),
        _ => Fail::Io(format!("{}: {e}", path.display())),
    })?;
    let mine = held.metadata().map_err(|e| Fail::Io(format!("{}: {e}", path.display())))?;
    match std::fs::metadata(never) {
        Ok(door) if (door.dev(), door.ino()) == (mine.dev(), mine.ino()) => Err(Fail::Door),
        Ok(_) => Ok(held),
        // No such socket on this machine (a host without BitsOS): nothing to keep away from.
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(held),
        // It may be there and cannot be looked at: not told apart, not used.
        Err(e) => Err(Fail::Io(format!("{}: {e}", never.display()))),
    }
}

fn check(ctl: &Ctl) -> Result<(), Fail> {
    if (ctl.cancel)() {
        return Err(Fail::Cancelled);
    }
    if Instant::now() >= ctl.deadline {
        return Err(Fail::Hang);
    }
    Ok(())
}

fn waiting(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted)
}

/// connect() on a unix socket waits without end while the listener's queue is full, and std has no deadline
/// for it: a helper thread connects, this one keeps looking at the deadline and at Esc. A helper that is
/// given up ends by itself when the kernel answers its connect.
fn connect(to: To, ctl: &Ctl) -> Result<UnixStream, Fail> {
    let (tx, rx) = mpsc::channel();
    let (held, p) = match to.never {
        None => (None, to.path.to_path_buf()),
        Some(never) => {
            let f = hold(to.path, never)?;
            let through = PathBuf::from(format!("/proc/self/fd/{}", f.as_raw_fd()));
            (Some(f), through)
        }
    };
    std::thread::Builder::new()
        .name("tools-connect".into())
        .spawn(move || {
            // The descriptor stays this thread's until its connect is answered: its number names nothing else.
            let _held = held;
            let _ = tx.send(UnixStream::connect(&p));
        })
        .map_err(|e| Fail::Io(format!("no thread for the connect: {e}")))?;
    loop {
        match rx.recv_timeout(TICK) {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => return Err(Fail::Absent(e.to_string())),
            Ok(Err(e)) => return Err(Fail::Io(e.to_string())),
            Err(RecvTimeoutError::Timeout) => check(ctl)?,
            Err(RecvTimeoutError::Disconnected) => return Err(Fail::Io("the connect ended without a result".into())),
        }
    }
}

/// False when the other side is gone: a refusal may still wait to be read.
fn send(mut s: &UnixStream, mut bytes: &[u8], ctl: &Ctl) -> Result<bool, Fail> {
    while !bytes.is_empty() {
        check(ctl)?;
        match s.write(bytes) {
            Ok(0) => return Ok(false),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if waiting(&e) => {}
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}

fn read_line(mut s: &UnixStream, ctl: &Ctl) -> Result<String, Fail> {
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        check(ctl)?;
        let n = match s.read(&mut chunk) {
            Ok(0) => return Err(Fail::Closed),
            Ok(n) => n,
            Err(e) if waiting(&e) => continue,
            Err(e) => return Err(Fail::Io(e.to_string())),
        };
        let end = chunk[..n].iter().position(|b| *b == b'\n');
        line.extend_from_slice(&chunk[..end.unwrap_or(n)]);
        if line.len() > ctl.budget {
            return Err(Fail::TooBig);
        }
        if end.is_some() {
            return String::from_utf8(line).map_err(|_| Fail::Io("the answer is not UTF-8".into()));
        }
    }
}

/// Sends `line` (without its line end) on a new connection and returns the answer line. The connection is
/// closed on every way out, which is also how a running call is given up: the door notices within 0.1 s.
pub fn exchange(to: To, line: &str, ctl: &Ctl) -> Result<String, Fail> {
    let s = connect(to, ctl)?;
    let io = |e: std::io::Error| Fail::Io(e.to_string());
    s.set_read_timeout(Some(TICK)).map_err(io)?;
    s.set_write_timeout(Some(TICK)).map_err(io)?;
    let sent = send(&s, line.as_bytes(), ctl)? && send(&s, b"\n", ctl)?;
    match read_line(&s, ctl) {
        // Closed before it took the request and without a word.
        Err(Fail::Closed | Fail::Io(_)) if !sent => Err(Fail::Closed),
        other => other,
    }
}
