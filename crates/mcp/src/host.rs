//! `Env` over std threads and clocks, for host tests (with `net::host::StdSockets`).

extern crate std;
use crate::server::{Env, ThreadKind};
use alloc::boxed::Box;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub struct StdEnv;

static BASE: OnceLock<Instant> = OnceLock::new();

impl Env for StdEnv {
    fn now_ns(&self) -> u64 {
        BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
    fn sleep_ns(&self, ns: u64) {
        std::thread::sleep(Duration::from_nanos(ns));
    }
    fn spawn(&self, name: &str, _kind: ThreadKind, f: Box<dyn FnOnce() + Send + 'static>) -> Result<(), crate::server::SpawnFailed> {
        std::thread::Builder::new().name(name.into()).spawn(f).map(|_| ()).map_err(|_| crate::server::SpawnFailed)
    }
    /// Off the kernel `WaitQueue` has no scheduler to wake us: look every millisecond.
    fn wait(&self, _wq: &ksync::WaitQueue, cond: &mut dyn FnMut() -> bool, timeout_ns: u64) -> bool {
        let end = Instant::now() + Duration::from_nanos(timeout_ns);
        loop {
            if cond() {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return cond();
            }
            std::thread::sleep((end - now).min(Duration::from_millis(1)));
        }
    }
}

/// MC-6 host tests: a service connection over std's Unix sockets.
pub struct StdLink {
    s: std::os::unix::net::UnixStream,
    w: std::os::unix::net::UnixStream,
    uid: u32,
    /// 0: this process's own pid.
    pid: u32,
    /// Bytes `hung_up` took while it looked for the end of the stream; `read` gives them first.
    ahead: std::sync::Mutex<Vec<u8>>,
}

/// `hung_up` stops looking once this much waits unread (it must not grow with what a client sends).
const AHEAD_MAX: usize = 64 * 1024;

impl StdLink {
    pub fn new(s: std::os::unix::net::UnixStream) -> std::io::Result<StdLink> {
        let w = s.try_clone()?;
        // Writes give up quickly (`Link::try_write` must not wait); reads block (no read timeout).
        w.set_write_timeout(Some(Duration::from_millis(1)))?;
        Ok(StdLink { s, w, uid: 0, pid: 0, ahead: std::sync::Mutex::new(Vec::new()) })
    }
}

impl crate::svc::Link for StdLink {
    fn read(&self, buf: &mut [u8]) -> Result<usize, i64> {
        use std::io::{ErrorKind, Read};
        {
            let mut a = self.ahead.lock().unwrap();
            if !a.is_empty() {
                let n = a.len().min(buf.len());
                buf[..n].copy_from_slice(&a[..n]);
                a.drain(..n);
                return Ok(n);
            }
        }
        loop {
            match (&self.s).read(buf) {
                Ok(n) => return Ok(n),
                // WouldBlock: a `hung_up` in another thread had its short timeout set for a moment.
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
                Err(_) => return Err(-5),
            }
        }
    }
    /// std has no poll: a read with a 1 ms timeout, its bytes kept for `read`. So this stand-in sees the end of
    /// the peer's sending side, the kernel's sockets the end of both (`UnixLink::hung_up`).
    fn hung_up(&self) -> bool {
        use std::io::{ErrorKind, Read};
        let mut a = self.ahead.lock().unwrap();
        if a.len() >= AHEAD_MAX || self.s.set_read_timeout(Some(Duration::from_millis(1))).is_err() {
            return false;
        }
        let mut b = [0u8; 4096];
        let r = (&self.s).read(&mut b);
        let _ = self.s.set_read_timeout(None);
        match r {
            Ok(0) => true,
            Ok(n) => {
                a.extend_from_slice(&b[..n]);
                false
            }
            Err(e) => !matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut),
        }
    }
    fn try_write(&self, data: &[u8]) -> Result<usize, i64> {
        use std::io::{ErrorKind, Write};
        match (&self.w).write(data) {
            Ok(n) => Ok(n),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => Ok(0),
            Err(_) => Err(-32),
        }
    }
    fn close(&self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
    }
    fn peer(&self) -> crate::svc::Peer {
        crate::svc::Peer { pid: if self.pid == 0 { std::process::id() } else { self.pid }, uid: self.uid, gid: 0 }
    }
}

/// MC-6 host tests: the registry's socket over std's `UnixListener` at a path.
pub struct StdListen {
    l: std::os::unix::net::UnixListener,
    path: std::path::PathBuf,
    closed: std::sync::atomic::AtomicBool,
    /// The uid every accepted peer reports (tests of the uid rules; the test process itself runs as one uid).
    uid: std::sync::atomic::AtomicU32,
    /// MC-L: the pid every accepted peer reports (0: the test process's own), so tests can tell peers apart.
    pid: std::sync::atomic::AtomicU32,
}

impl StdListen {
    pub fn bind(path: &std::path::Path) -> std::io::Result<StdListen> {
        let _ = std::fs::remove_file(path);
        let l = std::os::unix::net::UnixListener::bind(path)?;
        Ok(StdListen { l, path: path.to_path_buf(), closed: std::sync::atomic::AtomicBool::new(false), uid: std::sync::atomic::AtomicU32::new(0), pid: std::sync::atomic::AtomicU32::new(0) })
    }
    /// Peers accepted from now on report `uid` (0 by default).
    pub fn set_peer_uid(&self, uid: u32) {
        self.uid.store(uid, std::sync::atomic::Ordering::Release);
    }
    /// Peers accepted from now on report `pid` (0: the test process's own pid).
    pub fn set_peer_pid(&self, pid: u32) {
        self.pid.store(pid, std::sync::atomic::Ordering::Release);
    }
}

impl crate::svc::Listen for StdListen {
    fn accept(&self) -> Result<alloc::boxed::Box<dyn crate::svc::Link>, i64> {
        let (s, _) = self.l.accept().map_err(|_| -22i64)?;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(-22);
        }
        let uid = self.uid.load(std::sync::atomic::Ordering::Acquire);
        let pid = self.pid.load(std::sync::atomic::Ordering::Acquire);
        StdLink::new(s).map(|l| alloc::boxed::Box::new(StdLink { uid, pid, ..l }) as alloc::boxed::Box<dyn crate::svc::Link>).map_err(|_| -12)
    }
    fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::Release);
        // std has no way to wake a blocked accept: connect once (the accept loop sees `closed`).
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}
