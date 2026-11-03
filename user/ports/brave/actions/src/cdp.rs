//! The DevTools protocol on a pipe (Chromium's --remote-debugging-pipe): Brave reads commands on its fd 3 and
//! writes answers and events on its fd 4, each message one JSON text ended by a NUL byte. One reader thread
//! routes answers to their waiting command (by id) and events to the subscriptions that want them. Commands to a
//! tab carry the `sessionId` of `Target.attachToTarget {flatten: true}`.

use bitsact::events::{Events, Level, q};
use mcpreg::Json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Longest message taken from Brave. A screenshot of a 4K screen as base64 PNG is ~30 MB; read_page cuts its
/// text in the page. A longer one means the stream is broken: the connection ends, its writer is closed, so
/// Brave sees EOF on its fd 3 and ends too.
pub const MAX_MSG: usize = 64 << 20;

type Filter = Box<dyn Fn(&Json) -> bool + Send>;

pub struct Conn {
    w: Mutex<Box<dyn Write + Send>>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Result<Json, String>>>>,
    subs: Mutex<Vec<(u64, Filter, Sender<Json>)>>,
    alive: AtomicBool,
    why: Mutex<String>,
    ev: Events,
}

/// Events a caller waits for; unsubscribed when dropped.
pub struct Sub<'a> {
    conn: &'a Conn,
    id: u64,
    pub rx: Receiver<Json>,
}

impl Drop for Sub<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.conn.subs.lock() {
            s.retain(|(i, _, _)| *i != self.id);
        }
    }
}

impl Sub<'_> {
    /// The next event before `deadline`; None at the deadline or when the connection ended.
    pub fn next(&self, deadline: Instant) -> Option<Json> {
        let left = deadline.saturating_duration_since(Instant::now());
        self.rx.recv_timeout(left).ok()
    }
}

impl Conn {
    /// Starts the reader thread on `r` (Brave's fd 4); commands go to `w` (Brave's fd 3).
    pub fn new(r: impl Read + Send + 'static, w: impl Write + Send + 'static, ev: Events) -> Arc<Conn> {
        let c = Arc::new(Conn {
            w: Mutex::new(Box::new(w)),
            next: AtomicU64::new(1),
            pending: Mutex::default(),
            subs: Mutex::default(),
            alive: AtomicBool::new(true),
            why: Mutex::default(),
            ev,
        });
        let c2 = c.clone();
        let spawned = std::thread::Builder::new().name("cdp-reader".into()).spawn(move || c2.read_loop(r));
        if let Err(e) = spawned {
            c.end(&format!("no reader thread: {e}"));
        }
        c
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn why(&self) -> String {
        self.why.lock().map(|w| w.clone()).unwrap_or_default()
    }

    /// Ends the connection from brave-act's side (Brave sees EOF on its fd 3).
    pub fn close(&self, why: &str) {
        self.end(why);
    }

    fn end(&self, why: &str) {
        if self.alive.swap(false, Ordering::AcqRel) {
            if let Ok(mut w) = self.why.lock() {
                *w = why.to_string();
            }
            self.ev.emit(Level::Warn, "cdp.ended", &format!("why={}", q(why)));
        }
        // Brave's fd 3 sees EOF now, not only when the Conn is dropped (Chromium ends on it).
        if let Ok(mut w) = self.w.lock() {
            *w = Box::new(std::io::sink());
        }
        let waiting: Vec<_> = self.pending.lock().map(|mut p| p.drain().collect()).unwrap_or_default();
        for (_, tx) in waiting {
            let _ = tx.send(Err(format!("Brave's DevTools pipe ended ({why})")));
        }
        if let Ok(mut s) = self.subs.lock() {
            s.clear();
        }
    }

    fn read_loop(&self, mut r: impl Read) {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 64 << 10];
        let mut scanned = 0;
        loop {
            let n = match r.read(&mut chunk) {
                Ok(0) => return self.end("Brave closed it (Brave ended)"),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return self.end(&format!("read: {e}")),
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Some(p) = buf[scanned..].iter().position(|b| *b == 0) {
                let end = scanned + p;
                let msg = String::from_utf8_lossy(&buf[..end]).into_owned();
                buf.drain(..=end);
                scanned = 0;
                self.route(&msg);
            }
            scanned = buf.len();
            if buf.len() > MAX_MSG {
                return self.end(&format!("a message longer than {} MiB", MAX_MSG >> 20));
            }
        }
    }

    fn route(&self, msg: &str) {
        let j = match Json::parse(msg) {
            Ok(j) => j,
            Err(e) => {
                self.ev.emit(Level::Warn, "cdp.bad_message", &format!("why={}", q(&e)));
                return;
            }
        };
        if let Some(id) = j.get("id").and_then(Json::as_u64) {
            let tx = self.pending.lock().ok().and_then(|mut p| p.remove(&id));
            if let Some(tx) = tx {
                let r = match j.get("error") {
                    Some(e) => Err(e.get("message").and_then(Json::as_str).unwrap_or("error without message").to_string()),
                    None => Ok(j.get("result").cloned().unwrap_or(Json::Obj(Vec::new()))),
                };
                let _ = tx.send(r);
            }
        } else if j.get("method").is_some()
            && let Ok(subs) = self.subs.lock()
        {
            for (_, f, tx) in subs.iter() {
                if f(&j) {
                    let _ = tx.send(j.clone());
                }
            }
        }
    }

    /// Events for which `f` holds, from now on.
    pub fn subscribe(&self, f: impl Fn(&Json) -> bool + Send + 'static) -> Sub<'_> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        if let Ok(mut s) = self.subs.lock() {
            s.push((id, Box::new(f), tx));
        }
        Sub { conn: self, id, rx }
    }

    /// Sends `method` (to the tab of `session`, or to the browser) and waits for its answer until `deadline`.
    pub fn call(&self, method: &str, params: Json, session: Option<&str>, deadline: Instant) -> Result<Json, String> {
        if !self.alive() {
            return Err(format!("Brave's DevTools pipe ended ({})", self.why()));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        // end() may have drained `pending` between the check above and the insert: nobody would wake this call.
        if !self.alive() {
            self.forget(id);
            return Err(format!("{method}: Brave's DevTools pipe ended ({})", self.why()));
        }
        let mut m = vec![("id", Json::Num(id as f64)), ("method", Json::str(method)), ("params", params)];
        if let Some(s) = session {
            m.push(("sessionId", Json::str(s)));
        }
        let mut text = Json::obj(m).to_string().into_bytes();
        text.push(0);
        let wrote = self.w.lock().map_err(|_| "writer poisoned".to_string()).and_then(|mut w| w.write_all(&text).and_then(|_| w.flush()).map_err(|e| e.to_string()));
        if let Err(e) = wrote {
            self.forget(id);
            self.end(&format!("write: {e}"));
            return Err(format!("{method}: Brave's DevTools pipe ended ({e})"));
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(r) => r.map_err(|e| format!("{method}: {e}")),
            Err(_) => {
                self.forget(id);
                self.ev.emit(Level::Warn, "cdp.timeout", &format!("method={method}"));
                Err(format!("{method}: Brave did not answer in time"))
            }
        }
    }

    fn forget(&self, id: u64) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&id);
        }
    }
}
