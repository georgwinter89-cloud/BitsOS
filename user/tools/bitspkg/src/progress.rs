//! PK-6f: where bitspkgd sees a changing run while it happens. The library reports events (`refresh::event`),
//! the named cut points (`cut::step`: after download, after verify, after unpack, after triggers, before switch,
//! after switch) and every install report line as they come; with no sink set (the command line) nothing is
//! sent and each call costs one atomic load. bitspkgd sets the sink for the one changing run it lets through at
//! a time and turns the items into its `progress` lines (as libalpm's alpm_cb_event / alpm_cb_progress,
//! pacman/lib/libalpm/alpm.h:967, :1153: callbacks of one transaction).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// An event: its name and its key=value pairs (B6, names pkg.*).
    Event(String, Vec<(String, String)>),
    /// A named point of the install path ("after download", ... "after switch").
    Phase(String),
    /// A line of the install report (German).
    Line(String),
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SINK: Mutex<Option<Sender<Item>>> = Mutex::new(None);

/// The named points a run passes, in their order.
pub const PHASES: [&str; 6] = ["after download", "after verify", "after unpack", "after triggers", "before switch", "after switch"];

/// Sets (Some) or clears (None) the sink; the previous one is dropped.
pub fn set(tx: Option<Sender<Item>>) {
    let on = tx.is_some();
    *SINK.lock().unwrap_or_else(|p| p.into_inner()) = tx;
    ACTIVE.store(on, Ordering::Release);
}

fn send(i: Item) {
    if let Some(tx) = SINK.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        // A receiver that is gone (its client left) is no error of the run.
        let _ = tx.send(i);
    }
}

pub fn event(name: &str, kv: &[(&str, &str)]) {
    if ACTIVE.load(Ordering::Acquire) {
        send(Item::Event(name.to_string(), kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()));
    }
}

pub fn phase(what: &str) {
    if ACTIVE.load(Ordering::Acquire) && PHASES.contains(&what) {
        send(Item::Phase(what.to_string()));
    }
}

pub fn line(s: &str) {
    if ACTIVE.load(Ordering::Acquire) {
        send(Item::Line(s.to_string()));
    }
}
