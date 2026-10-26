//! CL-2: settingsd's tools in the kernel's MCP server (MC-6 through user/lib/mcpreg), so Claude configures BitsOS
//! by talking: `settings_list/describe/get/set/undo/history` (`mcp_tools`).
//!
//! The registry may not be there yet when init starts settingsd, and it drops our tools whenever the connection
//! ends, so the bridge never gives up: it reconnects with back-off and registers again, logging each state once
//! (not every retry). Reads are answered on the reader thread from the snapshot; changes go to one worker
//! thread per connection, because a change may wait for the stick up to its deadline and must not hold reads
//! back. A change whose caller has gone (cancelled, its deadline passed while it queued, or the connection
//! ended) is not started: the caller already got an error, so applying it anyway would be a change nobody saw
//! succeed.

use crate::events::{Events, Level, q};
use crate::mcp_tools;
use crate::server::Shared;
use mcpreg::{Call, Event, Replier, Service};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// The owner name of the tools in the kernel's `tools/list`.
pub const SERVICE: &str = "settingsd";

pub struct Bridge {
    pub socket: PathBuf,
    pub shared: Arc<Shared>,
    pub events: Events,
}

pub fn spawn(b: Bridge) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("settingsd-mcp".into()).spawn(move || run(b))
}

/// A state of the bridge worth one event; `key` decides whether it is new.
struct State {
    last: String,
    events: Events,
}

impl State {
    fn report(&mut self, key: &str, level: Level, name: &str, fields: &str) {
        if self.last != key {
            self.last = key.to_string();
            self.events.emit(level, name, fields);
        }
    }
}

fn run(b: Bridge) {
    let mut st = State { last: String::new(), events: b.events.clone() };
    let mut backoff = BACKOFF_MIN;
    loop {
        if session(&b, &mut st) {
            backoff = BACKOFF_MIN;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connection: connect, register, answer until it ends. True when the tools were registered.
fn session(b: &Bridge, st: &mut State) -> bool {
    let sock = b.socket.display().to_string();
    let mut svc = match Service::connect_at(&sock, SERVICE) {
        Ok(s) => s,
        Err(e) => {
            let why = e.to_string();
            st.report(&format!("waiting {why}"), Level::Info, "mcp.waiting", &format!("socket={} reason={}", q(&sock), q(&why)));
            return false;
        }
    };
    let names = match svc.register(&mcp_tools::tools()) {
        Ok(n) => n,
        Err(e) => {
            let why = e.to_string();
            st.report(&format!("refused {why}"), Level::Error, "mcp.refused", &format!("socket={} reason={}", q(&sock), q(&why)));
            return false;
        }
    };
    st.report("registered", Level::Info, "mcp.registered", &format!("socket={} tools={}", q(&sock), names.len()));
    let replier = svc.replier();
    let pending = Arc::new(Mutex::new(HashSet::new()));
    let (tx, rx) = channel();
    let worker = {
        let (shared, events, pending, replier) = (b.shared.clone(), b.events.clone(), pending.clone(), replier.clone());
        std::thread::Builder::new().name("settingsd-mcp-change".into()).spawn(move || changes(rx, &shared, &events, &pending, &replier))
    };
    if let Err(e) = worker {
        st.report("no worker", Level::Error, "mcp.lost", &format!("reason={}", q(&format!("no change thread: {e}"))));
        return true;
    }
    let why = loop {
        match svc.next() {
            Ok(Some(Event::Call(c))) if mcp_tools::is_change(&c.name) => {
                let deadline = (c.timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(c.timeout_ms));
                lock(&pending).insert(c.id);
                if tx.send((c, deadline)).is_err() {
                    break "the change thread ended".to_string();
                }
            }
            Ok(Some(Event::Call(c))) => {
                let r = mcp_tools::call(&b.shared, &c.name, &c.arguments);
                if let Err(e) = replier.reply(c.id, r) {
                    break format!("reply: {e}");
                }
            }
            Ok(Some(Event::Cancelled { id, reason })) => {
                if lock(&pending).remove(&id) {
                    b.events.emit(Level::Warn, "mcp.cancelled", &format!("call={id} reason={}", q(&reason)));
                }
            }
            Ok(None) => break "the registry closed the connection".to_string(),
            Err(e) => break e.to_string(),
        }
    };
    // The kernel ended every open call with the connection; queued changes must not run after that.
    let orphans: Vec<u64> = lock(&pending).drain().collect();
    for id in orphans {
        b.events.emit(Level::Warn, "mcp.skipped", &format!("call={id} reason=connection_lost"));
    }
    st.report("lost", Level::Warn, "mcp.lost", &format!("reason={}", q(&why)));
    true
}

fn lock(p: &Mutex<HashSet<u64>>) -> std::sync::MutexGuard<'_, HashSet<u64>> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

fn changes(rx: Receiver<(Call, Option<Instant>)>, shared: &Shared, ev: &Events, pending: &Mutex<HashSet<u64>>, out: &Replier) {
    for (c, deadline) in rx {
        let key = c.arguments.get("key").and_then(|k| k.as_str()).unwrap_or("-").to_string();
        if !lock(pending).remove(&c.id) {
            continue;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            ev.emit(Level::Warn, "mcp.skipped", &format!("tool={} key={} reason=deadline", c.name, q(&key)));
            continue;
        }
        let r = mcp_tools::call(shared, &c.name, &c.arguments);
        let result = match &r {
            mcpreg::Reply::Ok(_) => "ok".to_string(),
            mcpreg::Reply::Error(t) => t.split(':').next().unwrap_or("error").to_string(),
            mcpreg::Reply::BadArgument(_) => "bad_argument".to_string(),
        };
        ev.emit(Level::Info, "mcp.call", &format!("tool={} key={} result={}", c.name, q(&key), q(&result)));
        // A failed write means the connection is gone; the reader sees that and reconnects.
        let _ = out.reply(c.id, r);
    }
}
