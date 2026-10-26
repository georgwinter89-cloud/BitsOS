//! The socket server: one thread per client (R1: as many clients as come), a ticker for the resync. Requests are
//! read with a size bound so a client cannot make settingsd hold an endless line.

use crate::events::{Level, q};
use crate::json::Json;
use crate::proto::{self, Reply};
use crate::registry::{Registry, err};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

/// The longest request line read (a registration of many keys fits easily); longer lines are refused.
pub const REQUEST_MAX: usize = 1 << 20;

pub enum Line {
    Text(String),
    TooLong,
    End,
}

/// Reads one line of at most `max` bytes; a longer line is skipped up to its end and reported.
pub fn read_line<R: BufRead>(r: &mut R, max: usize) -> std::io::Result<Line> {
    let mut buf = Vec::new();
    let mut long = false;
    loop {
        let avail = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok(if long {
                Line::TooLong
            } else if buf.is_empty() {
                Line::End
            } else {
                Line::Text(String::from_utf8_lossy(&buf).into_owned())
            });
        }
        let (chunk, done) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (&avail[..i], Some(i + 1)),
            None => (avail, None),
        };
        if !long {
            if buf.len() + chunk.len() > max {
                long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let used = done.unwrap_or(avail.len());
        r.consume(used);
        if done.is_some() {
            return Ok(if long { Line::TooLong } else { Line::Text(String::from_utf8_lossy(&buf).into_owned()) });
        }
    }
}

/// The registry behind its lock, and the snapshot readers use (B4: a reader such as the compositor never
/// waits behind a change that waits for a hung stick).
pub struct Shared {
    reg: Mutex<Registry>,
    view: RwLock<Arc<Registry>>,
}

impl Shared {
    pub fn new(reg: Registry) -> Shared {
        let view = RwLock::new(Arc::new(reg.snapshot()));
        Shared { reg: Mutex::new(reg), view }
    }
    pub fn view(&self) -> Arc<Registry> {
        self.view.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn lock(&self) -> MutexGuard<'_, Registry> {
        self.reg.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn publish(&self, r: &Registry) {
        *self.view.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(r.snapshot());
    }
}

pub fn serve(listener: UnixListener, reg: Arc<Shared>) {
    let r = reg.clone();
    let _ = std::thread::Builder::new().name("settings-tick".into()).spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut g = r.lock();
            if g.tick() {
                r.publish(&g);
            }
        }
    });
    for conn in listener.incoming() {
        match conn {
            Ok(s) => {
                let reg = reg.clone();
                if let Err(e) =
                    std::thread::Builder::new().name("settings-client".into()).spawn(move || client(s, &reg))
                {
                    // Out of threads: this client is dropped, the next one may succeed.
                    eprintln!("settingsd client.spawn_failed error={}", q(&e.to_string()));
                }
            }
            Err(e) => {
                eprintln!("settingsd accept.failed error={}", q(&e.to_string()));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn client(s: UnixStream, reg: &Arc<Shared>) {
    let Ok(rs) = s.try_clone() else { return };
    let mut rd = BufReader::new(rs);
    let mut w = s;
    loop {
        let line = match read_line(&mut rd, REQUEST_MAX) {
            Ok(Line::Text(t)) => t,
            Ok(Line::TooLong) => {
                let e = err("bad_request", format!("request longer than {REQUEST_MAX} bytes"));
                if writeln!(w, "{}", proto::error_line(&e)).is_err() {
                    return;
                }
                continue;
            }
            Ok(Line::End) | Err(_) => return,
        };
        if line.trim().is_empty() {
            continue;
        }
        match proto::handle(reg, &line) {
            Reply::Line(a) => {
                if writeln!(w, "{a}").is_err() {
                    return;
                }
            }
            Reply::Watch(prefix) => return watch(rd, w, reg, &prefix),
        }
    }
}

/// Streams change events until the client closes. A helper thread reads the client's side only to see the
/// end; the sender checks for it every second.
fn watch(mut rd: BufReader<UnixStream>, mut w: UnixStream, reg: &Arc<Shared>, prefix: &str) {
    let (rx, lagged) = reg.view().watch(prefix);
    let closed = Arc::new(AtomicBool::new(false));
    let c = closed.clone();
    let _ = std::thread::Builder::new().name("settings-watch-eof".into()).spawn(move || {
        let mut sink = [0u8; 512];
        while matches!(rd.read(&mut sink), Ok(n) if n > 0) {}
        c.store(true, Ordering::Release);
    });
    let hello = Json::obj().with("ok", true).with("watching", prefix).to_text();
    if writeln!(w, "{hello}").is_err() {
        return;
    }
    while !closed.load(Ordering::Acquire) {
        if lagged.swap(false, Ordering::AcqRel)
            && writeln!(w, "{}", Json::obj().with("event", "lagged").to_text()).is_err()
        {
            break;
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(line) => {
                if writeln!(w, "{line}").is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Binds the socket; refuses when another settingsd answers on it, removes a stale one otherwise.
pub fn bind(path: &Path, reg: &Registry) -> Result<UnixListener, String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    if UnixStream::connect(path).is_ok() {
        return Err(format!("another settingsd answers on {}", path.display()));
    }
    let _ = std::fs::remove_file(path);
    // Every local program may talk to settingsd (programs MCP starts run as uid 1000; connect needs write
    // permission on the socket, linux-6.6/net/unix/af_unix.c unix_find_bsd); who may change what is B3's
    // policy (PM-3), not a file mode. bind gives 0777 & ~umask and init runs services with umask 022, so the
    // umask is 0111 for the bind alone (0666). Only this thread runs now; the workers are idle after load.
    let old = usys::umask(0o111).ok();
    let l = UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()));
    if let Some(o) = old {
        let _ = usys::umask(o);
    }
    let l = l?;
    let mode = std::fs::metadata(path).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777);
    if !matches!(mode, Ok(0o666)) {
        // umask failed: try chmod (ENOSYS on V2 until change request CL-1/5).
        if let Err(e) = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666)) {
            let mode = mode.as_ref().map(|m| format!("{m:o}")).unwrap_or_else(|e| e.to_string());
            reg.ev.emit(
                Level::Error,
                "socket.mode",
                &format!(
                    "socket={} mode={} error={} (uid != 0 clients get EACCES)",
                    path.display(),
                    q(&mode),
                    q(&e.to_string())
                ),
            );
        }
    }
    reg.ev.emit(Level::Info, "listening", &format!("socket={}", path.display()));
    Ok(l)
}
