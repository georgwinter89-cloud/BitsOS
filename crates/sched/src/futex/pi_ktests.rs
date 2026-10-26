//! SC-7b ktests of PI futexes on kernel words (`KSpace` of `ktests`: kernel threads, the scheduler id as TID):
//! word rules on one thread, the hand-over with priority passing to the owner, plain operations meeting a PI
//! waiter, the owner's exit, timeouts and interruption leaving the owner unboosted, deadlock detection, and a
//! storm of 16 threads of mixed classes on one PI lock (exact counter, nothing left queued). All `Safe`,
//! rerunnable.

use super::ktests::{KSpace, MS, SEC};
use super::pi::{lock_pi, unlock_pi};
use super::{Deadline, FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS};
use crate::{Class, JoinHandle};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

ktest!("sc.futex.core.pi_basics", Safe, basics);
ktest!("sc.futex.core.pi_handover", Safe, handover);
ktest!("sc.futex.core.pi_owner_exit", Safe, owner_exit);
ktest!("sc.futex.core.pi_give_up", Safe, give_up);
ktest!("sc.futex.core.pi_deadlock", Safe, deadlock);
ktest!("sc.futex.core.pi_storm", Safe, storm);

type Running = (JoinHandle, Arc<AtomicU64>);

fn err(r: Result<(), i64>) -> i64 {
    r.err().unwrap_or(0)
}

fn spawn_c(name: &str, class: Class, f: impl FnOnce() -> i64 + Send + 'static) -> Result<Running, String> {
    let r = Arc::new(AtomicU64::new(u64::MAX));
    let r2 = r.clone();
    let h = crate::spawn(name, class, move || r2.store(f() as u64, Ordering::Release)).map_err(|e| format!("spawn: {e:?}"))?;
    Ok((h, r))
}

fn done(h: &Running) -> Result<i64, String> {
    kassert!(h.0.join_timeout(30 * SEC), "thread {} did not finish within 30 s", h.0.id());
    Ok(h.1.load(Ordering::Acquire) as i64)
}

fn class_of(id: u64) -> Option<Class> {
    crate::table::get(id).map(|t| t.class())
}

fn until(what: &str, mut f: impl FnMut() -> bool) -> TestResult {
    let end = time::now_ns() + 10 * SEC;
    while !f() {
        kassert!(time::now_ns() < end, "{what}: not within 10 s");
        crate::sleep_ns(MS);
    }
    Ok(())
}

/// Records of other contended locks in the kernel pass quickly; ours must be gone.
fn no_records() -> TestResult {
    until("no priority-passing records left", || crate::pi::waiters() == 0)
}

fn tid() -> u32 {
    crate::current_id() as u32 & FUTEX_TID_MASK
}

fn word(s: &KSpace, i: usize) -> u32 {
    s.w(i).load(Ordering::SeqCst)
}

fn basics() -> TestResult {
    let s = KSpace::new(2);
    let a = KSpace::a(0);
    let me = tid();
    kassert!(lock_pi(&*s, a, false, None, true).is_ok(), "trylock of a free word");
    kassert!(word(&s, 0) == me, "word {:#x}", word(&s, 0));
    kassert!(err(lock_pi(&*s, a, false, None, false)) == EDEADLK, "lock of a word we own");
    kassert!(err(lock_pi(&*s, a, false, None, true)) == EDEADLK, "trylock of a word we own");
    kassert!(unlock_pi(&*s, a, false).is_ok());
    kassert!(word(&s, 0) == 0);
    kassert!(err(unlock_pi(&*s, a, false)) == EPERM, "unlock of a free word");
    s.w(0).store(FUTEX_WAITERS | FUTEX_OWNER_DIED, Ordering::SeqCst);
    kassert!(lock_pi(&*s, a, false, None, true).is_ok(), "a dead owner's word, nobody waiting");
    kassert!(word(&s, 0) == FUTEX_OWNER_DIED | me, "OWNER_DIED kept, WAITERS dropped: {:#x}", word(&s, 0));
    kassert!(unlock_pi(&*s, a, false).is_ok() && word(&s, 0) == 0);
    // No such thread: ESRCH, with FUTEX_WAITERS set on the way (as Linux).
    s.w(1).store(0x3fff_fff0, Ordering::SeqCst);
    kassert!(err(lock_pi(&*s, KSpace::a(1), false, None, false)) == ESRCH);
    kassert!(word(&s, 1) == 0x3fff_fff0 | FUTEX_WAITERS);
    kassert!(err(lock_pi(&*s, a + 1, false, None, false)) == EINVAL, "misaligned");
    kassert!(err(lock_pi(&*s, KSpace::a(9), false, None, false)) == EFAULT, "outside the space");
    Ok(())
}

/// A Background owner, an Interactive waiter: the owner runs as Interactive while the waiter sleeps, plain
/// FUTEX_WAKE and FUTEX_REQUEUE meeting the waiter answer EINVAL, the unlock hands the lock over (word =
/// WAITERS | the waiter's TID) and the owner falls back to Background.
fn handover() -> TestResult {
    let s = KSpace::new(2);
    let a = KSpace::a(0);
    let go = Arc::new(AtomicBool::new(false));
    let (s1, g1) = (s.clone(), go.clone());
    let owner = spawn_c("sc-kt-pi-owner", Class::Background, move || {
        if let Err(e) = lock_pi(&*s1, a, false, None, false) {
            return e;
        }
        while !g1.load(Ordering::Acquire) {
            crate::sleep_ns(MS);
        }
        let r = err(unlock_pi(&*s1, a, false));
        if r != 0 { r } else { crate::current_thread().map_or(99, |t| t.class() as i64) }
    })?;
    until("owner holds the word", || word(&s, 0) == owner.0.id() as u32)?;
    let s2 = s.clone();
    let waiter = spawn_c("sc-kt-pi-waiter", Class::Interactive, move || {
        let r = err(lock_pi(&*s2, a, false, None, false));
        if r != 0 {
            return r;
        }
        let w = word(&s2, 0);
        let me = tid();
        let r = err(unlock_pi(&*s2, a, false));
        if w != FUTEX_WAITERS | me { -1 } else { r }
    })?;
    until("owner lent Interactive", || class_of(owner.0.id()) == Some(Class::Interactive))?;
    kassert!(word(&s, 0) & FUTEX_WAITERS != 0, "no FUTEX_WAITERS");
    kassert!(super::wake(&*s, a, false, 1, super::BITSET_MATCH_ANY) == Err(EINVAL), "FUTEX_WAKE on a PI waiter");
    kassert!(super::requeue(&*s, a, KSpace::a(1), false, 0, 1, None) == Err(EINVAL), "FUTEX_REQUEUE of a PI waiter");
    go.store(true, Ordering::Release);
    let oc = done(&owner)?;
    kassert!(oc == Class::Background as i64, "owner after its unlock: {oc} (want Background)");
    kassert!(done(&waiter)? == 0, "waiter: word was not WAITERS | its TID after the hand-over, or its unlock failed");
    kassert!(word(&s, 0) == 0, "word {:#x}", word(&s, 0));
    no_records()?;
    Ok(())
}

/// The owner ends holding the lock: its waiter gets it, with OWNER_DIED | WAITERS | its TID in the word.
fn owner_exit() -> TestResult {
    let s = KSpace::new(1);
    let a = KSpace::a(0);
    let s1 = s.clone();
    let owner = spawn_c("sc-kt-pi-dier", Class::Normal, move || {
        if let Err(e) = lock_pi(&*s1, a, false, None, false) {
            return e;
        }
        let end = time::now_ns() + 10 * SEC;
        while word(&s1, 0) & FUTEX_WAITERS == 0 && time::now_ns() < end {
            crate::sleep_ns(MS);
        }
        crate::sleep_ns(10 * MS);
        0
    })?;
    until("owner holds the word", || word(&s, 0) == owner.0.id() as u32)?;
    let s2 = s.clone();
    let waiter = spawn_c("sc-kt-pi-heir", Class::Normal, move || {
        let r = err(lock_pi(&*s2, a, false, Some(Deadline::Monotonic(time::now_ns() + 20 * SEC)), false));
        if r != 0 {
            return r;
        }
        let w = word(&s2, 0);
        let _ = unlock_pi(&*s2, a, false);
        if w == FUTEX_OWNER_DIED | FUTEX_WAITERS | tid() { 0 } else { w as i64 }
    })?;
    kassert!(done(&owner)? == 0, "owner");
    let r = done(&waiter)?;
    kassert!(r == 0, "heir: {r:#x} (want 0: word OWNER_DIED | WAITERS | its TID)");
    kassert!(word(&s, 0) == 0, "word {:#x}", word(&s, 0));
    no_records()?;
    Ok(())
}

/// A waiter that times out, and one that is interrupted, take their class back from the owner; the word keeps
/// FUTEX_WAITERS (the owner's unlock then finds nobody and clears it).
fn give_up() -> TestResult {
    let s = KSpace::new(1);
    let a = KSpace::a(0);
    let go = Arc::new(AtomicBool::new(false));
    let (s1, g1) = (s.clone(), go.clone());
    let owner = spawn_c("sc-kt-pi-owner2", Class::Background, move || {
        if let Err(e) = lock_pi(&*s1, a, false, None, false) {
            return e;
        }
        while !g1.load(Ordering::Acquire) {
            crate::sleep_ns(MS);
        }
        err(unlock_pi(&*s1, a, false))
    })?;
    until("owner holds the word", || word(&s, 0) == owner.0.id() as u32)?;
    for interrupt in [false, true] {
        s.stop.store(false, Ordering::Release);
        let s2 = s.clone();
        let w = spawn_c("sc-kt-pi-quitter", Class::Interactive, move || {
            let d = (!interrupt).then(|| Deadline::Monotonic(time::now_ns() + 2 * SEC));
            err(lock_pi(&*s2, a, false, d, false))
        })?;
        until("owner lent Interactive", || class_of(owner.0.id()) == Some(Class::Interactive))?;
        if interrupt {
            s.stop.store(true, Ordering::Release);
            crate::kick_thread(w.0.id());
        }
        let r = done(&w)?;
        kassert!(r == if interrupt { EINTR } else { ETIMEDOUT }, "waiter (interrupt {interrupt}): {r}");
        until("owner back to Background", || class_of(owner.0.id()) == Some(Class::Background))?;
        kassert!(word(&s, 0) & FUTEX_WAITERS != 0, "FUTEX_WAITERS cleared by a waiter that left");
    }
    s.stop.store(false, Ordering::Release);
    go.store(true, Ordering::Release);
    kassert!(done(&owner)? == 0, "owner's unlock");
    kassert!(word(&s, 0) == 0, "word {:#x}", word(&s, 0));
    no_records()?;
    Ok(())
}

/// The other thread owns word 0 and waits for word 1, which we own: our wait for word 0 answers EDEADLK.
fn deadlock() -> TestResult {
    let s = KSpace::new(2);
    let (a0, a1) = (KSpace::a(0), KSpace::a(1));
    kassert!(lock_pi(&*s, a1, false, None, true).is_ok());
    let s1 = s.clone();
    let other = spawn_c("sc-kt-pi-cross", Class::Normal, move || {
        if let Err(e) = lock_pi(&*s1, a0, false, None, true) {
            return e;
        }
        let r = err(lock_pi(&*s1, a1, false, None, false));
        let _ = unlock_pi(&*s1, a1, false);
        let _ = unlock_pi(&*s1, a0, false);
        r
    })?;
    until("other waits for word 1", || word(&s, 1) & FUTEX_WAITERS != 0)?;
    kassert!(err(lock_pi(&*s, a0, false, None, false)) == EDEADLK, "the cycle was not seen");
    kassert!(unlock_pi(&*s, a1, false).is_ok());
    kassert!(done(&other)? == 0, "other");
    kassert!(word(&s, 0) == 0 && word(&s, 1) == 0, "words {:#x} {:#x}", word(&s, 0), word(&s, 1));
    no_records()?;
    Ok(())
}

/// 16 threads (Background, Normal, Interactive) take one PI lock 300 times each with the user-space fast path
/// (cmpxchg 0 -> TID, TID -> 0) and the kernel when contended; a plain counter changed only under the lock must
/// come out exact, and nothing stays queued or recorded.
fn storm() -> TestResult {
    const THREADS: usize = 16;
    const ROUNDS: u64 = 300;
    // Other threads (init's services on a real boot) wait on futexes too: only ours must be gone at the end.
    let base = super::waiters();
    let s = KSpace::new(1);
    let a = KSpace::a(0);
    let count = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for i in 0..THREADS {
        let class = [Class::Background, Class::Normal, Class::Interactive][i % 3];
        let (s1, c1) = (s.clone(), count.clone());
        hs.push(spawn_c("sc-kt-pi-storm", class, move || {
            let me = tid();
            for _ in 0..ROUNDS {
                if s1.w(0).compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed).is_err()
                    && let Err(e) = lock_pi(&*s1, a, false, None, false)
                {
                    return e;
                }
                // Not atomic as a whole on purpose: a broken lock loses updates.
                let v = c1.load(Ordering::Relaxed);
                crate::yield_now();
                c1.store(v + 1, Ordering::Relaxed);
                if s1.w(0).compare_exchange(me, 0, Ordering::Release, Ordering::Relaxed).is_err()
                    && let Err(e) = unlock_pi(&*s1, a, false)
                {
                    return 1000 + e;
                }
            }
            0
        })?);
    }
    for h in &hs {
        let r = done(h)?;
        kassert!(r == 0, "a thread failed: {r}");
    }
    kassert!(count.load(Ordering::SeqCst) == THREADS as u64 * ROUNDS, "count {}", count.load(Ordering::SeqCst));
    kassert!(word(&s, 0) == 0, "word {:#x}", word(&s, 0));
    kassert!(super::waiters() == base, "futex waiters left: {} (before: {base})", super::waiters());
    no_records()?;
    Ok(())
}
