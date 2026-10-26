//! Priority passing (SC-7b, B4: "kernel mutexes pass on priority"; V2-PLAN §1a; Linux linux-6.6/kernel/locking/
//! rtmutex.c `rt_mutex_adjust_prio_chain`, kernel/sched/core.c `rt_mutex_setprio`).
//!
//! A thread that sleeps for a lock another thread holds lends its class to the holder while it holds: the
//! holder runs in the most urgent class of all waiters of all locks it holds (`attr::boosted_class`: only among
//! the fair classes, KernelRt never changes, nothing is lent above Interactive). If the holder itself waits for a
//! lock, the lent class goes on to that lock's holder, and so on (the chain walk, bounded by `MAX_DEPTH`).
//! Locks are kernel `Mutex`es (through `ksync::SchedHooks::pi_*`) and PI futexes (`futex::pi`).
//!
//! **The book** (`Book`, host-tested): the waiters (thread, lock, own class, arrival) and, for every lock that
//! has waiters, its owner (Linux's `pi_state` lives exactly as long). Boosts are derived: a thread's boost is the
//! most urgent donation among the waiters of the locks it owns; a waiter's donation is its own class or its boost,
//! whichever is more urgent. Every change re-derives the boosts along the chain from the changed lock and stops
//! where nothing changes. One global spin lock (`sched.pi`) guards the book, so a chain walk takes no further lock
//! of a lock class and cannot invert any order (it nests inside the futex bucket locks and above the scheduler's
//! thread table, which it reads). It is only taken on contended paths.
//!
//! **Applying a boost**: the book writes `Thread::boost`; after its lock is dropped every thread whose boost
//! changed is refreshed (`attr::refresh`: `reweight` is the single writer of `Thread::class`, so policy and grant
//! changes during a boost stay right). Refreshes read the latest boost, so their order does not matter.

use crate::futex::Key;
use crate::{Thread, attr, table};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use ksync::{SpinLock, SpinLockGuard};

/// `Thread::boost` when nothing is lent.
pub const NO_BOOST: u8 = u8::MAX;
/// Linux's `max_lock_depth`: a chain longer than this is not walked further (an input bound against hostile
/// chains, not a count limit); a futex PI wait whose owner chain is longer answers EDEADLK as Linux does.
pub const MAX_DEPTH: u32 = 1024;
/// The most urgent class a donation can carry (Interactive).
const TOP: u8 = 1;

/// A lock that passes on priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockId {
    /// A kernel `Mutex` (its address).
    Mutex(usize),
    /// A PI futex (its key).
    Futex(Key),
}

/// The holder of a lock: scheduler id and (PI futexes) the TID programs see in the futex word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub id: u64,
    pub tid: u32,
}

#[derive(Debug)]
struct Waiter {
    who: Owner,
    lock: LockId,
    /// Its class without boost (`Class` as u8).
    own: u8,
    /// What it lends now: `own` or its boost, the more urgent, at most Interactive (kept current by the walks).
    don: u8,
    seq: u64,
}

#[derive(Debug)]
struct Rec {
    lock: LockId,
    owner: Option<Owner>,
    /// The waiters as (donation, arrival, thread id), most urgent first.
    q: Vec<(u8, u64, u64)>,
}

/// Where the book reads and writes boosts (the threads in the kernel, a map in the host tests).
pub trait Boosts {
    fn get(&self, id: u64) -> u8;
    fn set(&mut self, id: u64, boost: u8);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitError {
    NoMemory,
    /// The owner chain leads back to the waiter (or is longer than `MAX_DEPTH`).
    Deadlock,
}

/// The waiters and owners of contended locks (module doc). Three sorted vectors, so every step of a walk is a
/// binary search (the sched.pi hold time grows with the log of the waiters, not with their number; SC-7b fixer):
/// `waiters` by thread, `recs` by lock, and `tops`, one entry per owned record with its top donation, by owner
/// (a thread's boost is its first entry there). Memory is reserved before anything changes, so a wait either
/// fails with NoMemory untouched or completes; the other operations only shrink or move entries in place.
pub struct Book {
    waiters: Vec<Waiter>,
    recs: Vec<Rec>,
    /// (owner, top donation, its arrival, lock).
    tops: Vec<(u64, u8, u64, LockId)>,
    seq: u64,
    /// Chain walks cut at `MAX_DEPTH`.
    pub cut: u64,
}

impl Default for Book {
    fn default() -> Self {
        Self::new()
    }
}

impl Book {
    pub const fn new() -> Book {
        Book { waiters: Vec::new(), recs: Vec::new(), tops: Vec::new(), seq: 0, cut: 0 }
    }
    fn wi(&self, id: u64) -> Result<usize, usize> {
        self.waiters.binary_search_by_key(&id, |w| w.who.id)
    }
    fn ri(&self, lock: LockId) -> Result<usize, usize> {
        self.recs.binary_search_by_key(&lock, |r| r.lock)
    }
    fn rec(&self, lock: LockId) -> Option<&Rec> {
        self.ri(lock).ok().map(|i| &self.recs[i])
    }
    fn owner_id(&self, lock: LockId) -> Option<u64> {
        self.rec(lock).and_then(|r| r.owner).map(|o| o.id)
    }
    fn top_entry(r: &Rec) -> Option<(u64, u8, u64, LockId)> {
        let (o, &(d, s, _)) = (r.owner?, r.q.first()?);
        Some((o.id, d, s, r.lock))
    }
    /// Takes record `ri`'s entry out of `tops` (before its owner or its first waiter changes).
    fn untop(&mut self, ri: usize) {
        if let Some(e) = Self::top_entry(&self.recs[ri]) {
            if let Ok(i) = self.tops.binary_search(&e) {
                self.tops.remove(i);
            }
        }
    }
    /// Puts record `ri`'s entry back (capacity for one per record is reserved in `wait`).
    fn retop(&mut self, ri: usize) {
        if let Some(e) = Self::top_entry(&self.recs[ri]) {
            let i = self.tops.partition_point(|x| *x < e);
            self.tops.insert(i, e);
        }
    }
    /// The boost `id` should have now: the first of its `tops`.
    fn boost_of(&self, id: u64) -> u8 {
        let i = self.tops.partition_point(|e| e.0 < id);
        self.tops.get(i).filter(|e| e.0 == id).map_or(NO_BOOST, |e| e.1)
    }
    /// Waiter `wi` lends `don` from now on: its place in its record's queue moves.
    fn set_don(&mut self, wi: usize, don: u8) {
        let (id, seq, lock, old) = {
            let w = &mut self.waiters[wi];
            let old = core::mem::replace(&mut w.don, don);
            (w.who.id, w.seq, w.lock, old)
        };
        let Ok(ri) = self.ri(lock) else { return };
        self.untop(ri);
        let q = &mut self.recs[ri].q;
        if let Ok(i) = q.binary_search(&(old, seq, id)) {
            q.remove(i);
        }
        let i = q.partition_point(|x| *x < (don, seq, id));
        q.insert(i, (don, seq, id));
        self.retop(ri);
    }
    /// Re-derives `id`'s boost and walks on while it changes (to the owner of the lock `id` waits for).
    fn settle(&mut self, bx: &mut dyn Boosts, mut id: u64) {
        for _ in 0..MAX_DEPTH {
            let b = self.boost_of(id);
            if b == bx.get(id) {
                return;
            }
            bx.set(id, b);
            let Ok(wi) = self.wi(id) else { return };
            let w = &self.waiters[wi];
            let don = w.own.min(b).max(TOP);
            if don == w.don {
                return;
            }
            let lock = w.lock;
            self.set_don(wi, don);
            match self.owner_id(lock) {
                Some(o) => id = o,
                None => return,
            }
        }
        self.cut += 1;
    }
    /// Whether `me` waiting for a lock held by `owner` closes a cycle (Linux's deadlock detection).
    pub fn would_deadlock(&self, me: u64, owner: Option<Owner>) -> bool {
        let mut o = owner.map(|o| o.id);
        for _ in 0..MAX_DEPTH {
            let Some(id) = o else { return false };
            if id == me {
                return true;
            }
            o = self.wi(id).ok().and_then(|i| self.owner_id(self.waiters[i].lock));
        }
        true
    }
    /// `who` (own class `own`) starts waiting for `lock`, held by `owner` when the lock has no record yet (a
    /// record's owner is kept current by `set_owner`). `detect`: refuse a wait that closes a cycle.
    pub fn wait(&mut self, bx: &mut dyn Boosts, lock: LockId, who: Owner, own: u8, owner: Option<Owner>, detect: bool) -> Result<(), WaitError> {
        // A thread waits for one lock at a time; a stale entry would corrupt the queues.
        self.leave(bx, who.id);
        let ri = self.ri(lock);
        let owner = match ri {
            Ok(i) => self.recs[i].owner.or(owner),
            Err(_) => owner,
        };
        if detect && self.would_deadlock(who.id, owner) {
            return Err(WaitError::Deadlock);
        }
        let recs = self.recs.len() + usize::from(ri.is_err());
        let mut fresh = Vec::new();
        let room = self.waiters.try_reserve(1).is_ok()
            && self.tops.try_reserve(recs.saturating_sub(self.tops.len())).is_ok()
            && match ri {
                Ok(i) => self.recs[i].q.try_reserve(1).is_ok(),
                Err(_) => self.recs.try_reserve(1).is_ok() && fresh.try_reserve(1).is_ok(),
            };
        if !room {
            return Err(WaitError::NoMemory);
        }
        let ri = match ri {
            Ok(i) => {
                self.untop(i);
                self.recs[i].owner = owner;
                i
            }
            Err(i) => {
                self.recs.insert(i, Rec { lock, owner, q: fresh });
                i
            }
        };
        self.seq += 1;
        let don = own.min(bx.get(who.id)).max(TOP);
        let e = (don, self.seq, who.id);
        let q = &mut self.recs[ri].q;
        let at = q.partition_point(|x| *x < e);
        q.insert(at, e);
        self.retop(ri);
        let wi = self.wi(who.id).unwrap_or_else(|i| i);
        self.waiters.insert(wi, Waiter { who, lock, own, don, seq: self.seq });
        if let Some(o) = owner {
            self.settle(bx, o.id);
        }
        Ok(())
    }
    /// `id` stops waiting (it gave up, or is about to own the lock): its donation leaves the owner; the record
    /// goes with the last waiter. Returns the lock it waited for.
    pub fn leave(&mut self, bx: &mut dyn Boosts, id: u64) -> Option<LockId> {
        let wi = self.wi(id).ok()?;
        let w = self.waiters.remove(wi);
        let Ok(ri) = self.ri(w.lock) else { return Some(w.lock) };
        self.untop(ri);
        let owner = self.recs[ri].owner.map(|o| o.id);
        let q = &mut self.recs[ri].q;
        if let Ok(i) = q.binary_search(&(w.don, w.seq, id)) {
            q.remove(i);
        }
        if q.is_empty() {
            self.recs.remove(ri);
        } else {
            self.retop(ri);
        }
        if let Some(o) = owner {
            self.settle(bx, o);
        }
        Some(w.lock)
    }
    /// `lock` changed hands (`None`: released and not taken yet). Only locks with waiters have a record.
    pub fn set_owner(&mut self, bx: &mut dyn Boosts, lock: LockId, new: Option<Owner>) {
        let Ok(ri) = self.ri(lock) else { return };
        if self.recs[ri].owner.map(|o| o.id) == new.map(|o| o.id) {
            self.recs[ri].owner = new;
            return;
        }
        self.untop(ri);
        let old = core::mem::replace(&mut self.recs[ri].owner, new);
        self.retop(ri);
        if let Some(o) = old {
            self.settle(bx, o.id);
        }
        if let Some(n) = new {
            self.settle(bx, n.id);
        }
    }
    /// `id` released `lock`: the record loses its owner if that is still `id`.
    pub fn released(&mut self, bx: &mut dyn Boosts, lock: LockId, id: u64) {
        if self.owner_id(lock) == Some(id) {
            self.set_owner(bx, lock, None);
        }
    }
    /// The waiter the lock goes to next: the most urgent donation, the earliest among equals.
    pub fn top(&self, lock: LockId) -> Option<Owner> {
        let &(_, _, id) = self.rec(lock)?.q.first()?;
        self.wi(id).ok().map(|i| self.waiters[i].who)
    }
    /// The lock's record: None without waiters, else its owner (None: released, not taken yet).
    pub fn owner(&self, lock: LockId) -> Option<Option<Owner>> {
        self.rec(lock).map(|r| r.owner)
    }
    pub fn waiting(&self, id: u64) -> Option<LockId> {
        self.wi(id).ok().map(|i| self.waiters[i].lock)
    }
    pub fn waiter_count(&self, lock: LockId) -> usize {
        self.rec(lock).map_or(0, |r| r.q.len())
    }
    /// `id`'s own class changed while it waits: its donation follows.
    pub fn adjust(&mut self, bx: &mut dyn Boosts, id: u64, own: u8) {
        let Ok(wi) = self.wi(id) else { return };
        let w = &mut self.waiters[wi];
        if w.own == own {
            return;
        }
        w.own = own;
        let (lock, don, old) = (w.lock, own.min(bx.get(id)).max(TOP), w.don);
        if don == old {
            return;
        }
        self.set_don(wi, don);
        if let Some(o) = self.owner_id(lock) {
            self.settle(bx, o);
        }
    }
    /// The locks `id` owns that have waiters.
    pub fn owned_by(&self, id: u64) -> impl Iterator<Item = LockId> + '_ {
        let s = self.tops.partition_point(|e| e.0 < id);
        let e = self.tops.partition_point(|e| e.0 <= id);
        self.tops[s..e].iter().map(|e| e.3)
    }
    /// Thread `id` ends: it stops waiting, its records lose their owner, its boost ends.
    pub fn forget(&mut self, bx: &mut dyn Boosts, id: u64) {
        self.leave(bx, id);
        // Each step takes one entry of `id` out of `tops`, so this ends.
        loop {
            let Some(l) = self.owned_by(id).next() else { break };
            self.set_owner(bx, l, None);
        }
        if bx.get(id) != NO_BOOST {
            bx.set(id, NO_BOOST);
        }
    }
    /// Waiters recorded now.
    pub fn len(&self) -> usize {
        self.waiters.len()
    }
    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }
}

// ---- the kernel's book ----

static BOOK: SpinLock<Book> = SpinLock::named("sched.pi", Book::new());
/// Records in the book (kept by `Locked`'s drop): thread exit skips the lock while there are none.
static RECS: AtomicUsize = AtomicUsize::new(0);

/// The book, locked.
pub(crate) struct Locked(SpinLockGuard<'static, Book>);

pub(crate) fn book() -> Locked {
    Locked(BOOK.lock())
}

impl Drop for Locked {
    fn drop(&mut self) {
        RECS.store(self.0.recs.len(), Ordering::Release);
    }
}
impl core::ops::Deref for Locked {
    type Target = Book;
    fn deref(&self) -> &Book {
        &self.0
    }
}
impl core::ops::DerefMut for Locked {
    fn deref_mut(&mut self) -> &mut Book {
        &mut self.0
    }
}

klog::counter!(BOOSTS, "sched.pi.boosts");
klog::counter!(UNBOOSTS, "sched.pi.unboosts");
klog::counter!(DEADLOCKS, "sched.pi.deadlocks");
klog::counter!(NO_MEMORY, "sched.pi.no_memory");

/// Boosts on the live threads; collects the changed ones for `apply`.
pub(crate) struct Live {
    changed: Vec<(Arc<Thread>, u8, u8)>,
    /// Some change could not be noted (no memory): those threads carry `pi_stale`.
    missed: bool,
}

/// Changes noted without allocating under the book's lock (a walk changes one or two threads, rarely more).
const NOTED: usize = 4;

impl Live {
    /// Call before taking the book's lock: the room for the usual changes is reserved here.
    pub(crate) fn new() -> Live {
        let mut changed = Vec::new();
        let _ = changed.try_reserve(NOTED);
        Live { changed, missed: false }
    }
    /// Refreshes every thread whose boost changed (call with no `sched.pi` or bucket lock held).
    pub(crate) fn apply(self) {
        for (t, old, new) in self.changed {
            attr::refresh(&t);
            if new == NO_BOOST {
                UNBOOSTS.inc();
                klog::event!(Debug, "sched", "pi.unboost", id = t.id, name = t.name.as_str(), class = t.class().name());
            } else if old == NO_BOOST || new < old {
                BOOSTS.inc();
                klog::event!(Debug, "sched", "pi.boost", id = t.id, name = t.name.as_str(), lent = new, class = t.class().name());
            }
        }
        if self.missed {
            // Fallback: find the marked threads in the table (ids ascend, so the walk ends) and refresh them.
            let mut n = 0u64;
            let mut after = 0;
            while let Some(t) = table::next_where(after, |t| t.pi_stale.load(Ordering::Acquire)) {
                after = t.id;
                t.pi_stale.store(false, Ordering::Release);
                attr::refresh(&t);
                n += 1;
            }
            klog::event!(Warn, "sched", "pi.refresh_fallback", threads = n; "no memory to note boost changes: the marked threads were found in the thread table and refreshed");
        }
    }
}

impl Boosts for Live {
    fn get(&self, id: u64) -> u8 {
        table::get(id).map_or(NO_BOOST, |t| t.boost.load(Ordering::Acquire))
    }
    fn set(&mut self, id: u64, b: u8) {
        let Some(t) = table::get(id) else { return };
        let old = t.boost.swap(b, Ordering::AcqRel);
        if self.changed.try_reserve(1).is_ok() {
            self.changed.push((t, old, b));
        } else {
            NO_MEMORY.inc();
            t.pi_stale.store(true, Ordering::Release);
            self.missed = true;
        }
    }
}

/// Its class without boost, for the book.
pub(crate) fn own_of(t: &Thread) -> u8 {
    t.own.load(Ordering::Relaxed)
}

const NO_OWNER: u64 = u64::MAX;

/// `SchedHooks::pi_wait`.
pub(crate) fn mutex_wait(lock: usize, owner: &AtomicU64) -> bool {
    let Some(me) = crate::current_thread() else { return false };
    if me.is_idle() {
        return false;
    }
    let mut live = Live::new();
    let r = {
        let mut b = book();
        let o = owner.load(Ordering::SeqCst);
        let o = (o != NO_OWNER && o != me.id).then_some(Owner { id: o, tid: 0 });
        b.wait(&mut live, LockId::Mutex(lock), Owner { id: me.id, tid: 0 }, own_of(&me), o, false)
    };
    live.apply();
    if r == Err(WaitError::NoMemory) {
        NO_MEMORY.inc();
    }
    r.is_ok()
}

/// `SchedHooks::pi_acquired`.
pub(crate) fn mutex_acquired(lock: usize, waited: bool) {
    let me = crate::current_id();
    let mut live = Live::new();
    {
        let mut b = book();
        if waited {
            b.leave(&mut live, me);
        }
        b.set_owner(&mut live, LockId::Mutex(lock), Some(Owner { id: me, tid: 0 }));
    }
    live.apply();
}

/// `SchedHooks::pi_gave_up`.
pub(crate) fn mutex_gave_up(_lock: usize) {
    let mut live = Live::new();
    book().leave(&mut live, crate::current_id());
    live.apply();
}

/// `SchedHooks::pi_released`.
pub(crate) fn mutex_released(lock: usize) {
    let mut live = Live::new();
    book().released(&mut live, LockId::Mutex(lock), crate::current_id());
    live.apply();
}

/// `t`'s class changed (`attr::class_changed`): if it waits, its donation follows (Linux `rt_mutex_adjust_pi`).
pub(crate) fn class_changed(t: &Thread) {
    let mut live = Live::new();
    {
        let mut b = book();
        if b.waiting(t.id).is_none() {
            return;
        }
        b.adjust(&mut live, t.id, own_of(t));
    }
    live.apply();
}

/// Thread `t` ends (`sched::exit`): PI futexes it owns go to their top waiters (`futex::pi::owner_exits`), then
/// it leaves the book (a kernel mutex held at exit is a bug the lock checker reports; its waiters just lose the
/// owner they lent to).
pub(crate) fn thread_exits(t: &Arc<Thread>) {
    crate::futex::pi::owner_exits(t);
    if RECS.load(Ordering::Acquire) == 0 && t.boost.load(Ordering::Acquire) == NO_BOOST {
        return;
    }
    let mut live = Live::new();
    book().forget(&mut live, t.id);
    live.apply();
}

/// Waiters recorded now (ktests, MCP).
pub fn waiters() -> usize {
    book().len()
}

#[cfg(test)]
mod tests;
