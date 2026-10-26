//! File locks (AB-5): POSIX record locks and open-file-description (OFD) locks of fcntl(2), and flock(2)
//! locks, after linux-6.6/fs/locks.c.
//!
//! - **Owners.** A POSIX lock belongs to the fd table (`fl_owner = current->files`): threads and CLONE_FILES
//!   processes share them, fork does not inherit them, and *any* close of an fd of the file by that table
//!   drops every lock the table holds on it (`filp_close` → `locks_remove_posix`; `closed`, `closed_files`,
//!   `release_owner` at the process's end). An OFD lock and a flock lock belong to the open file description
//!   (`fl_owner = filp`): dup'd and inherited fds share them, and they go with its last close.
//! - **Conflicts.** POSIX and OFD locks share one list per inode and conflict when the owners differ, the
//!   ranges overlap and one of them is a write lock (`posix_locks_conflict`). flock locks are a list of their
//!   own (`flock_locks_conflict`): one lock per open file description, converted in place (the old one goes
//!   first, so a refused non-blocking conversion leaves none, as Linux).
//! - **Ranges** are inclusive, `OFFSET_MAX` = to the end. A new lock replaces what its owner held in its range
//!   and merges with the owner's adjacent or overlapping locks of the same type; an unlock splits
//!   (`posix_lock_inode`).
//! - **Waits** (F_SETLKW, F_OFD_SETLKW, flock without LOCK_NB) sleep on the inode's wait queue until the locks
//!   changed, in slices (B4), and end with EINTR when the caller is killed. A POSIX waiter that would wait for
//!   itself through a chain of waiting owners gets EDEADLK (`posix_locks_deadlock`, at most
//!   `MAX_DEADLK_ITERATIONS` steps); OFD waiters are not part of that check, as on Linux.
//! - **Lifetime.** Locks name their owner weakly. The fd-table owner is released explicitly at every close
//!   path proc and vfs-sys see; an open file description's locks are dropped lazily once it is gone (every
//!   operation on the inode first drops the locks of dead owners, and a waiter looks again every slice), so
//!   nothing in vfs has to call back at the last close.
//!
//! - **Node lifetime.** An inode's node leaves the table only when it is empty, and it is marked `dead` under
//!   its own lock in the same step (table lock held). Every operation that found a node looks at that mark
//!   after taking the node's lock and looks the inode up again if it is set, so no lock can land in a node
//!   that nobody else would see (review AB-5 finding 1).
//!
//! Locks: `proc.locks` (the inode table) > `proc.locks.node` (one inode) > `proc.locks.blocked`; none is held
//! across a wait or an allocation of another subsystem's lock.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::ab::errno_more::ENOLCK;
use uapi::errno::*;
use vfs::{Errno, FdTable, File, Inode, OpenFile};

/// `OFFSET_MAX` (`LLONG_MAX`): a lock "to the end".
pub const OFFSET_MAX: i64 = i64::MAX;
/// `MAX_DEADLK_ITERATIONS` of fs/locks.c (a bound on the walk, not a count of things).
const MAX_DEADLK_ITERATIONS: usize = 10;
/// How long a waiter sleeps before it looks again by itself (dead owners are noticed then).
const SLICE_NS: u64 = 20_000_000;

klog::counter!(LOCKS_SET, "proc.locks_set");
klog::counter!(LOCK_WAITS, "proc.lock_waits");
klog::counter!(LOCK_DEADLOCKS, "proc.lock_deadlocks");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockType {
    Read,
    Write,
    Unlock,
}

impl LockType {
    /// `F_RDLCK` 0, `F_WRLCK` 1, `F_UNLCK` 2.
    pub fn from_linux(t: i16) -> Option<LockType> {
        match t {
            0 => Some(LockType::Read),
            1 => Some(LockType::Write),
            2 => Some(LockType::Unlock),
            _ => None,
        }
    }
    pub fn linux(self) -> i16 {
        match self {
            LockType::Read => 0,
            LockType::Write => 1,
            LockType::Unlock => 2,
        }
    }
}

/// Who holds a lock (the address of the owner object, tagged so an fd table and an open file description can
/// never be taken for each other).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnerId(usize, u8);

#[derive(Clone)]
enum Alive {
    Files(Weak<FdTable>),
    File(Weak<OpenFile>),
}

impl Alive {
    fn alive(&self) -> bool {
        match self {
            Alive::Files(w) => w.strong_count() > 0,
            Alive::File(w) => w.strong_count() > 0,
        }
    }
}

/// The owner of a request.
#[derive(Clone)]
pub enum Owner {
    /// A POSIX lock of fd table `fds` made by process `pid` (F_GETLK reports it).
    Posix { fds: Arc<FdTable>, pid: u32 },
    /// An OFD lock of this open file description (F_GETLK reports pid -1).
    Ofd(Arc<OpenFile>),
}

impl Owner {
    fn id(&self) -> OwnerId {
        match self {
            Owner::Posix { fds, .. } => OwnerId(Arc::as_ptr(fds) as usize, 0),
            Owner::Ofd(f) => OwnerId(Arc::as_ptr(f) as usize, 1),
        }
    }
    fn alive(&self) -> Alive {
        match self {
            Owner::Posix { fds, .. } => Alive::Files(Arc::downgrade(fds)),
            Owner::Ofd(f) => Alive::File(Arc::downgrade(f)),
        }
    }
    fn pid(&self) -> i32 {
        match self {
            Owner::Posix { pid, .. } => *pid as i32,
            Owner::Ofd(_) => -1,
        }
    }
    fn is_posix(&self) -> bool {
        matches!(self, Owner::Posix { .. })
    }
}

fn files_owner(fds: &Arc<FdTable>) -> OwnerId {
    OwnerId(Arc::as_ptr(fds) as usize, 0)
}

fn file_id(f: &Arc<OpenFile>) -> OwnerId {
    OwnerId(Arc::as_ptr(f) as usize, 1)
}

#[derive(Clone)]
struct Lock {
    owner: OwnerId,
    alive: Alive,
    typ: LockType,
    start: i64,
    end: i64,
    pid: i32,
}

/// A lock that stands in a request's way (what F_GETLK reports).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub typ: LockType,
    pub start: i64,
    pub end: i64,
    /// The holder's process id; -1 for an OFD lock.
    pub pid: i32,
    owner: OwnerId,
}

impl Conflict {
    /// `l_len` as `posix_lock_to_flock` reports it: 0 for a lock to the end.
    pub fn len(&self) -> i64 {
        if self.end == OFFSET_MAX { 0 } else { self.end - self.start + 1 }
    }
}

struct FlockEntry {
    file: OwnerId,
    alive: Weak<OpenFile>,
    typ: LockType,
}

#[derive(Default)]
struct State {
    posix: Vec<Lock>,
    flock: Vec<FlockEntry>,
    waiters: usize,
    /// The node left the table (set under this lock while the table lock is held, when it was empty). Whoever
    /// found the node before that and locks it afterwards must look it up again: a lock put into a node that
    /// left the table would be invisible to everybody else.
    dead: bool,
}

impl State {
    /// Drops the locks whose owner is gone; true if any went.
    fn purge(&mut self) -> bool {
        let (p, f) = (self.posix.len(), self.flock.len());
        self.posix.retain(|l| l.alive.alive());
        self.flock.retain(|l| l.alive.strong_count() > 0);
        POSIX_LOCKS.fetch_sub(p - self.posix.len(), Ordering::Relaxed);
        p != self.posix.len() || f != self.flock.len()
    }
    fn empty(&self) -> bool {
        self.posix.is_empty() && self.flock.is_empty() && self.waiters == 0
    }
}

/// What keeps the inode (or the nameless object) a key stands for alive while it has locks: the key is its
/// address.
#[allow(dead_code)]
enum Keep {
    Inode(Arc<Inode>),
    Object(Arc<dyn File>),
}

struct Node {
    state: SpinLock<State>,
    /// Bumped at every change; waiters sleep until it moves.
    generation: AtomicU64,
    wq: WaitQueue,
    _keep: Keep,
}

impl Node {
    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.wq.wake_all();
    }
}

static NODES: SpinLock<BTreeMap<usize, Arc<Node>>> = SpinLock::named("proc.locks", BTreeMap::new());
/// POSIX locks held anywhere (so a process's end skips the walk when there are none).
static POSIX_LOCKS: AtomicUsize = AtomicUsize::new(0);
/// POSIX waiters (`blocked_hash` with `fl_blocker`): (waiting owner, waiter number) → the owner it waits for.
/// One entry per blocked request, so several threads of one fd table waiting at once each keep their edge.
static BLOCKED: SpinLock<BTreeMap<(OwnerId, u64), OwnerId>> = SpinLock::named("proc.locks.blocked", BTreeMap::new());
/// Numbers for BLOCKED's entries.
static WAITER_SEQ: AtomicU64 = AtomicU64::new(1);

/// The inode an open file locks (its name's inode; a nameless object such as a pipe end locks itself).
fn key_of(f: &OpenFile) -> (usize, Keep) {
    match f.inode() {
        Some(i) => (Arc::as_ptr(i) as *const u8 as usize, Keep::Inode(i.clone())),
        None => (Arc::as_ptr(&f.file) as *const u8 as usize, Keep::Object(f.file.clone())),
    }
}

fn node(f: &OpenFile, create: bool) -> Result<Option<Arc<Node>>, Errno> {
    let (key, keep) = key_of(f);
    let mut t = NODES.lock();
    if let Some(n) = t.get(&key) {
        return Ok(Some(n.clone()));
    }
    if !create {
        return Ok(None);
    }
    let n = Arc::new(Node { state: SpinLock::named("proc.locks.node", State::default()), generation: AtomicU64::new(0), wq: WaitQueue::new(), _keep: keep });
    t.insert(key, n.clone());
    Ok(Some(n))
}

/// Removes `n` from the table when it holds nothing and nobody waits on it. The node is marked dead under its
/// own lock while the table lock is held, so a caller that found it earlier sees that and looks again.
fn maybe_forget(f: &OpenFile, n: &Arc<Node>) {
    let (key, _keep) = key_of(f);
    let gone = {
        let mut t = NODES.lock();
        forget_locked(&mut t, key, n)
    };
    drop(gone);
}

/// The removal of `maybe_forget` and `release_owner` (table lock held by the caller).
fn forget_locked(t: &mut BTreeMap<usize, Arc<Node>>, key: usize, n: &Arc<Node>) -> Option<Arc<Node>> {
    if !t.get(&key).is_some_and(|x| Arc::ptr_eq(x, n)) {
        return None;
    }
    let mut s = n.state.lock();
    if !s.empty() || s.dead {
        return None;
    }
    s.dead = true;
    drop(s);
    t.remove(&key)
}

fn overlap(a: (i64, i64), b: (i64, i64)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

fn conflicts(a: LockType, b: LockType) -> bool {
    a == LockType::Write || b == LockType::Write
}

/// The first lock of another owner in the way of (`owner`, `typ`, `range`).
fn first_conflict(s: &State, owner: OwnerId, typ: LockType, range: (i64, i64)) -> Option<Conflict> {
    s.posix
        .iter()
        .find(|l| l.owner != owner && overlap((l.start, l.end), range) && conflicts(l.typ, typ))
        .map(|l| Conflict { typ: l.typ, start: l.start, end: l.end, pid: l.pid, owner: l.owner })
}

/// `owner`'s locks without `range` (an unlock, or the room a new lock takes): parts outside it stay.
fn cut(s: &mut State, owner: OwnerId, range: (i64, i64)) -> Result<bool, Errno> {
    // At most one lock splits in two: room for it first, so the change below cannot fail half-way.
    s.posix.try_reserve(1).map_err(|_| ENOLCK)?;
    let mut changed = false;
    let mut i = 0;
    while i < s.posix.len() {
        let l = &s.posix[i];
        if l.owner != owner || !overlap((l.start, l.end), range) {
            i += 1;
            continue;
        }
        changed = true;
        let (ls, le) = (l.start, l.end);
        let left = (ls < range.0).then(|| (ls, range.0 - 1));
        let right = (le > range.1).then(|| (range.1 + 1, le));
        match (left, right) {
            (None, None) => {
                s.posix.remove(i);
                POSIX_LOCKS.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            (Some(a), None) | (None, Some(a)) => {
                s.posix[i].start = a.0;
                s.posix[i].end = a.1;
            }
            (Some(a), Some(b)) => {
                let mut r = s.posix[i].clone();
                s.posix[i].start = a.0;
                s.posix[i].end = a.1;
                r.start = b.0;
                r.end = b.1;
                s.posix.insert(i + 1, r);
                POSIX_LOCKS.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        }
        i += 1;
    }
    Ok(changed)
}

/// Puts (`owner`, `typ`, `range`) in, merged with the owner's adjacent locks of the same type.
fn add(s: &mut State, lock: Lock) -> Result<(), Errno> {
    s.posix.try_reserve(1).map_err(|_| ENOLCK)?;
    let mut l = lock;
    let mut i = 0;
    while i < s.posix.len() {
        let o = &s.posix[i];
        let touches = o.owner == l.owner && o.typ == l.typ && (overlap((o.start, o.end), (l.start, l.end)) || (o.end != OFFSET_MAX && o.end + 1 == l.start) || (l.end != OFFSET_MAX && l.end + 1 == o.start));
        if touches {
            l.start = l.start.min(o.start);
            l.end = l.end.max(o.end);
            s.posix.remove(i);
            POSIX_LOCKS.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        i += 1;
    }
    let at = s.posix.iter().position(|o| o.start > l.start).unwrap_or(s.posix.len());
    s.posix.insert(at, l);
    POSIX_LOCKS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// `posix_locks_deadlock`: whether `me` waiting for `blocker` closes a circle of waiting POSIX owners. Like
/// Linux's `what_owner_is_waiting_for`, each step follows the first blocked request of the owner it reached.
#[cfg(test)]
fn deadlock(me: OwnerId, blocker: OwnerId) -> bool {
    let b = BLOCKED.lock();
    deadlock_in(&b, me, blocker)
}

fn deadlock_in(b: &BTreeMap<(OwnerId, u64), OwnerId>, me: OwnerId, blocker: OwnerId) -> bool {
    let mut cur = blocker;
    for _ in 0..=MAX_DEADLK_ITERATIONS {
        if cur == me {
            return true;
        }
        match b.range((cur, 0)..=(cur, u64::MAX)).next() {
            Some((_, &next)) => cur = next,
            None => return false,
        }
    }
    false
}

/// F_GETLK / F_OFD_GETLK (`posix_test_lock`): the first lock of another owner that conflicts with the
/// request, None when it could be set.
pub fn test(f: &OpenFile, owner: &Owner, typ: LockType, start: i64, end: i64) -> Result<Option<Conflict>, Errno> {
    loop {
        let Some(n) = node(f, false)? else { return Ok(None) };
        let r = {
            let mut s = n.state.lock();
            if s.dead {
                continue;
            }
            if s.purge() {
                n.changed();
            }
            // An unlock request "conflicts" with write locks only (`locks_conflict` of F_UNLCK).
            first_conflict(&s, owner.id(), if typ == LockType::Unlock { LockType::Read } else { typ }, (start, end))
        };
        maybe_forget(f, &n);
        return Ok(r);
    }
}

/// F_SETLK/F_SETLKW and the OFD variants (`posix_lock_inode` + `do_lock_file_wait`): sets or clears the
/// owner's lock on `[start, end]` of `f`'s inode. EAGAIN when another owner's lock is in the way and `wait` is
/// false; with `wait` it sleeps until it can (EDEADLK for a POSIX circle, EINTR once `interrupted`).
pub fn set(f: &OpenFile, owner: &Owner, typ: LockType, start: i64, end: i64, wait: bool, interrupted: &dyn Fn() -> bool) -> Result<(), Errno> {
    let me = owner.id();
    let mut edge = Edge(None);
    loop {
        let Some(n) = node(f, typ != LockType::Unlock)? else { return Ok(()) };
        let seen = n.generation.load(Ordering::Acquire);
        let r = {
            let mut s = n.state.lock();
            if s.dead {
                continue;
            }
            let mut changed = s.purge();
            let r = if typ == LockType::Unlock {
                cut(&mut s, me, (start, end)).map(|c| {
                    changed |= c;
                    None
                })
            } else if let Some(c) = first_conflict(&s, me, typ, (start, end)) {
                Ok(Some(c))
            } else {
                // Room first (a split and the new lock), then the change: nothing fails half-way.
                match s.posix.try_reserve(2) {
                    Err(_) => Err(ENOLCK),
                    Ok(()) => cut(&mut s, me, (start, end)).and_then(|_| add(&mut s, Lock { owner: me, alive: owner.alive(), typ, start, end, pid: owner.pid() })).map(|_| {
                        changed = true;
                        None
                    }),
                }
            };
            if matches!(r, Ok(Some(_))) && wait {
                s.waiters += 1;
            }
            if changed {
                n.changed();
            }
            r
        };
        match r {
            Err(e) => {
                maybe_forget(f, &n);
                return Err(e);
            }
            Ok(None) => {
                if typ != LockType::Unlock {
                    LOCKS_SET.inc();
                }
                maybe_forget(f, &n);
                return Ok(());
            }
            Ok(Some(_)) if !wait => {
                maybe_forget(f, &n);
                return Err(EAGAIN);
            }
            Ok(Some(c)) => {
                let res = wait_for_change(&n, seen, me, owner.is_posix().then_some(c.owner), &mut edge, interrupted);
                n.state.lock().waiters -= 1;
                maybe_forget(f, &n);
                res?;
            }
        }
    }
}

/// A blocked POSIX request's edge in BLOCKED (`fl_blocker` in `blocked_hash`). It stays while the request
/// sleeps through its slices and leaves when the inode's locks changed (Linux unlinks a waiter when it is
/// woken) or the request ends. INT-A2a2: it used to leave after every 20 ms slice, and an owner that asked in
/// that gap missed the circle, waited for the waiter and never woke (`ab.ab5.record_locks`, gate hang).
struct Edge(Option<(OwnerId, u64)>);

impl Edge {
    fn clear(&mut self, m: &mut BTreeMap<(OwnerId, u64), OwnerId>) {
        if let Some(k) = self.0.take() {
            m.remove(&k);
        }
    }
}

impl Drop for Edge {
    fn drop(&mut self) {
        if self.0.is_some() {
            self.clear(&mut BLOCKED.lock());
        }
    }
}

/// One wait of a blocked request: EDEADLK when a POSIX owner would wait for itself, EINTR once the caller is
/// killed, else Ok after the inode's locks changed or a slice passed. A POSIX request's `edge` names the owner
/// it waits for now; it is kept across slices and dropped when the locks changed (the caller looks again).
fn wait_for_change(n: &Node, seen: u64, me: OwnerId, posix_blocker: Option<OwnerId>, edge: &mut Edge, interrupted: &dyn Fn() -> bool) -> Result<(), Errno> {
    if let Some(b) = posix_blocker {
        // The check and the insertion under one lock: two owners blocking on each other at the same moment
        // cannot both miss the circle.
        let mut m = BLOCKED.lock();
        if deadlock_in(&m, me, b) {
            edge.clear(&mut m);
            drop(m);
            LOCK_DEADLOCKS.inc();
            klog::event!(Debug, "proc", "lock.deadlock"; "a POSIX lock wait would wait for itself: EDEADLK");
            return Err(EDEADLK);
        }
        let key = *edge.0.get_or_insert_with(|| (me, WAITER_SEQ.fetch_add(1, Ordering::Relaxed)));
        m.insert(key, b);
    }
    LOCK_WAITS.inc();
    n.wq.wait_until_timeout(|| n.generation.load(Ordering::Acquire) != seen || interrupted(), SLICE_NS);
    if n.generation.load(Ordering::Acquire) != seen && edge.0.is_some() {
        edge.clear(&mut BLOCKED.lock());
    }
    if interrupted() {
        return Err(EINTR);
    }
    Ok(())
}

/// flock(2) (`flock_lock_inode` + `locks_lock_file_wait`): one lock per open file description. Asking for the
/// type it holds changes nothing; another type (or an unlock) first drops the one it holds, so a refused
/// non-blocking conversion leaves none (Linux). EAGAIN (EWOULDBLOCK) without `wait` when another description's
/// lock is in the way, else a wait (EINTR once `interrupted`).
pub fn flock(f: &Arc<OpenFile>, typ: LockType, wait: bool, interrupted: &dyn Fn() -> bool) -> Result<(), Errno> {
    let me = file_id(f);
    // A node that left the table meanwhile was empty: it holds no lock of this open file either.
    if let Some(n) = node(f, false)? {
        let (same, removed) = {
            let mut s = n.state.lock();
            let purged = s.purge();
            match s.flock.iter().position(|e| e.file == me) {
                Some(i) if s.flock[i].typ == typ => (true, purged),
                Some(i) => {
                    s.flock.remove(i);
                    (false, true)
                }
                None => (false, purged),
            }
        };
        if removed {
            n.changed();
        }
        if same || typ == LockType::Unlock {
            maybe_forget(f, &n);
            return Ok(());
        }
    } else if typ == LockType::Unlock {
        return Ok(());
    }
    loop {
        let Some(n) = node(f, true)? else { return Err(ENOLCK) };
        let seen = n.generation.load(Ordering::Acquire);
        let r = {
            let mut s = n.state.lock();
            if s.dead {
                continue;
            }
            let mut changed = s.purge();
            let r = if s.flock.iter().any(|e| e.file != me && conflicts(e.typ, typ)) {
                if wait {
                    s.waiters += 1;
                }
                Ok(true)
            } else if s.flock.try_reserve(1).is_err() {
                Err(ENOLCK)
            } else {
                s.flock.push(FlockEntry { file: me, alive: Arc::downgrade(f), typ });
                changed = true;
                Ok(false)
            };
            if changed {
                n.changed();
            }
            r
        };
        match r {
            Err(e) => {
                maybe_forget(f, &n);
                return Err(e);
            }
            Ok(false) => {
                LOCKS_SET.inc();
                return Ok(());
            }
            Ok(true) if !wait => {
                maybe_forget(f, &n);
                return Err(EAGAIN);
            }
            Ok(true) => {
                let res = wait_for_change(&n, seen, me, None, &mut Edge(None), interrupted);
                n.state.lock().waiters -= 1;
                maybe_forget(f, &n);
                res?;
            }
        }
    }
}

/// A close of an fd of `f` by fd table `fds` (`filp_close` → `locks_remove_posix`): the table's POSIX locks
/// on that inode go.
pub fn closed(fds: &Arc<FdTable>, f: &OpenFile) {
    if POSIX_LOCKS.load(Ordering::Relaxed) == 0 {
        return;
    }
    loop {
        let Ok(Some(n)) = node(f, false) else { return };
        let changed = {
            let mut s = n.state.lock();
            if s.dead {
                continue;
            }
            let before = s.posix.len();
            let me = files_owner(fds);
            s.posix.retain(|l| l.owner != me);
            let gone = before - s.posix.len();
            POSIX_LOCKS.fetch_sub(gone, Ordering::Relaxed);
            gone > 0 || s.purge()
        };
        if changed {
            n.changed();
        }
        maybe_forget(f, &n);
        return;
    }
}

/// `closed` for each of `files` (execve's close-on-exec).
pub fn closed_files(fds: &Arc<FdTable>, files: &[Arc<OpenFile>]) {
    for f in files {
        closed(fds, f);
    }
}

/// The open files of `fds` that close-on-exec will close (taken before it does).
pub fn cloexec_files(fds: &FdTable) -> Vec<Arc<OpenFile>> {
    let mut v = Vec::new();
    if POSIX_LOCKS.load(Ordering::Relaxed) == 0 {
        return v;
    }
    for fd in 0..fds.capacity() {
        if fds.cloexec(fd) == Ok(true)
            && let Ok(f) = fds.get(fd)
            && v.try_reserve(1).is_ok()
        {
            v.push(f);
        }
    }
    v
}

/// The end of fd table `fds` (`exit_files` → every `filp_close`): all its POSIX locks go.
pub fn release_owner(fds: &Arc<FdTable>) {
    if POSIX_LOCKS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let me = files_owner(fds);
    let nodes: Vec<(usize, Arc<Node>)> = {
        let t = NODES.lock();
        let mut v = Vec::new();
        if v.try_reserve(t.len()).is_err() {
            klog::event!(Warn, "proc", "locks.release_no_memory"; "no memory to walk the lock table: the dead owner's locks go lazily");
            return;
        }
        v.extend(t.iter().map(|(k, n)| (*k, n.clone())));
        v
    };
    for (key, n) in nodes {
        let (changed, empty) = {
            let mut s = n.state.lock();
            let before = s.posix.len();
            s.posix.retain(|l| l.owner != me);
            let gone = before - s.posix.len();
            POSIX_LOCKS.fetch_sub(gone, Ordering::Relaxed);
            (gone > 0, s.empty())
        };
        if changed {
            n.changed();
        }
        if empty {
            let gone = {
                let mut t = NODES.lock();
                forget_locked(&mut t, key, &n)
            };
            drop(gone);
        }
    }
}

/// How many inodes have locks or waiters (tests and MCP).
pub fn inode_count() -> usize {
    NODES.lock().len()
}

/// POSIX lock requests blocked right now (the edges of the deadlock check; tests).
pub fn blocked_requests() -> usize {
    BLOCKED.lock().len()
}

/// POSIX locks held system-wide (tests).
pub fn posix_count() -> usize {
    POSIX_LOCKS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn st(locks: &[(usize, LockType, i64, i64)]) -> State {
        let mut s = State::default();
        POSIX_LOCKS.fetch_add(locks.len(), Ordering::Relaxed);
        for &(o, t, a, b) in locks {
            s.posix.push(Lock { owner: OwnerId(o, 0), alive: Alive::Files(Weak::new()), typ: t, start: a, end: b, pid: o as i32 });
        }
        s
    }

    fn ranges(s: &State, o: usize) -> Vec<(LockType, i64, i64)> {
        s.posix.iter().filter(|l| l.owner == OwnerId(o, 0)).map(|l| (l.typ, l.start, l.end)).collect()
    }

    fn lock(o: usize, t: LockType, a: i64, b: i64) -> Lock {
        Lock { owner: OwnerId(o, 0), alive: Alive::Files(Weak::new()), typ: t, start: a, end: b, pid: o as i32 }
    }

    #[test]
    fn unlock_splits_and_trims() {
        let mut s = st(&[(1, LockType::Write, 0, 99)]);
        assert!(cut(&mut s, OwnerId(1, 0), (10, 19)).unwrap());
        assert_eq!(ranges(&s, 1), vec![(LockType::Write, 0, 9), (LockType::Write, 20, 99)]);
        assert!(cut(&mut s, OwnerId(1, 0), (0, 4)).unwrap());
        assert_eq!(ranges(&s, 1), vec![(LockType::Write, 5, 9), (LockType::Write, 20, 99)]);
        assert!(!cut(&mut s, OwnerId(2, 0), (0, OFFSET_MAX)).unwrap(), "another owner's unlock touches nothing");
        assert!(cut(&mut s, OwnerId(1, 0), (0, OFFSET_MAX)).unwrap());
        assert!(s.posix.is_empty());
    }

    #[test]
    fn same_type_merges_other_type_replaces() {
        let mut s = State::default();
        add(&mut s, lock(1, LockType::Read, 0, 9)).unwrap();
        add(&mut s, lock(1, LockType::Read, 10, 19)).unwrap();
        assert_eq!(ranges(&s, 1), vec![(LockType::Read, 0, 19)], "adjacent read locks merge");
        cut(&mut s, OwnerId(1, 0), (5, 9)).unwrap();
        add(&mut s, lock(1, LockType::Write, 5, 9)).unwrap();
        assert_eq!(ranges(&s, 1), vec![(LockType::Read, 0, 4), (LockType::Write, 5, 9), (LockType::Read, 10, 19)]);
        add(&mut s, lock(1, LockType::Write, 20, OFFSET_MAX)).unwrap();
        assert_eq!(s.posix.last().map(|l| (l.start, l.end)), Some((20, OFFSET_MAX)));
    }

    #[test]
    fn conflicts_need_another_owner_overlap_and_a_writer() {
        let s = st(&[(1, LockType::Read, 0, 9), (2, LockType::Write, 100, OFFSET_MAX)]);
        assert_eq!(first_conflict(&s, OwnerId(3, 0), LockType::Read, (0, 50)), None, "readers share");
        assert_eq!(first_conflict(&s, OwnerId(1, 0), LockType::Write, (0, 9)), None, "own locks never conflict");
        let c = first_conflict(&s, OwnerId(3, 0), LockType::Write, (5, 5)).unwrap();
        assert_eq!((c.typ, c.start, c.end, c.pid, c.len()), (LockType::Read, 0, 9, 1, 10));
        let c = first_conflict(&s, OwnerId(3, 0), LockType::Read, (1000, 1000)).unwrap();
        assert_eq!((c.typ, c.len()), (LockType::Write, 0));
        assert_eq!(first_conflict(&s, OwnerId(3, 0), LockType::Write, (10, 99)), None, "the gap is free");
    }

    #[test]
    fn dead_owners_are_purged() {
        let table = Arc::new(FdTable::new());
        let mut s = State::default();
        POSIX_LOCKS.fetch_add(2, Ordering::Relaxed);
        s.posix.push(Lock { owner: files_owner(&table), alive: Alive::Files(Arc::downgrade(&table)), typ: LockType::Write, start: 0, end: 1, pid: 5 });
        s.posix.push(lock(9, LockType::Write, 2, 3));
        assert!(s.purge(), "the lock without a live owner went");
        assert_eq!(s.posix.len(), 1);
        drop(table);
        assert!(s.purge());
        assert!(s.posix.is_empty());
    }

    #[test]
    fn deadlock_walk() {
        let (a, b, c) = (OwnerId(0xa, 0), OwnerId(0xb, 0), OwnerId(0xc, 0));
        BLOCKED.lock().insert((b, 1), c);
        BLOCKED.lock().insert((c, 2), a);
        assert!(deadlock(a, b), "a waits for b, b for c, c for a");
        assert!(!deadlock(OwnerId(0xd, 0), b));
        BLOCKED.lock().remove(&(b, 1));
        BLOCKED.lock().remove(&(c, 2));
    }

    #[test]
    fn two_waiters_of_one_owner_keep_their_edges() {
        // Two threads of fd table x wait at once: one for y, one for z (review finding 2: the second used to
        // overwrite the first, and whichever woke first removed the only edge).
        let (x, y, z) = (OwnerId(0x10, 0), OwnerId(0x20, 0), OwnerId(0x30, 0));
        let mut m = BTreeMap::new();
        m.insert((x, 1), y);
        m.insert((x, 2), z);
        m.insert((y, 3), x);
        assert!(deadlock_in(&m, y, x), "y waiting for x: x waits (first request) for y");
        m.remove(&(x, 1));
        assert!(!deadlock_in(&m, y, x), "x's first request ended: its other request waits for z, not y");
        assert!(deadlock_in(&m, z, x), "z waiting for x closes the circle through x's remaining request");
    }

    struct Dummy;
    impl vfs::File for Dummy {
        fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
            Ok(0)
        }
        fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
            Ok(buf.len())
        }
        fn mode(&self) -> u32 {
            0o100644
        }
    }

    #[test]
    fn a_forgotten_node_is_dead_and_never_takes_locks() {
        let f = OpenFile::anon(Arc::new(Dummy), 2);
        let (key, _) = key_of(&f);
        let stale = node(&f, true).unwrap().unwrap();
        maybe_forget(&f, &stale);
        assert!(stale.state.lock().dead, "the empty node left the table and is marked");
        assert!(!NODES.lock().contains_key(&key));
        let fds = Arc::new(FdTable::new());
        let owner = Owner::Posix { fds: fds.clone(), pid: 7 };
        set(&f, &owner, LockType::Write, 0, 9, false, &|| false).unwrap();
        assert!(stale.state.lock().posix.is_empty(), "the lock went into a live node");
        let live = node(&f, false).unwrap().expect("a live node holds the lock");
        assert!(!Arc::ptr_eq(&live, &stale));
        let other = Owner::Posix { fds: Arc::new(FdTable::new()), pid: 8 };
        assert_eq!(test(&f, &other, LockType::Read, 5, 5).unwrap().map(|c| c.pid), Some(7), "another owner sees it");
        // A node that is not empty is never forgotten.
        maybe_forget(&f, &live);
        assert!(!live.state.lock().dead && NODES.lock().contains_key(&key));
        closed(&fds, &f);
        assert!(!NODES.lock().contains_key(&key), "the last lock's release removes the node");
        assert!(live.state.lock().dead);
    }
}
