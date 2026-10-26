//! Open files in flight (SCM_RIGHTS) and the collector for unreachable cycles (linux-6.6/net/unix/garbage.c
//! and scm.c `unix_inflight`/`too_many_unix_fds`).
//!
//! A file sent over an AF_UNIX socket lives in the receiver's queue (an `Arc<OpenFile>`) until it is received or
//! the queue is purged (the receiving socket is closed: "closing in flight"). A socket sent over itself, or two
//! sockets sent over each other, keep each other alive after every fd is closed. The collector finds them as
//! Linux does: candidates are unix sockets whose open file is referenced only from queues (strong count ==
//! in-flight count); a candidate referenced from a queue that is not a candidate's, or from an fd, is alive, and
//! so is everything reachable from it; the rest is garbage and its queues are purged, which breaks the cycles.
//! It runs when a unix socket is released while anything is in flight (as Linux's `unix_release_sock`) and when
//! many files are in flight on a send (rate-limited: again only after the in-flight count grew by `TRIGGER_STEP`).
//!
//! Closing files that were in flight is iterative ([`close_files`]): a socket in flight can hold the last
//! reference to another socket whose queue holds the next one, so a program can build a chain as long as its
//! in-flight limit allows (root has none). Dropping such a chain by recursion would run off the kernel stack
//! (R2, R8); Linux defers `fput` to task work for the same reason. Here the outermost close on a thread drains a
//! per-thread list, and closes nested inside it only append to that list.

use super::unix::UnixSock;
use super::SockFile;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::SpinLock;
use vfs::{Errno, OpenFile};

static TOTAL: AtomicUsize = AtomicUsize::new(0);
static RUNS: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// A collection was asked for while one ran: the running one makes another pass.
static AGAIN: AtomicBool = AtomicBool::new(false);
static PER_UID: SpinLock<BTreeMap<u32, usize>> = SpinLock::named("net.unix.inflight_uid", BTreeMap::new());
/// Unix sockets whose own open file is in flight now (or was: pruned on every run), keyed by the socket's
/// address, so a send finds out in O(log n) whether its socket is listed already.
static LIST: SpinLock<BTreeMap<usize, (Weak<OpenFile>, Weak<UnixSock>)>> = SpinLock::named("net.unix.gc_list", BTreeMap::new());
/// The in-flight count when the last send-triggered collection ran (the rate limit of `attach`).
static LAST_SEND_RUN: AtomicUsize = AtomicUsize::new(0);
/// Files whose closing a thread's outermost [`close_files`] does (thread key -> files still to close).
static DEFERRED: SpinLock<BTreeMap<u64, Vec<Arc<OpenFile>>>> = SpinLock::named("net.unix.deferred_close", BTreeMap::new());

/// A send with this many files in flight system-wide runs the collector first (`UNIX_INFLIGHT_TRIGGER_GC`).
const TRIGGER: usize = 16_000;
/// Above `TRIGGER`, a send runs the collector again only when this many more files went into flight since the
/// last send-triggered run (Linux runs it on every such send; one pass is O(in flight), so every send would
/// make a flood O(n^2)). Garbage is still found on the next release of any unix socket.
const TRIGGER_STEP: usize = 1_000;

pub fn inflight_total() -> usize {
    TOTAL.load(Ordering::Relaxed)
}
pub fn runs() -> u64 {
    RUNS.load(Ordering::Relaxed)
}
pub fn freed() -> u64 {
    FREED.load(Ordering::Relaxed)
}

fn unix_of(f: &OpenFile) -> Option<Arc<UnixSock>> {
    SockFile::of(&f.file).and_then(|s| s.unix().cloned())
}

/// The files of one SCM_RIGHTS message while they sit in a queue. Dropping it (a purged queue) closes them;
/// the owner drops it outside every socket lock.
pub struct InFlight {
    files: Vec<Arc<OpenFile>>,
    uid: u32,
}

impl InFlight {
    pub fn none() -> InFlight {
        InFlight { files: Vec::new(), uid: 0 }
    }

    /// Puts `files` in flight for user `uid`. ETOOMANYREFS when that user already has more files in flight
    /// than its RLIMIT_NOFILE (`nofile`; root is not limited, CAP_SYS_RESOURCE).
    pub fn attach(files: Vec<Arc<OpenFile>>, uid: u32, nofile: usize) -> Result<InFlight, Errno> {
        if files.is_empty() {
            return Ok(InFlight::none());
        }
        {
            let mut m = PER_UID.lock();
            let cur = m.get(&uid).copied().unwrap_or(0);
            if uid != 0 && cur > nofile {
                return Err(super::abi::ETOOMANYREFS);
            }
            *m.entry(uid).or_insert(0) += files.len();
        }
        let total = TOTAL.fetch_add(files.len(), Ordering::AcqRel) + files.len();
        if total > TRIGGER && total >= LAST_SEND_RUN.load(Ordering::Relaxed).saturating_add(TRIGGER_STEP) {
            LAST_SEND_RUN.store(total, Ordering::Relaxed);
            collect();
        }
        for f in &files {
            if let Some(u) = unix_of(f)
                && u.inflight.fetch_add(1, Ordering::AcqRel) == 0
            {
                // An entry under the same address with a dead socket is a freed one's: replaced.
                LIST.lock().insert(Arc::as_ptr(&u) as usize, (Arc::downgrade(f), Arc::downgrade(&u)));
            }
        }
        Ok(InFlight { files, uid })
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Copies for MSG_PEEK (Linux `scm_fp_dup`): the files stay in flight.
    pub fn peek(&self) -> Vec<Arc<OpenFile>> {
        self.files.clone()
    }

    /// Takes the files out of flight (received).
    pub fn take(mut self) -> Vec<Arc<OpenFile>> {
        self.detach();
        core::mem::take(&mut self.files)
    }

    /// The unix sockets among the files (the collector's edges).
    pub(crate) fn sockets(&self) -> Vec<Arc<UnixSock>> {
        self.files.iter().filter_map(|f| unix_of(f)).collect()
    }

    fn detach(&mut self) {
        if self.files.is_empty() {
            return;
        }
        for f in &self.files {
            if let Some(u) = unix_of(f) {
                u.inflight.fetch_sub(1, Ordering::AcqRel);
            }
        }
        TOTAL.fetch_sub(self.files.len(), Ordering::AcqRel);
        let mut m = PER_UID.lock();
        if let Some(c) = m.get_mut(&self.uid) {
            *c = c.saturating_sub(self.files.len());
            if *c == 0 {
                m.remove(&self.uid);
            }
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.detach();
        close_files(core::mem::take(&mut self.files));
    }
}

/// Who calls: the kernel thread (its scheduler id), or the host thread in host tests.
fn thread_key() -> u64 {
    #[cfg(target_os = "none")]
    {
        sched::current_id()
    }
    #[cfg(not(target_os = "none"))]
    {
        static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        std::thread_local!(static ID: u64 = NEXT.fetch_add(1, Ordering::Relaxed));
        ID.with(|i| *i)
    }
}

/// Closes files that were in flight without recursion: inside another `close_files` of the same thread (a
/// closed socket's queue held them) they are appended to that thread's list, and the outermost call closes the
/// list until it stays empty. The stack depth is therefore one level whatever chain a program built.
pub(crate) fn close_files(files: Vec<Arc<OpenFile>>) {
    if files.is_empty() {
        return;
    }
    let me = thread_key();
    let mut files = Some(files);
    let outer = {
        let mut d = DEFERRED.lock();
        match d.get_mut(&me) {
            Some(v) => {
                if let Some(f) = files.take_if(|f| v.try_reserve(f.len()).is_ok()) {
                    v.extend(f);
                }
                false
            }
            None => {
                d.insert(me, Vec::new());
                true
            }
        }
    };
    // Nested and the list could not grow (R2: a full heap): closed here, one level deeper, which is rare and
    // still correct.
    drop(files);
    if !outer {
        return;
    }
    loop {
        let batch = {
            let mut d = DEFERRED.lock();
            match d.get_mut(&me) {
                Some(v) if !v.is_empty() => core::mem::take(v),
                _ => {
                    d.remove(&me);
                    break;
                }
            }
        };
        drop(batch);
    }
}

/// Runs the collector if anything is in flight (a unix socket was released).
pub fn maybe_collect() {
    if TOTAL.load(Ordering::Acquire) > 0 {
        collect();
    }
}

/// A collection. One runs at a time: a request while one runs (from another CPU, or from a purge that
/// releases sockets) makes the running one do another pass instead of waiting.
pub fn collect() {
    if RUNNING.swap(true, Ordering::AcqRel) {
        AGAIN.store(true, Ordering::Release);
        return;
    }
    loop {
        pass();
        RUNNING.store(false, Ordering::Release);
        if !AGAIN.swap(false, Ordering::AcqRel) || RUNNING.swap(true, Ordering::AcqRel) {
            return;
        }
    }
}

fn pass() {
    RUNS.fetch_add(1, Ordering::Relaxed);
    let list: Vec<(Weak<OpenFile>, Weak<UnixSock>)> = {
        let mut l = LIST.lock();
        l.retain(|_, (f, s)| f.strong_count() > 0 && s.strong_count() > 0);
        l.values().cloned().collect()
    };
    // Candidates: referenced only from queues (no fd, no syscall holding the file right now).
    let mut cands: Vec<(Arc<UnixSock>, usize)> = Vec::new();
    for (f, s) in &list {
        let Some(s) = s.upgrade() else { continue };
        let inflight = s.inflight.load(Ordering::Acquire);
        if inflight > 0 && f.strong_count() == inflight {
            cands.push((s, inflight));
        }
    }
    if cands.is_empty() {
        return;
    }
    let by_ptr: BTreeMap<usize, usize> = cands.iter().enumerate().map(|(i, (s, _))| (Arc::as_ptr(s) as usize, i)).collect();
    let index = |u: &Arc<UnixSock>, _: &[(Arc<UnixSock>, usize)]| by_ptr.get(&(Arc::as_ptr(u) as usize)).copied();
    let children: Vec<Vec<Arc<UnixSock>>> = cands.iter().map(|(s, _)| s.inflight_children()).collect();
    // References from candidates' queues do not count as outside references.
    let mut remaining: Vec<usize> = cands.iter().map(|(_, n)| *n).collect();
    for kids in &children {
        for k in kids {
            if let Some(i) = index(k, &cands) {
                remaining[i] = remaining[i].saturating_sub(1);
            }
        }
    }
    let mut alive: Vec<bool> = remaining.iter().map(|r| *r > 0).collect();
    let mut todo: Vec<usize> = (0..cands.len()).filter(|&i| alive[i]).collect();
    while let Some(i) = todo.pop() {
        for k in &children[i] {
            if let Some(j) = index(k, &cands)
                && !alive[j]
            {
                alive[j] = true;
                todo.push(j);
            }
        }
    }
    let garbage: Vec<Arc<UnixSock>> = cands.iter().zip(alive.iter()).filter(|(_, a)| !**a).map(|((s, _), _)| s.clone()).collect();
    drop(children);
    drop(cands);
    let n = garbage.len();
    for g in &garbage {
        g.purge();
    }
    drop(garbage);
    if n > 0 {
        FREED.fetch_add(n as u64, Ordering::Relaxed);
        klog::event!(Info, "net", "unix.gc", freed = n, inflight = TOTAL.load(Ordering::Relaxed); "unreachable sockets in flight released");
    }
}
