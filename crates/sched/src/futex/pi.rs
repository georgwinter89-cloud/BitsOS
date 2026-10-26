//! PI futexes (SC-7b; linux-6.6/kernel/futex/pi.c: `futex_lock_pi` :930, `futex_lock_pi_atomic` :514,
//! `attach_to_pi_state` :202, `attach_to_pi_owner` :410, `handle_exit_race` :320, `futex_unlock_pi` :1100,
//! `wake_futex_pi` :613, `fixup_pi_state_owner` :683; core.c `exit_pi_state_list`).
//!
//! The word holds the owner's TID (`FUTEX_TID_MASK`), `FUTEX_WAITERS` while the kernel knows of waiters and
//! `FUTEX_OWNER_DIED` after a robust owner died. Programs take and release an uncontended lock in user space;
//! the kernel is entered when the word is not 0 (lock) or has FUTEX_WAITERS (unlock).
//!
//! - **Waiters** are bucket entries with `table::PI_BITSET` (a plain wake meeting one answers EINVAL) and, at the
//!   same time and under the same bucket lock, waiters in `sched::pi`'s book, which records the owner (Linux's
//!   `pi_state`, alive exactly while waiters exist), lends the waiters' class to it along chains, and picks the
//!   next owner (class first, then arrival). A lock without waiters has no record.
//! - **Lock** follows `futex_lock_pi_atomic`: own TID in the word: EDEADLK; waiters known: check the word
//!   against the record (`attach_to_pi_state`, EINVAL for an inconsistent word); word 0 (OWNER_DIED kept): taken;
//!   else FUTEX_WAITERS is set and the owner looked up by TID (`Space::thread_of`): none, or one that has ended:
//!   ESRCH (unless the word changed meanwhile: again); one that is ending: sleep until it has ended (`wait_exit`),
//!   again. Then
//!   queue and sleep; a deadlocking chain answers EDEADLK. TRYLOCK_PI does the same up to the queueing and
//!   answers EAGAIN (EWOULDBLOCK) instead, FUTEX_WAITERS left set as Linux does.
//! - **Unlock** (only by the TID in the word, EPERM): with waiters the lock goes to the top waiter directly
//!   (word = FUTEX_WAITERS | its TID, `wake_futex_pi`); without, the word becomes 0 (EAGAIN if it changed).
//! - **Owner exit** (`owner_exits`, from `sched::exit` through `sched::pi::thread_exits`): no waiter attaches to
//!   the thread any more (`pi_exit`, Linux's `futex_state`, checked under the book's lock), then every PI futex
//!   it owns with waiters goes to its top waiter, which writes its TID into the word in its own address space,
//!   with FUTEX_OWNER_DIED (Linux sets it for every owner that died, robust or not).
//! - **Interruption**: a signal ends a LOCK_PI wait with the Space's errno (the syscall layer gives
//!   ERESTARTNOINTR, as Linux turns -EINTR into); a deadline with ETIMEDOUT. A waiter that was handed the lock at
//!   the same moment owns it and answers 0 (`rt_mutex_cleanup_proxy_lock`).
//!
//! Retries after a racing change of the word are bounded (`RACE_LIMIT`, then EAGAIN), retries after faults by
//! `FAULT_RETRY_LIMIT` (B4).

use super::table::{self, Key, PI_BITSET};
use super::{Deadline, FEntry, FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, Space, Table};
use super::{begin, buckets, cancel_block, enqueue, expired, me, not_loaded, retry_fault, sleep, wake_entry};
use crate::pi::{self, Live, LockId, Owner, WaitError};
use crate::{State, Thread};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use ksync::user_word::{WordError, WordOp};
use uapi::errno::{EAGAIN, EDEADLK, EFAULT, EINVAL, ENOMEM, EPERM, ESRCH, ETIMEDOUT};

/// `ThreadState::pi_exit`.
pub(crate) const RUNNING: u8 = 0;
pub(crate) const EXITING: u8 = 1;
pub(crate) const DEAD: u8 = 2;

/// Retries of one operation after the word changed under it (user space racing it) or its owner was still
/// ending: past this many the operation answers EAGAIN instead of looping (an input bound, B4).
pub const RACE_LIMIT: u32 = 1024;

klog::counter!(LOCKS, "futex.pi.locks");
klog::counter!(WAITS, "futex.pi.waits");
klog::counter!(HANDOVERS, "futex.pi.handovers");
klog::counter!(OWNER_DIED, "futex.pi.owner_died");
klog::counter!(TIMEOUTS, "futex.pi.timeouts");
klog::counter!(DEADLOCKS, "futex.pi.deadlocks");

/// What the atomic part of a lock attempt found (`futex_lock_pi_atomic`).
enum Step {
    /// The word was free: the caller owns the lock now.
    Taken,
    /// The caller must wait for `owner` (`fresh`: no waiters were known; the owner's exit state is checked
    /// when it is recorded; `word`: the word as we left it).
    Attach { owner: Option<Owner>, fresh: bool, word: u32 },
    /// The word changed under us (EAGAIN in Linux): again.
    Again,
    /// The word could not be accessed without a fault: fault it in, again.
    Fault,
    Fail(i64),
}

/// The owner thread's exit state as an attach sees it (under the book's lock).
fn owner_state(id: u64) -> u8 {
    crate::table::get(id).map_or(DEAD, |t| t.futex.pi_exit.load(Ordering::Acquire))
}

fn cmpxchg(sp: &dyn Space, addr: u64, old: u32, new: u32) -> Result<u32, Step> {
    match sp.word(addr, WordOp::CmpXchg { old, new }) {
        Ok(cur) => Ok(cur),
        Err(WordError::NotLoaded) => {
            not_loaded();
            Err(Step::Fail(EFAULT))
        }
        Err(_) => Err(Step::Fault),
    }
}

/// `handle_exit_race` for an owner that is gone: the word changed since we read it: again; else ESRCH.
fn exit_race(sp: &dyn Space, addr: u64, uval: u32) -> Step {
    match sp.word(addr, WordOp::Load) {
        Ok(v) if v != uval => Step::Again,
        Ok(_) => Step::Fail(ESRCH),
        Err(WordError::NotLoaded) => {
            not_loaded();
            Step::Fail(EFAULT)
        }
        Err(_) => Step::Fault,
    }
}

/// `futex_lock_pi_atomic` under the bucket lock `q` for the caller with TID `tid`.
fn atomic(sp: &dyn Space, q: &[FEntry], addr: u64, key: &Key, tid: u32) -> Step {
    let uval = match sp.word(addr, WordOp::Load) {
        Ok(v) => v,
        Err(WordError::NotLoaded) => {
            not_loaded();
            return Step::Fail(EFAULT);
        }
        Err(_) => return Step::Fault,
    };
    let pid = uval & FUTEX_TID_MASK;
    if pid == tid {
        return Step::Fail(EDEADLK);
    }
    if let Some(top) = q.iter().find(|e| e.key == *key) {
        // `attach_to_pi_state`: a plain waiter first, or a word that does not match the record: EINVAL.
        if !top.is_pi() {
            return Step::Fail(EINVAL);
        }
        let Some(owner) = pi::book().owner(LockId::Futex(*key)) else { return Step::Fail(EINVAL) };
        let ok = match (uval & FUTEX_OWNER_DIED != 0, owner) {
            // The owner died and its waiter took over, but has not written its TID yet ([4], [5]).
            (true, None) => pid == 0,
            (true, Some(_)) if pid == 0 => true,
            (false, None) => false,
            (_, Some(o)) => o.tid == pid,
        };
        return if ok { Step::Attach { owner, fresh: false, word: uval } } else { Step::Fail(EINVAL) };
    }
    if pid == 0 {
        return match cmpxchg(sp, addr, uval, (uval & FUTEX_OWNER_DIED) | tid) {
            Ok(cur) if cur == uval => Step::Taken,
            Ok(_) => Step::Again,
            Err(s) => s,
        };
    }
    let newval = uval | FUTEX_WAITERS;
    match cmpxchg(sp, addr, uval, newval) {
        Ok(cur) if cur == uval => {}
        Ok(_) => return Step::Again,
        Err(s) => return s,
    }
    // `attach_to_pi_owner`.
    match sp.thread_of(pid) {
        Some(id) => Step::Attach { owner: Some(Owner { id, tid: pid }), fresh: true, word: newval },
        None => exit_race(sp, addr, newval),
    }
}

/// FUTEX_LOCK_PI / FUTEX_LOCK_PI2 (`trylock` false) and FUTEX_TRYLOCK_PI on the word at `addr`. Ok: the caller
/// owns the lock (its TID is in the word).
pub fn lock_pi(sp: &dyn Space, addr: u64, shared: bool, deadline: Option<Deadline>, trylock: bool) -> Result<(), i64> {
    let tid = sp.tid().ok_or(EINVAL)? & FUTEX_TID_MASK;
    let me = me()?;
    let t = buckets()?;
    me.futex.reserve(1)?;
    LOCKS.inc();
    let (mut faults, mut races) = (0u32, 0u32);
    loop {
        let kr = sp.key(addr, shared, true)?;
        let key = kr.key;
        let lock = LockId::Futex(key);
        let bi = t.index(&key);
        let mut q = t.lock(bi);
        if q.try_reserve(1).is_err() {
            return Err(ENOMEM);
        }
        let (owner, fresh, word) = match atomic(sp, &q[..], addr, &key, tid) {
            Step::Taken => return Ok(()),
            Step::Attach { owner, fresh, word } => (owner, fresh, word),
            Step::Fail(e) => return Err(e),
            Step::Again => {
                drop(q);
                again(&mut races)?;
                continue;
            }
            Step::Fault => {
                drop(q);
                sp.fault_in(addr, true)?;
                retry_fault(&mut faults)?;
                continue;
            }
        };
        // Recording ourselves (or, for a trylock, only looking) and the owner's exit state are one step under
        // the book's lock, so an owner that starts to exit either finds us in its records or we see it exiting.
        let mut live = Live::new();
        let state = {
            let mut b = pi::book();
            let state = match owner {
                Some(o) if fresh => owner_state(o.id),
                _ => RUNNING,
            };
            if state == RUNNING && !trylock {
                begin(&me);
                enqueue(&mut q, bi, &me, key, PI_BITSET, 0);
                if let Err(e) = b.wait(&mut live, lock, Owner { id: me.id, tid }, pi::own_of(&me), owner, true) {
                    let _ = table::remove(&mut q, 0, &|w: &Arc<Thread>| Arc::ptr_eq(w, &me));
                    me.futex.set_loc(0, u32::MAX);
                    cancel_block(&me);
                    drop(b);
                    drop(q);
                    if e == WaitError::NoMemory {
                        return Err(ENOMEM);
                    }
                    DEADLOCKS.inc();
                    klog::event!(Info, "futex", "pi.deadlock", thread = me.id, tid = tid; "FUTEX_LOCK_PI would close a cycle of owners (EDEADLK)");
                    return Err(EDEADLK);
                }
            }
            state
        };
        match state {
            RUNNING if trylock => return Err(EAGAIN),
            RUNNING => {}
            EXITING => {
                // `wait_for_owner_exiting`: the owner is between its end and its hand-overs.
                drop(q);
                wait_exit(owner.and_then(|o| crate::table::get(o.id)), sp, deadline)?;
                continue;
            }
            _ => match exit_race(sp, addr, word) {
                Step::Fail(e) => return Err(e),
                Step::Fault => {
                    drop(q);
                    sp.fault_in(addr, true)?;
                    retry_fault(&mut faults)?;
                    continue;
                }
                _ => {
                    drop(q);
                    again(&mut races)?;
                    continue;
                }
            },
        }
        drop(q);
        live.apply();
        WAITS.inc();
        let r = wait_granted(t, &me, sp, deadline);
        if r.is_ok() && me.futex.pi_fixup.swap(false, Ordering::AcqRel) {
            return fixup(sp, t, addr, &key, tid);
        }
        return r;
    }
}

/// Sleeps until `owner` has ended its PI hand-overs (Linux `wait_for_owner_exiting` sleeps on the owner's
/// `futex_exit_mutex`; it does not count as a race). Bounded by the owner's exit work; a deadline or a signal of
/// the waiter ends it early.
fn wait_exit(owner: Option<Arc<Thread>>, sp: &dyn Space, deadline: Option<Deadline>) -> Result<(), i64> {
    let Some(t) = owner else { return Ok(()) };
    let gone = || t.futex.pi_exit.load(Ordering::Acquire) != EXITING;
    let mut polls = 0u64;
    loop {
        if EXITS.wait_until_timeout(gone, EXIT_POLL_NS) {
            return Ok(());
        }
        polls += 1;
        if polls == 1 {
            klog::event!(Debug, "futex", "pi.owner_exiting", thread = crate::current_id(), owner = t.id; "FUTEX_LOCK_PI waits for an ending owner's hand-overs");
        }
        if expired(deadline) {
            TIMEOUTS.inc();
            return Err(ETIMEDOUT);
        }
        if let Some(e) = sp.interrupted() {
            return Err(e);
        }
    }
}

/// Waiters for an ending owner (`wait_exit`), woken when its `pi_exit` becomes DEAD.
static EXITS: ksync::WaitQueue = ksync::WaitQueue::new();
/// How often such a waiter looks at its deadline and signals.
const EXIT_POLL_NS: u64 = 1_000_000;

fn again(races: &mut u32) -> Result<(), i64> {
    *races += 1;
    if *races > RACE_LIMIT {
        klog::event!(Info, "futex", "pi.race_limit", thread = crate::current_id(); "a PI futex word kept changing under FUTEX_LOCK_PI/UNLOCK_PI: EAGAIN");
        return Err(EAGAIN);
    }
    crate::cond_resched();
    Ok(())
}

/// Sleeps until the lock is handed to us (Ok), the deadline passes or the wait is interrupted.
fn wait_granted(t: &Table, me: &Arc<Thread>, sp: &dyn Space, deadline: Option<Deadline>) -> Result<(), i64> {
    loop {
        sleep(me, sp, deadline);
        if me.futex.woken.load(Ordering::Acquire) {
            return Ok(());
        }
        let why = if expired(deadline) { Some(ETIMEDOUT) } else { sp.interrupted() };
        if let Some(e) = why {
            if !unqueue(t, me) {
                // Handed over at the same moment: ours.
                return Ok(());
            }
            if e == ETIMEDOUT {
                TIMEOUTS.inc();
            }
            return Err(e);
        }
        // Spurious: sleep on (`sleep` re-checks `woken` after a full fence).
        me.set_state(State::Blocking);
    }
}

/// Takes `me`'s PI entry off its bucket and out of the book (one step under the bucket lock); false when an
/// unlock or an owner's exit took it (the lock is ours).
fn unqueue(t: &Table, me: &Arc<Thread>) -> bool {
    loop {
        let bi = me.futex.loc(0);
        if bi == u32::MAX {
            return false;
        }
        let mut q = t.lock(bi);
        if me.futex.loc(0) != bi {
            continue;
        }
        if table::remove(&mut q, 0, &|w: &Arc<Thread>| Arc::ptr_eq(w, me)).is_none() {
            return false;
        }
        let mut live = Live::new();
        pi::book().leave(&mut live, me.id);
        drop(q);
        live.apply();
        return true;
    }
}

/// `__fixup_pi_state_owner` after an owner's exit handed us the lock (the record had no owner): our TID into the
/// word with FUTEX_WAITERS and FUTEX_OWNER_DIED (pi.c:760: set whenever the previous owner is gone, robust or not).
fn fixup(sp: &dyn Space, t: &Table, addr: u64, key: &Key, tid: u32) -> Result<(), i64> {
    let mut faults = 0;
    let mut races = 0;
    loop {
        let q = t.lock(t.index(key));
        let uval = match sp.word(addr, WordOp::Load) {
            Ok(v) => v,
            Err(WordError::NotLoaded) => {
                not_loaded();
                return Err(EFAULT);
            }
            Err(_) => {
                drop(q);
                sp.fault_in(addr, true)?;
                retry_fault(&mut faults)?;
                continue;
            }
        };
        let newval = FUTEX_OWNER_DIED | FUTEX_WAITERS | tid;
        match cmpxchg(sp, addr, uval, newval) {
            Ok(cur) if cur == uval => return Ok(()),
            Ok(_) => {
                drop(q);
                again(&mut races)?;
            }
            Err(Step::Fail(e)) => return Err(e),
            Err(_) => {
                drop(q);
                sp.fault_in(addr, true)?;
                retry_fault(&mut faults)?;
            }
        }
    }
}

/// FUTEX_UNLOCK_PI: EPERM unless the word's TID is the caller's; with waiters the top one gets the lock.
pub fn unlock_pi(sp: &dyn Space, addr: u64, shared: bool) -> Result<(), i64> {
    let tid = sp.tid().ok_or(EINVAL)? & FUTEX_TID_MASK;
    let me = me()?;
    let t = buckets()?;
    let (mut faults, mut races) = (0u32, 0u32);
    loop {
        let uval = sp.read_u32(addr)?;
        if uval & FUTEX_TID_MASK != tid {
            return Err(EPERM);
        }
        let kr = sp.key(addr, shared, true)?;
        let key = kr.key;
        let lock = LockId::Futex(key);
        let mut q = t.lock(t.index(&key));
        let r = if let Some(top) = q.iter().find(|e| e.key == key) {
            if !top.is_pi() {
                return Err(EINVAL);
            }
            let mut live = Live::new();
            let mut b = pi::book();
            if b.owner(lock).flatten().map(|o| o.id) != Some(me.id) {
                return Err(EINVAL);
            }
            let Some(new) = b.top(lock) else { return Err(EINVAL) };
            match cmpxchg(sp, addr, uval, FUTEX_WAITERS | new.tid) {
                Ok(cur) if cur == uval => {
                    hand(&mut q, &mut b, &mut live, lock, new, false);
                    drop(b);
                    drop(q);
                    live.apply();
                    HANDOVERS.inc();
                    return Ok(());
                }
                // Linux's odd test in `wake_futex_pi`, kept as it is.
                Ok(cur) if cur & FUTEX_TID_MASK == uval => Step::Again,
                Ok(_) => return Err(EINVAL),
                Err(s) => s,
            }
        } else {
            match cmpxchg(sp, addr, uval, 0) {
                Ok(cur) => return if cur == uval { Ok(()) } else { Err(EAGAIN) },
                Err(s) => s,
            }
        };
        drop(q);
        match r {
            Step::Fail(e) => return Err(e),
            Step::Fault => {
                sp.fault_in(addr, true)?;
                retry_fault(&mut faults)?;
            }
            _ => again(&mut races)?,
        }
    }
}

/// Gives `lock` to waiter `new` (bucket lock `q` and the book held): out of the book's waiters, recorded as the
/// owner, its entry off the bucket, woken (`fixup`: it writes the word itself).
fn hand(q: &mut Vec<FEntry>, b: &mut pi::Locked, live: &mut Live, lock: LockId, new: Owner, fixup: bool) {
    b.leave(live, new.id);
    b.set_owner(live, lock, Some(new));
    let LockId::Futex(key) = lock else { return };
    if let Some(i) = q.iter().position(|e| e.key == key && e.is_pi() && e.who.id == new.id) {
        let e = q.remove(i);
        e.who.futex.pi_fixup.store(fixup, Ordering::Release);
        wake_entry(e);
    }
}

/// Thread `t` ends (Linux `exit_pi_state_list`): no waiter attaches to it from now on, and every PI futex it
/// owns goes to its top waiter.
pub(crate) fn owner_exits(t: &Arc<Thread>) {
    let mut keys: Vec<Key> = Vec::new();
    {
        let b = pi::book();
        t.futex.pi_exit.store(EXITING, Ordering::Release);
        for l in b.owned_by(t.id) {
            if let LockId::Futex(k) = l {
                if keys.try_reserve(1).is_err() {
                    klog::event!(Error, "futex", "pi.exit_no_memory", thread = t.id; "no memory to list the PI futexes of an ending owner: their waiters keep waiting");
                    break;
                }
                keys.push(k);
            }
        }
    }
    if keys.is_empty() {
        t.futex.pi_exit.store(DEAD, Ordering::Release);
        EXITS.wake_all();
        return;
    }
    if let Ok(tb) = buckets() {
        for key in keys {
            let mut q = tb.lock(tb.index(&key));
            let mut live = Live::new();
            let mut b = pi::book();
            let lock = LockId::Futex(key);
            if b.owner(lock).flatten().map(|o| o.id) != Some(t.id) {
                continue;
            }
            let Some(new) = b.top(lock) else { continue };
            hand(&mut q, &mut b, &mut live, lock, new, true);
            drop(b);
            drop(q);
            live.apply();
            OWNER_DIED.inc();
            klog::event!(Debug, "futex", "pi.owner_exit", thread = t.id, to = new.id, tid = new.tid);
        }
    }
    t.futex.pi_exit.store(DEAD, Ordering::Release);
    EXITS.wake_all();
}
