//! DB1: `ui-history`, a bounded ring of Georg's recent actions on the desktop (contract C2 lines
//! `{t,kind,app,window,element,text?}`), and each of Georg's own (real input) also to ctxd as C4's `habit_event`
//! (DB4), fire-and-forget: a thread of its own sends them, the main loop only hands them over and never waits.
//! Text is kept only for fields that are not secret.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use uilist::Event;

/// Events kept for `ui-history` (a few minutes of busy use; older ones fall out).
pub const RING: usize = 512;
/// Events waiting for the sender thread; more are dropped (ctxd slow or gone).
const QUEUE: usize = 64;
const CTXD_WAIT: Duration = Duration::from_secs(1);
/// Between tries to reach a ctxd that is not there.
const RETRY: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct History {
    ring: VecDeque<Event>,
    tx: Option<SyncSender<Event>>,
    pub dropped: u64,
    /// Tests: ctxd's socket instead of the usual place.
    socket: Option<std::path::PathBuf>,
}

static RING_OF_APP: std::sync::Mutex<Option<History>> = std::sync::Mutex::new(None);

/// bitscomp's one history (kept here, so no shared state file needs a field for it).
pub fn with<R>(f: impl FnOnce(&mut History) -> R) -> R {
    let mut g = RING_OF_APP.lock().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(History::default))
}

pub fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl History {
    /// Notes an action; `own`: from Georg's own hand (real input), those go to ctxd too.
    pub fn push(&mut self, e: Event, own: bool) {
        if own {
            let socket = self.socket.clone();
            let tx = self.tx.get_or_insert_with(|| start_sender(socket));
            if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = tx.try_send(e.clone()) {
                self.dropped += 1;
            }
        }
        if self.ring.len() == RING {
            self.ring.pop_front();
        }
        self.ring.push_back(e);
    }

    /// The events of the last `secs` seconds (all with None), oldest first, one JSON line each.
    pub fn lines(&self, secs: Option<u64>, now: u64) -> String {
        let from = secs.map_or(0, |s| now.saturating_sub(s));
        let mut out = String::new();
        for e in self.ring.iter().filter(|e| e.t >= from) {
            if let Ok(l) = serde_json::to_string(e) {
                out.push_str(&l);
                out.push('\n');
            }
        }
        out
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ring.len()
    }
}

fn ctxd_path() -> std::path::PathBuf {
    let own = crate::rundir::dir().join("ctxd.sock");
    if own.exists() { own } else { "/run/bitsos/ctxd.sock".into() }
}

fn start_sender(socket: Option<std::path::PathBuf>) -> SyncSender<Event> {
    let (tx, rx) = sync_channel(QUEUE);
    let _ = std::thread::Builder::new().name("bitscomp-habits".into()).spawn(move || send_loop(rx, socket));
    tx
}

/// C4's request line for one event.
pub fn habit_line(n: u64, e: &Event) -> String {
    let mut args = serde_json::json!({"kind": e.kind, "app": e.app, "window": e.window, "element": e.element, "t": e.t});
    if let Some(name) = &e.name {
        args["name"] = serde_json::Value::String(name.clone());
    }
    format!("{}\n", serde_json::json!({"id": format!("h{n}"), "op": "habit_event", "args": args}))
}

fn send_loop(rx: Receiver<Event>, socket: Option<std::path::PathBuf>) {
    let mut conn: Option<(UnixStream, BufReader<UnixStream>)> = None;
    let mut next_try = Instant::now();
    let mut n = 0u64;
    while let Ok(e) = rx.recv() {
        if conn.is_none() && Instant::now() >= next_try {
            conn = UnixStream::connect(socket.clone().unwrap_or_else(ctxd_path)).ok().and_then(|s| {
                s.set_write_timeout(Some(CTXD_WAIT)).ok()?;
                s.set_read_timeout(Some(CTXD_WAIT)).ok()?;
                Some((s.try_clone().ok()?, BufReader::new(s)))
            });
            if conn.is_none() {
                next_try = Instant::now() + RETRY;
            }
        }
        let Some((w, r)) = conn.as_mut() else { continue };
        n += 1;
        let mut answer = String::new();
        // The answer is read and dropped (C4 says what it learned elsewhere); an error drops the connection.
        if w.write_all(habit_line(n, &e).as_bytes()).is_err() || r.read_line(&mut answer).is_err() {
            conn = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: u64, kind: &str) -> Event {
        Event { t, kind: kind.into(), app: "bitscomp".into(), window: "desktop".into(), element: "desktop/x".into(), text: None, name: None, by: None }
    }

    #[test]
    fn the_ring_is_bounded_and_filters_by_age() {
        let mut h = History::default();
        for i in 0..(RING as u64 + 10) {
            h.push(ev(i, "press"), false);
        }
        assert_eq!(h.len(), RING);
        let all = h.lines(None, 10_000);
        assert_eq!(all.lines().count(), RING);
        assert!(all.lines().next().unwrap().contains("\"t\":10,"), "the oldest fell out");
        assert_eq!(h.lines(Some(5), RING as u64 + 9).lines().count(), 6);
        assert!(h.tx.is_none(), "nothing of a script goes to ctxd");
    }

    #[test]
    fn habit_events_reach_a_listening_socket_without_text() {
        let dir = std::env::temp_dir().join(format!("bitscomp-habit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ctxd.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut h = History { socket: Some(path.clone()), ..Default::default() };
        let mut e = ev(7, "type");
        e.text = Some("einstellungen".into());
        h.push(e, true);
        let (s, _) = l.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["op"], "habit_event");
        assert_eq!(v["args"]["kind"], "type");
        assert!(v["args"].get("text").is_none(), "typed text stays in bitscomp");
        (&s).write_all(b"{\"id\":\"h1\",\"ok\":true}\n").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
