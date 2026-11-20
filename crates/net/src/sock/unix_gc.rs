//! Open files in flight (SCM_RIGHTS) and the collector for unreachable cycles (linux-7.3.1/net/unix/garbage.c,
//! `too_many_unix_fds` in af_unix.c:1934).
//!
//! A file sent over an AF_UNIX socket lives in the receiver's queue (an `Arc<OpenFile>`) until it is received or
//! the queue is purged (the receiving socket is closed: "closing in flight"). A socket sent over itself, or two
//! sockets sent over each other, keep each other alive after every fd is closed. The collector finds them as
//! Linux does: candidates are unix sockets whose open file is referenced only from queues (strong count ==
//! in-flight count); a candidate referenced from a queue that is not a candidate's, or from an fd, is alive, and
//! so is everything reachable from it; the rest is garbage and its queues are purged, which breaks the cycles.
//! When it runs follows Linux 7 (`unix_schedule_gc`, garbage.c:646): only while the graph of sockets in flight
//! may hold a cycle (`unix_graph_state`: a message with sockets was queued at a socket that is itself in flight;
//! a pass that finds no cycle clears it), on the release of any unix socket, and on a send of sockets by a user
//! who has `SANE_USER` files in flight already; such a send waits for the collection while a cycle is known
//! (`flush_work`). The send trigger keeps V2's rate limit (`TRIGGER_STEP` files since the last pass,
//! plan/spec/net.md §10.3).
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
use ksync::{SpinLock, WaitQueue};
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
/// Files put in flight since the last pass that walked the graph (the rate limit of `attach`'s trigger): a
/// count since the last collection, not a level, so a burst that drained again never holds the trigger off.
static SINCE_PASS: AtomicUsize = AtomicUsize::new(0);
/// Sends that ran (or joined) a collection.
static SEND_RUNS: AtomicU64 = AtomicU64::new(0);
/// `unix_graph_state` in the low two bits; above them a count of the changes that may close a cycle, so a pass
/// that saw none during its walk can say "no cycle" without missing one made meanwhile.
static GRAPH: AtomicU64 = AtomicU64::new(NOT_CYCLIC);
const NOT_CYCLIC: u64 = 0;
const MAYBE_CYCLIC: u64 = 1;
const CYCLIC: u64 = 2;
const STATE: u64 = 3;
/// Finished collections and their waiters (a send that waits, `flush_work`).
static DONE: AtomicU64 = AtomicU64::new(0);
static DONE_WQ: WaitQueue = WaitQueue::new();
/// Files whose closing a thread's outermost [`close_files`] does (thread key -> files still to close).
static DEFERRED: SpinLock<BTreeMap<u64, Vec<Arc<OpenFile>>>> = SpinLock::named("net.unix.deferred_close", BTreeMap::new());

/// A send of sockets by a user with this many files in flight runs the collector (`UNIX_INFLIGHT_SANE_USER`,
/// SCM_MAX_FD * 8).
const SANE_USER: usize = super::abi::SCM_MAX_FD * 8;
/// A send runs the collector only when this many files went into flight since the last pass (Linux runs it on
/// every such send; one pass here is O(in flight), so every send would make a flood O(n^2)). Garbage is still
/// found on the next release of any unix socket.
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
pub fn send_runs() -> u64 {
    SEND_RUNS.load(Ordering::Relaxed)
}

fn unix_of(f: &OpenFile) -> Option<Arc<UnixSock>> {
    SockFile::of(&f.file).and_then(|s| s.unix().cloned())
}

/// The files of one SCM_RIGHTS message while they sit in a queue. Dropping it (a purged queue) closes them;
/// the owner drops it outside every socket lock.
pub struct InFlight {
    files: Vec<Arc<OpenFile>>,
    uid: u32,
    /// How many of the files are unix sockets (`count_unix`): only those make edges of the graph.
    unix: usize,
}

impl InFlight {
    pub fn none() -> InFlight {
        InFlight { files: Vec::new(), uid: 0, unix: 0 }
    }

    /// Puts `files` in flight for user `uid`. ETOOMANYREFS when that user already has more files in flight
    /// than its RLIMIT_NOFILE (`nofile`; root is not limited, CAP_SYS_RESOURCE).
    pub fn attach(files: Vec<Arc<OpenFile>>, uid: u32, nofile: usize) -> Result<InFlight, Errno> {
        if files.is_empty() {
            return Ok(InFlight::none());
        }
        let before = {
            let mut m = PER_UID.lock();
            let cur = m.get(&uid).copied().unwrap_or(0);
            if uid != 0 && cur > nofile {
                return Err(super::abi::ETOOMANYREFS);
            }
            *m.entry(uid).or_insert(0) += files.len();
            cur
        };
        TOTAL.fetch_add(files.len(), Ordering::AcqRel);
        let since = SINCE_PASS.fetch_add(files.len(), Ordering::Relaxed) + files.len();
        let mut unix = 0;
        for f in &files {
            if let Some(u) = unix_of(f) {
                unix += 1;
                if u.inflight.fetch_add(1, Ordering::AcqRel) == 0 {
                    // An entry under the same address with a dead socket is a freed one's: replaced.
                    LIST.lock().insert(Arc::as_ptr(&u) as usize, (Arc::downgrade(f), Arc::downgrade(&u)));
                }
            }
        }
        // `unix_prepare_fpl`: only a message with sockets asks, with what the user had in flight before it.
        if unix > 0 && before >= SANE_USER && since >= TRIGGER_STEP && schedule(true) {
            SEND_RUNS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(InFlight { files, uid, unix })
    }

    /// The message was queued at `receiver` (`unix_add_edges`): a socket among its files may close a cycle
    /// when the receiver is in flight itself (or is an embryo, whose listener Linux counts, `unix_edge_successor`).
    pub(crate) fn queued_at(&self, receiver: &UnixSock, embryo: bool) {
        if self.unix > 0 && (embryo || receiver.inflight.load(Ordering::Acquire) > 0) {
            maybe_cyclic();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Copies for MSG_PEEK (Linux `scm_fp_dup`): the files stay in flight.
    pub fn peek(&self) -> Vec<Arc<OpenFile>> {
        self.files.clone()
    }

    /// Takes the files out of flight (received by `receiver`). As `unix_del_edges`, a receiver in flight makes
    /// the graph worth a recount: the cycle it was part of may be gone.
    pub fn take(mut self, receiver: &UnixSock) -> Vec<Arc<OpenFile>> {
        if self.unix > 0 && receiver.inflight.load(Ordering::Acquire) > 0 {
            maybe_cyclic();
        }
        self.detach();
        core::mem::take(&mut self.files)
    }

    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    /// Adds the unix sockets among the files (the collector's edges); `out` has room for `len` more.
    pub(crate) fn sockets_into(&self, out: &mut Vec<Arc<UnixSock>>) {
        out.extend(self.files.iter().filter_map(|f| unix_of(f)));
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

/// `unix_update_graph`: a cycle may have been made (or broken: the next pass counts again).
fn maybe_cyclic() {
    let mut w = GRAPH.load(Ordering::Acquire);
    while let Err(now) = GRAPH.compare_exchange_weak(w, ((w >> 2) + 1) << 2 | MAYBE_CYCLIC, Ordering::AcqRel, Ordering::Acquire) {
        w = now;
    }
}

/// Whether the collector would run at all now (`unix_graph_state` is not NOT_CYCLIC).
pub fn may_be_cyclic() -> bool {
    GRAPH.load(Ordering::Acquire) & STATE != NOT_CYCLIC
}

/// A unix socket was released (`unix_release_sock` -> `unix_schedule_gc(NULL)`).
pub fn maybe_collect() {
    schedule(false);
}

/// `unix_schedule_gc`: nothing while no cycle can exist (false); else a collection, which a sending user waits
/// for while a cycle is known.
fn schedule(sender: bool) -> bool {
    let w = GRAPH.load(Ordering::Acquire);
    if w & STATE == NOT_CYCLIC {
        return false;
    }
    let done = DONE.load(Ordering::Acquire);
    run(false);
    if sender && w & STATE == CYCLIC {
        // Another CPU's pass was running: wait until it (and the pass it owes us) is over.
        while DONE.load(Ordering::Acquire) == done && RUNNING.load(Ordering::Acquire) {
            DONE_WQ.wait_until_timeout(|| DONE.load(Ordering::Acquire) != done, super::SLICE_NS);
        }
    }
    true
}

/// A full collection whatever the graph says (tests, and the kernel's own checks).
pub fn collect() {
    run(true);
}

/// One runs at a time: a request while one runs (from another CPU, or from a purge that releases sockets)
/// makes the running one do another pass instead of waiting.
fn run(full: bool) {
    if RUNNING.swap(true, Ordering::AcqRel) {
        AGAIN.store(true, Ordering::Release);
        return;
    }
    let mut full = full;
    loop {
        pass(full);
        full = false;
        RUNNING.store(false, Ordering::Release);
        DONE.fetch_add(1, Ordering::AcqRel);
        if DONE_WQ.waiter_count() > 0 {
            DONE_WQ.wake_all();
        }
        if !AGAIN.swap(false, Ordering::AcqRel) || RUNNING.swap(true, Ordering::AcqRel) {
            return;
        }
    }
}

/// Whether the sockets in `nodes` without `gone` hold a cycle (`children[i]`: indices of the sockets in node
/// i's queue): Kahn's peeling leaves exactly the nodes on or behind a cycle.
fn has_cycle(children: &[Vec<usize>], gone: &[bool]) -> bool {
    let n = children.len();
    let mut indeg = alloc::vec![0usize; n];
    for (i, kids) in children.iter().enumerate() {
        if gone[i] {
            continue;
        }
        for &k in kids {
            if !gone[k] {
                indeg[k] += 1;
            }
        }
    }
    let mut todo: Vec<usize> = (0..n).filter(|&i| !gone[i] && indeg[i] == 0).collect();
    let mut peeled = todo.len();
    while let Some(i) = todo.pop() {
        for &k in &children[i] {
            if gone[k] {
                continue;
            }
            indeg[k] -= 1;
            if indeg[k] == 0 {
                peeled += 1;
                todo.push(k);
            }
        }
    }
    peeled < gone.iter().filter(|g| !**g).count()
}

/// One walk. `full` (or a graph that may have changed) also counts the cycles among every socket in flight,
/// which decides the graph state; a known-cyclic graph only looks at the candidates, as Linux's fast walk.
fn pass(full: bool) {
    let w0 = GRAPH.load(Ordering::Acquire);
    let state = w0 & STATE;
    if !full && state == NOT_CYCLIC {
        return;
    }
    let count = full || state == MAYBE_CYCLIC;
    RUNS.fetch_add(1, Ordering::Relaxed);
    SINCE_PASS.store(0, Ordering::Relaxed);
    // The snapshot's memory is made with LIST dropped (`unix::fill`'s reason); a list that grew asks again.
    let mut list: Vec<(Weak<OpenFile>, Weak<UnixSock>)> = Vec::new();
    loop {
        let want = {
            let mut l = LIST.lock();
            l.retain(|_, (f, s)| f.strong_count() > 0 && s.strong_count() > 0);
            if l.len() <= list.capacity() {
                list.extend(l.values().cloned());
                break;
            }
            l.len()
        };
        list.reserve_exact(want);
    }
    // Candidates: referenced only from queues (no fd, no syscall holding the file right now). Counting cycles
    // needs every socket in flight; the candidates come first.
    let mut nodes: Vec<(Arc<UnixSock>, usize)> = Vec::new();
    let mut others: Vec<(Arc<UnixSock>, usize)> = Vec::new();
    for (f, s) in &list {
        let Some(s) = s.upgrade() else { continue };
        let inflight = s.inflight.load(Ordering::Acquire);
        if inflight > 0 && f.strong_count() == inflight {
            nodes.push((s, inflight));
        } else if count && inflight > 0 {
            others.push((s, inflight));
        }
    }
    let cands = nodes.len();
    nodes.append(&mut others);
    let by_ptr: BTreeMap<usize, usize> = nodes.iter().enumerate().map(|(i, (s, _))| (Arc::as_ptr(s) as usize, i)).collect();
    let index = |u: &Arc<UnixSock>| by_ptr.get(&(Arc::as_ptr(u) as usize)).copied();
    let children: Vec<Vec<usize>> = nodes.iter().map(|(s, _)| s.inflight_children().iter().filter_map(index).collect()).collect();
    // References from candidates' queues do not count as outside references.
    let mut remaining: Vec<usize> = nodes.iter().take(cands).map(|(_, n)| *n).collect();
    for kids in children.iter().take(cands) {
        for &k in kids {
            if k < cands {
                remaining[k] = remaining[k].saturating_sub(1);
            }
        }
    }
    let mut alive: Vec<bool> = remaining.iter().map(|r| *r > 0).collect();
    let mut todo: Vec<usize> = (0..cands).filter(|&i| alive[i]).collect();
    while let Some(i) = todo.pop() {
        for &j in &children[i] {
            if j < cands && !alive[j] {
                alive[j] = true;
                todo.push(j);
            }
        }
    }
    let gone: Vec<bool> = (0..nodes.len()).map(|i| i < cands && !alive[i]).collect();
    let garbage: Vec<Arc<UnixSock>> = nodes.iter().zip(gone.iter()).filter(|(_, g)| **g).map(|((s, _), _)| s.clone()).collect();
    // The state after this pass: what is left once the garbage is gone (a cycle among the rest is alive), or,
    // without a count, "count next time" when garbage went (the known cycles may have been the dead ones).
    let next = if count {
        Some(if has_cycle(&children, &gone) { CYCLIC } else { NOT_CYCLIC })
    } else if !garbage.is_empty() {
        Some(MAYBE_CYCLIC)
    } else {
        None
    };
    if let Some(st) = next {
        // A change during the walk (another `maybe_cyclic`) keeps MAYBE_CYCLIC.
        let _ = GRAPH.compare_exchange(w0, (w0 & !STATE) | st, Ordering::AcqRel, Ordering::Acquire);
    }
    drop(children);
    drop(nodes);
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

#[cfg(test)]
mod tests {
    use super::has_cycle;
    use alloc::vec;

    #[test]
    fn cycles_are_found_and_only_cycles() {
        let none = [false; 4];
        assert!(!has_cycle(&[vec![1], vec![2], vec![], vec![2]], &none), "a tree of queues");
        assert!(has_cycle(&[vec![0]], &[false]), "a socket in its own queue");
        assert!(has_cycle(&[vec![1], vec![2], vec![0], vec![]], &none), "a ring of three");
        assert!(has_cycle(&[vec![3], vec![2], vec![1], vec![]], &none), "a pair behind a chain");
        assert!(!has_cycle(&[vec![1], vec![2], vec![0], vec![]], &[true, false, false, false]), "the ring was garbage");
        assert!(!has_cycle(&[], &[]));
    }
}
