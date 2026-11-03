//! The confirmer's place (`confirm` is the protocol): bitsinstall listens on `confirm::SOCKET`; one confirmer at
//! a time holds the place until it disconnects. `ask` sends the dialog and waits (bounded) for its answer; the
//! confirmer's grants, revocations and lists are handled as they come. Who may connect is `peer_ok`'s question
//! (the real check reads SO_PEERCRED and /proc/<pid>/exe; tests pass their own).

use crate::access::Client;
use crate::confirm::{self, FromConfirmer, Input};
use crate::decide::{Confirmer, Outcome, State};
use crate::log;
use mcp_wire::installer::Kind;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Who a peer is, or why it is not taken.
pub type PeerCheck = Box<dyn Fn(&UnixStream) -> Result<String, String> + Send + Sync>;

/// A confirmer that does not take a line within this long is treated as gone (B4: no write waits forever,
/// and none holds `slot`).
const WRITE_WAIT: Duration = Duration::from_secs(2);

struct Conn {
    /// Its own lock, so lines do not interleave and a slow write never holds `slot`.
    w: Arc<Mutex<UnixStream>>,
    /// For `shutdown` without the write lock (it also wakes a write stuck on this socket).
    ctl: UnixStream,
    seq: u64,
    who: String,
}

/// What came back for an open dialog.
#[derive(Clone, Copy)]
enum Reply {
    Answer(bool, Input),
    /// MC-10: the access dialog's choice.
    Access(i64, bool, Input),
}

#[derive(Default)]
struct Asks {
    open: HashSet<u64>,
    answers: HashMap<u64, Reply>,
    /// MC-10: the client each open access dialog is about (an answer must name the same one).
    access: HashMap<u64, String>,
}

pub struct Hub {
    state: Arc<State>,
    peer_ok: PeerCheck,
    slot: Mutex<Option<Conn>>,
    asks: Mutex<Asks>,
    cv: Condvar,
    seqs: AtomicU64,
    /// Unix seconds now (the grants' clock).
    clock: fn() -> i64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Hub {
    pub fn new(state: Arc<State>, peer_ok: PeerCheck, clock: fn() -> i64) -> Arc<Hub> {
        Arc::new(Hub { state, peer_ok, slot: Mutex::new(None), asks: Mutex::new(Asks::default()), cv: Condvar::new(), seqs: AtomicU64::new(1), clock })
    }

    /// Takes confirmers from `l` in a thread of its own.
    pub fn serve(self: &Arc<Hub>, l: UnixListener) -> std::io::Result<()> {
        let hub = self.clone();
        std::thread::Builder::new().name("confirmers".into()).spawn(move || {
            for s in l.incoming() {
                match s {
                    Ok(s) => hub.admit(s),
                    Err(e) => {
                        log::warn(&format!("confirmer socket: accept: {e}"));
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })?;
        Ok(())
    }

    fn admit(self: &Arc<Hub>, mut s: UnixStream) {
        // The socket's send timeout covers the clone used for writing as well (one socket, two descriptors).
        if s.set_write_timeout(Some(WRITE_WAIT)).is_err() {
            return;
        }
        let who = match (self.peer_ok)(&s) {
            Ok(w) => w,
            Err(why) => {
                log::warn(&format!("confirmer refused: {why}"));
                let _ = s.write_all(confirm::refused("connect", &why).as_bytes());
                return;
            }
        };
        let seq = self.seqs.fetch_add(1, Ordering::Relaxed);
        // Only this thread fills the place, so a place seen free stays free until it is filled below; the
        // hello (bounded by WRITE_WAIT) goes out without `slot` held.
        let holder = lock(&self.slot).as_ref().map(|c| c.who.clone());
        if let Some(other) = holder {
            let why = format!("another confirmer ({other}) holds the place");
            log::warn(&format!("confirmer {who} refused: {why}"));
            let _ = s.write_all(confirm::refused("connect", &why).as_bytes());
            return;
        }
        let (Ok(w), Ok(ctl)) = (s.try_clone(), s.try_clone()) else { return };
        let (shown, clients) = self.state.access().map_or_else(Default::default, |a| (a.shown(), a.clients()));
        if s.write_all(confirm::hello(&self.state.grants(), &shown, &clients).as_bytes()).is_err() {
            return;
        }
        *lock(&self.slot) = Some(Conn { w: Arc::new(Mutex::new(w)), ctl, seq, who: who.clone() });
        log::info(&format!("confirmer connected: {who}"));
        let hub = self.clone();
        let spawned = std::thread::Builder::new().name("confirmer".into()).spawn(move || {
            hub.read(s, seq);
            hub.gone(seq);
        });
        if spawned.is_err() {
            self.gone(seq);
        }
    }

    /// One line to confirmer `seq`; false (and the confirmer counts as gone) when it is not there any more or
    /// does not take the line within `WRITE_WAIT`.
    fn send(&self, seq: u64, line: &str) -> bool {
        let w = match lock(&self.slot).as_ref() {
            Some(c) if c.seq == seq => c.w.clone(),
            _ => return false,
        };
        if lock(&w).write_all(line.as_bytes()).is_ok() {
            return true;
        }
        log::warn("confirmer did not take a line in time; dropping it");
        self.gone(seq);
        false
    }

    fn gone(&self, seq: u64) {
        let mut slot = lock(&self.slot);
        if slot.as_ref().is_some_and(|c| c.seq == seq) {
            let c = slot.take();
            drop(slot);
            if let Some(c) = c {
                let _ = c.ctl.shutdown(std::net::Shutdown::Both);
                log::info(&format!("confirmer gone: {}", c.who));
            }
            self.cv.notify_all();
        }
    }

    fn read(&self, s: UnixStream, seq: u64) {
        let mut r = BufReader::new(s);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // A line longer than LINE_MAX ends the connection (bounded memory).
            match (&mut r).take(confirm::LINE_MAX as u64 + 1).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) if n > confirm::LINE_MAX => {
                    log::warn("confirmer sent a line longer than 64 KiB; closing it");
                    return;
                }
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&buf);
            let reply = match confirm::parse(text.trim_end()) {
                Some(FromConfirmer::Answer { id, yes, input }) => {
                    let mut a = lock(&self.asks);
                    if a.open.contains(&id) {
                        a.answers.insert(id, Reply::Answer(yes, input));
                        self.cv.notify_all();
                        None
                    } else {
                        Some(confirm::refused("answer", "no dialog with this id is open"))
                    }
                }
                Some(FromConfirmer::Grant { kind, kind_text, until, input }) => match self.state.set_grant(kind, &kind_text, until, input, (self.clock)()) {
                    Ok(_) => Some(confirm::grants(&self.state.grants())),
                    Err(why) => Some(confirm::refused("grant", &why)),
                },
                Some(FromConfirmer::Revoke(Some(Kind::Access))) => {
                    self.state.access().map(|a| a.revoke());
                    Some(self.access_line())
                }
                Some(FromConfirmer::Revoke(k)) => {
                    self.state.revoke(k);
                    // "all" ends full access too.
                    if k.is_none()
                        && let Some(a) = self.state.access()
                        && a.revoke() > 0
                        && !self.send(seq, &self.access_line())
                    {
                        return;
                    }
                    Some(confirm::grants(&self.state.grants()))
                }
                Some(FromConfirmer::List) => {
                    if !self.send(seq, &self.access_line()) {
                        return;
                    }
                    Some(confirm::grants(&self.state.grants()))
                }
                Some(FromConfirmer::Access { id: Some(id), client, hours, nvme, input }) => {
                    let mut a = lock(&self.asks);
                    if a.open.contains(&id) && a.access.get(&id) == Some(&client) {
                        a.answers.insert(id, Reply::Access(hours, nvme, input));
                        self.cv.notify_all();
                        None
                    } else {
                        Some(confirm::refused("access", "no access dialog for this client with this id is open"))
                    }
                }
                Some(FromConfirmer::Access { id: None, client, hours, nvme, input }) => match self.state.access() {
                    // Georg's own grant from Strg+Alt+V.
                    Some(acc) => match acc.grant(&client, hours, nvme, input) {
                        Ok(_) => Some(self.access_line()),
                        Err(why) => Some(confirm::refused("access", &why)),
                    },
                    None => Some(confirm::refused("access", "full access is not kept by this installer")),
                },
                None => Some(confirm::refused("line", "not understood")),
            };
            if let Some(line) = reply
                && !self.send(seq, &line)
            {
                return;
            }
        }
    }
}

impl Hub {
    fn access_line(&self) -> String {
        let (shown, clients) = self.state.access().map_or_else(Default::default, |a| (a.shown(), a.clients()));
        confirm::access(&shown, &clients)
    }

    /// Sends `line` to the confirmer holding the place and waits for the reply to dialog `id`.
    fn wait_reply(&self, id: u64, line: &str, timeout: Duration) -> Result<Reply, Outcome> {
        let Some(seq) = lock(&self.slot).as_ref().map(|c| c.seq) else { return Err(Outcome::NoSession) };
        lock(&self.asks).open.insert(id);
        if !self.send(seq, line) {
            let mut a = lock(&self.asks);
            a.open.remove(&id);
            a.access.remove(&id);
            return Err(Outcome::Ended);
        }
        let end = Instant::now() + timeout;
        let mut a = lock(&self.asks);
        let out = loop {
            if let Some(r) = a.answers.remove(&id) {
                break Ok(r);
            }
            if !lock(&self.slot).as_ref().is_some_and(|c| c.seq == seq) {
                break Err(Outcome::Ended);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(Outcome::Timeout);
            }
            a = self.cv.wait_timeout(a, left.min(Duration::from_millis(500))).unwrap_or_else(|e| e.into_inner()).0;
        };
        a.open.remove(&id);
        a.access.remove(&id);
        drop(a);
        if matches!(out, Err(Outcome::Timeout)) && self.send(seq, &confirm::cancel(id, "no answer in time")) {
            log::info(&format!("dialog {id} closed at the confirmer: no answer in time"));
        }
        out
    }
}

impl Confirmer for Hub {
    fn connected(&self) -> bool {
        lock(&self.slot).is_some()
    }

    fn ask(&self, id: u64, kind: Kind, title: &str, text: &str, timeout: Duration) -> Outcome {
        match self.wait_reply(id, &confirm::ask(id, kind, title, text, timeout.as_millis() as u64), timeout) {
            Ok(Reply::Answer(yes, input)) => Outcome::Answer { yes, input },
            // An access choice answers no other question.
            Ok(Reply::Access(..)) => Outcome::Answer { yes: false, input: Input::None },
            Err(o) => o,
        }
    }

    fn ask_access(&self, id: u64, client: &Client, hours: i64, title: &str, text: &str, timeout: Duration) -> Outcome {
        lock(&self.asks).access.insert(id, client.fp.clone());
        match self.wait_reply(id, &confirm::ask_access(id, title, text, timeout.as_millis() as u64, client, hours), timeout) {
            Ok(Reply::Answer(yes, input)) => Outcome::Answer { yes, input },
            Ok(Reply::Access(hours, nvme, input)) => Outcome::Access { hours, nvme, input },
            Err(o) => o,
        }
    }

    fn access_changed(&self) {
        let seq = lock(&self.slot).as_ref().map(|c| c.seq);
        if let Some(seq) = seq {
            self.send(seq, &self.access_line());
        }
    }
}
