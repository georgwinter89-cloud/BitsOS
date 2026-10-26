//! The warnings and log lines a module keeps for its caller (rule B6): the crate
//! itself never logs, the kernel driver (WL-4) drains them into klog after each
//! call. Not Linux (its rtw89_warn goes straight to the kernel log).
//!
//! A caller that does not drain (or a fault that repeats faster than it drains)
//! must not grow memory without end: past `cap` entries the oldest one goes and
//! is counted, so the log says how many were lost.

use alloc::vec::Vec;

/// How many events a log keeps by default before the oldest go. Not a hardware or
/// resource count (R1): a caller that wants more (or fewer) sets its own with `set_cap`.
pub const DEFAULT_EVENT_CAP: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLog<T> {
    buf: Vec<T>,
    cap: usize,
    dropped: u64,
}

impl<T> Default for EventLog<T> {
    fn default() -> EventLog<T> {
        EventLog {
            buf: Vec::new(),
            cap: DEFAULT_EVENT_CAP,
            dropped: 0,
        }
    }
}

impl<T> EventLog<T> {
    pub fn new() -> EventLog<T> {
        EventLog::default()
    }
    /// At most `cap` (at least 1) events are kept; the oldest beyond it go now
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        self.trim();
    }
    pub fn cap(&self) -> usize {
        self.cap
    }
    pub fn push(&mut self, e: T) {
        self.buf.push(e);
        self.trim();
    }
    fn trim(&mut self) {
        if self.buf.len() > self.cap {
            let n = self.buf.len() - self.cap;
            self.buf.drain(..n);
            self.dropped = self.dropped.saturating_add(n as u64);
        }
    }
    /// What was kept since the last call, oldest first
    pub fn take(&mut self) -> Vec<T> {
        core::mem::take(&mut self.buf)
    }
    pub fn as_slice(&self) -> &[T] {
        &self.buf
    }
    /// Events lost to the cap since the log was made (never reset, so a caller logs the
    /// difference to its last look)
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
