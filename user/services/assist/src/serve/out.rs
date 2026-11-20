//! The way to one client. Answers (from its own thread) and events (from the worker) share it, a whole line
//! at a time. A client that stops reading costs the writer `WRITE_WAIT` once: the connection is then closed
//! for both directions, which also ends the client's thread and with it the question it asked.

use crate::json::Json;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

pub const WRITE_WAIT: Duration = Duration::from_secs(5);

pub struct Out {
    w: Mutex<UnixStream>,
    dead: AtomicBool,
}

impl Out {
    pub fn new(w: UnixStream) -> Out {
        Out { w: Mutex::new(w), dead: AtomicBool::new(false) }
    }

    /// One line. False: the client is gone (now or before).
    pub fn line(&self, j: &Json) -> bool {
        if self.dead() {
            return false;
        }
        let mut text = j.to_text();
        text.push('\n');
        let mut w = self.w.lock().unwrap_or_else(PoisonError::into_inner);
        if w.write_all(text.as_bytes()).is_ok() {
            return true;
        }
        self.dead.store(true, Ordering::SeqCst);
        let _ = w.shutdown(Shutdown::Both);
        false
    }

    pub fn dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }
}
