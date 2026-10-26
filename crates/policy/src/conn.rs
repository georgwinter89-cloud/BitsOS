//! Connection event records (Opened/Accepted/Closed; `net_info`, R9 "never phones home" check, PM-4). The
//! connection view itself, one entry per connection, is `view`; `record_conn` feeds both.
//!
//! A ring of the newest records: adding one is O(1) (it was a `Vec::remove(0)` of 4096 records). How many are
//! kept is a setting (`set_conn_keep`, also the view's past connections), not a limit on connections: it bounds
//! the history kept in memory; the journal (RB-8) keeps it longer from the `pm conn.closed` events.

use crate::{IpAddr, NetProto, Subject};
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::SpinLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnKind {
    Opened,
    Accepted,
    Closed,
}

#[derive(Clone, Debug)]
pub struct ConnRecord {
    pub subject: Subject,
    pub kind: ConnKind,
    pub proto: NetProto,
    pub local_port: u16,
    pub remote: IpAddr,
    pub remote_port: u16,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub time_ns: u64,
}

static CONNS: SpinLock<VecDeque<ConnRecord>> = SpinLock::named("policy.conns", VecDeque::new());
static KEEP: AtomicUsize = AtomicUsize::new(4096);

pub fn set_conn_keep(n: usize) {
    let n = n.max(1);
    KEEP.store(n, Ordering::Relaxed);
    let mut v = CONNS.lock();
    while v.len() > n {
        v.pop_front();
    }
    drop(v);
    crate::view::trim(n);
}

pub fn conn_keep() -> usize {
    KEEP.load(Ordering::Relaxed)
}

/// An event record for a connection without a `ConnHandle` (the view ends a live one on its Closed record).
/// O(1) under the lock: the ring grows (doubling, up to `conn_keep`) with memory allocated before the lock is
/// taken, so the net stack's connection path never waits on the allocator while holding it.
pub fn record_conn(r: ConnRecord) {
    crate::view::from_record(&r);
    push_event(r);
}

/// The ring part of `record_conn` (the view's own opens and closes add their event records here).
pub(crate) fn push_event(r: ConnRecord) {
    let keep = conn_keep();
    let mut spare: Option<VecDeque<ConnRecord>> = None;
    loop {
        let mut v = CONNS.lock();
        let full = v.len() == v.capacity() && v.len() < keep;
        if full {
            if let Some(b) = spare.as_mut().filter(|b| b.capacity() > v.len()) {
                // Plain moves into memory that is already there.
                b.extend(v.drain(..));
                core::mem::swap(&mut *v, b);
            } else {
                let want = keep.min(v.capacity().max(8) * 2);
                drop(v);
                spare = Some(VecDeque::with_capacity(want));
                continue;
            }
        }
        while v.len() >= keep {
            v.pop_front();
        }
        v.push_back(r);
        drop(v);
        // `spare` (now the old, empty ring) is freed here, after the lock.
        return;
    }
}

/// The newest `max` records, newest first. The result is allocated before the lock is taken.
pub fn recent_connections(max: usize) -> Vec<ConnRecord> {
    let mut out = Vec::with_capacity(max.min(conn_keep()));
    let v = CONNS.lock();
    out.extend(v.iter().rev().take(max).cloned());
    out
}

/// Drops `pid`'s event records (rerunnable tests, `forget_test_connections`); records are plain data, nothing is
/// freed under the lock.
pub(crate) fn forget_pid(pid: u32) {
    CONNS.lock().retain(|r| r.subject.pid != pid);
}

/// Drops the event records of one peer of `subject`'s socket on `local_port` (`view::forget_test_peer`).
pub(crate) fn forget_peer(subject: &Subject, local_port: u16, remote: IpAddr, remote_port: u16) {
    CONNS.lock().retain(|r| !(r.subject == *subject && r.local_port == local_port && r.remote == remote && r.remote_port == remote_port));
}
