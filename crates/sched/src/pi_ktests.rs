//! SC-7b ktests of priority passing on kernel mutexes (`sc.locks.pi_*`): a waiter lends its class to the holder
//! while it holds, along a chain of holders, and takes it back when it gives up; a release wakes the most urgent
//! sleeper first. Functional only (QEMU judges no timing); all `Safe` and rerunnable (static mutexes are left
//! unlocked, every thread is joined).

use crate::{Class, JoinHandle, SpawnOpts, sleep_ms, spawn, spawn_with};
use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::{Completion, Mutex, lockdep};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sc.locks.pi_boost", Safe, pi_boost);
ktest!("sc.locks.pi_chain", Safe, pi_chain);
ktest!("sc.locks.pi_wake_order", Safe, pi_wake_order);
ktest!("sc.locks.pi_under_load", Safe, pi_under_load);

const SEC: u64 = 1_000_000_000;

fn join(h: &JoinHandle, what: &str) -> R {
    kassert!(h.join_timeout(30 * SEC), "{what}: thread {} did not finish within 30 s", h.id());
    Ok(())
}

fn class_of(id: u64) -> Option<Class> {
    crate::table::get(id).map(|t| t.class())
}

/// Polls `f` for up to 10 s (functional bound, not a timing check).
fn until(what: &str, mut f: impl FnMut() -> bool) -> R {
    let end = time::now_ns() + 10 * SEC;
    while !f() {
        kassert!(time::now_ns() < end, "{what}: not within 10 s");
        sleep_ms(1);
    }
    Ok(())
}

fn unexpected() -> u64 {
    lockdep::counts().unexpected
}

/// A Background holder runs as Interactive while an Interactive thread sleeps for its mutex, and falls back when
/// the waiter gets the lock, and again when a waiter gives up (`lock_timeout`).
fn pi_boost() -> R {
    static M: Mutex<u64> = Mutex::named("sc.kt.pi", 0);
    static HELD: AtomicBool = AtomicBool::new(false);
    static GO: Completion = Completion::new();
    static AFTER: AtomicU64 = AtomicU64::new(0);
    let v0 = unexpected();
    FAILED.store(false, Ordering::Release);
    for round in 0..2 {
        GO.reset();
        HELD.store(false, Ordering::Release);
        AFTER.store(u64::MAX, Ordering::Release);
        let h = spawn("sc-kt-pi-holder", Class::Background, || {
            let mut g = M.lock();
            HELD.store(true, Ordering::Release);
            GO.wait_timeout(20 * SEC);
            *g += 1;
            drop(g);
            AFTER.store(crate::current_thread().map_or(9, |t| t.class() as u64), Ordering::Release);
        })
        .map_err(|e| format!("{e:?}"))?;
        until("holder has the lock", || HELD.load(Ordering::Acquire))?;
        kassert!(class_of(h.id()) == Some(Class::Background), "holder before: {:?}", class_of(h.id()));
        let timed = round == 1;
        let w = spawn("sc-kt-pi-waiter", Class::Interactive, move || {
            if timed {
                kassert_ok(M.lock_timeout(2 * SEC).is_none());
            } else {
                *M.lock() += 1;
            }
        })
        .map_err(|e| format!("{e:?}"))?;
        until("holder lent Interactive", || class_of(h.id()) == Some(Class::Interactive))?;
        if timed {
            join(&w, "timed waiter")?;
            until("holder back to Background after the waiter gave up", || class_of(h.id()) == Some(Class::Background))?;
            GO.complete();
            join(&h, "holder")?;
        } else {
            GO.complete();
            join(&h, "holder")?;
            join(&w, "waiter")?;
        }
        let after = AFTER.load(Ordering::Acquire);
        kassert!(after == Class::Background as u64, "round {round}: holder's class right after its release: {after}");
    }
    kassert!(!FAILED.load(Ordering::Acquire), "the timed waiter got the lock while it was held");
    kassert!(*M.lock() % 3 == 0, "count {}", *M.lock());
    kassert!(M.waiters() == 0, "left: {} sleeping", M.waiters());
    until("no records left", || crate::pi::waiters() == 0)?;
    kassert!(unexpected() == v0, "lock violations during the test");
    Ok(())
}

static FAILED: AtomicBool = AtomicBool::new(false);
fn kassert_ok(c: bool) {
    if !c {
        FAILED.store(true, Ordering::Release);
    }
}

/// A (Background) holds M1; B (Background) holds M2 and sleeps for M1; C (Interactive) sleeps for M2: both A
/// and B run as Interactive. C gives up: both fall back. Then A releases: B gets M1 and finishes.
fn pi_chain() -> R {
    static M1: Mutex<()> = Mutex::named("sc.kt.pi.a", ());
    static M2: Mutex<()> = Mutex::named("sc.kt.pi.b", ());
    static A_HELD: AtomicBool = AtomicBool::new(false);
    static B_HELD: AtomicBool = AtomicBool::new(false);
    static GO: Completion = Completion::new();
    let v0 = unexpected();
    FAILED.store(false, Ordering::Release);
    GO.reset();
    A_HELD.store(false, Ordering::Release);
    B_HELD.store(false, Ordering::Release);
    let a = spawn("sc-kt-pi-a", Class::Background, || {
        let _g = M1.lock();
        A_HELD.store(true, Ordering::Release);
        GO.wait_timeout(20 * SEC);
    })
    .map_err(|e| format!("{e:?}"))?;
    until("A holds M1", || A_HELD.load(Ordering::Acquire))?;
    let b = spawn("sc-kt-pi-b", Class::Background, || {
        let _g2 = M2.lock();
        B_HELD.store(true, Ordering::Release);
        let _g1 = M1.lock();
    })
    .map_err(|e| format!("{e:?}"))?;
    until("B holds M2 and sleeps for M1", || B_HELD.load(Ordering::Acquire) && M1.waiters() == 1)?;
    kassert!(class_of(a.id()) == Some(Class::Background), "A lent something by a Background waiter: {:?}", class_of(a.id()));
    let c = spawn("sc-kt-pi-c", Class::Interactive, || kassert_ok(M2.lock_timeout(2 * SEC).is_none())).map_err(|e| format!("{e:?}"))?;
    until("A and B lent Interactive along the chain", || class_of(a.id()) == Some(Class::Interactive) && class_of(b.id()) == Some(Class::Interactive))?;
    join(&c, "C")?;
    kassert!(!FAILED.load(Ordering::Acquire), "C got M2 although B held it");
    until("A and B back to Background", || class_of(a.id()) == Some(Class::Background) && class_of(b.id()) == Some(Class::Background))?;
    GO.complete();
    join(&a, "A")?;
    join(&b, "B")?;
    kassert!(!M1.is_locked() && !M2.is_locked(), "locks left held");
    until("no records left", || crate::pi::waiters() == 0)?;
    kassert!(unexpected() == v0, "lock violations during the test");
    Ok(())
}

/// Two Normal waiters arrive before an Interactive one; the release wakes the Interactive one first, then the
/// Normal ones in arrival order.
fn pi_wake_order() -> R {
    static M: Mutex<()> = Mutex::named("sc.kt.pi.order", ());
    static HELD: AtomicBool = AtomicBool::new(false);
    static GO: Completion = Completion::new();
    static ORDER: AtomicU64 = AtomicU64::new(0);
    static N: AtomicUsize = AtomicUsize::new(0);
    let v0 = unexpected();
    GO.reset();
    HELD.store(false, Ordering::Release);
    ORDER.store(0, Ordering::Release);
    N.store(0, Ordering::Release);
    let h = spawn("sc-kt-pi-order-h", Class::Normal, || {
        let _g = M.lock();
        HELD.store(true, Ordering::Release);
        GO.wait_timeout(20 * SEC);
    })
    .map_err(|e| format!("{e:?}"))?;
    until("holder", || HELD.load(Ordering::Acquire))?;
    let mut ws = Vec::new();
    for (i, class) in [(1u64, Class::Normal), (2, Class::Normal), (3, Class::Interactive)] {
        ws.push(
            spawn("sc-kt-pi-order-w", class, move || {
                let _g = M.lock();
                let k = N.fetch_add(1, Ordering::AcqRel);
                ORDER.fetch_or(i << (8 * k), Ordering::AcqRel);
            })
            .map_err(|e| format!("{e:?}"))?,
        );
        let want = i as usize;
        until("waiter asleep", || M.waiters() == want)?;
    }
    GO.complete();
    join(&h, "holder")?;
    for w in &ws {
        join(w, "waiter")?;
    }
    let o = ORDER.load(Ordering::Acquire);
    kassert!(o == 3 | (1 << 8) | (2 << 16), "order of taking the lock {o:#x} (want Interactive 3, then 1, 2)");
    kassert!(unexpected() == v0, "lock violations during the test");
    Ok(())
}

fn spin(n: u32) {
    let mut x = 0u64;
    for i in 0..n {
        x = core::hint::black_box(x.wrapping_mul(31).wrapping_add(i as u64));
    }
    core::hint::black_box(x);
}

/// The number for the Mini-PC list: two busy Normal threads on every CPU; a Background holder (pinned to the
/// last CPU) needs 20 ms of CPU time under its mutex while another thread waits for it. An Interactive waiter
/// lends Interactive: the holder gets about 3121 : 2048 of its CPU instead of 110 : 2048, so the wait is tens of
/// milliseconds; a Background waiter (nothing to lend) shows the wait without priority passing. Event
/// `ktest.pi_under_load` (`lent_us`, `unlent_us`); asserted only that the holder ran lent and every wait ended
/// (QEMU judges no timing).
fn pi_under_load() -> R {
    let n = crate::wait_cpus_online(5 * SEC);
    let stop = alloc::sync::Arc::new(AtomicBool::new(false));
    let mut hogs = Vec::new();
    let run = (|| -> Result<[(u64, bool); 2], alloc::string::String> {
        for cpu in 0..n {
            for _ in 0..2 {
                let s = stop.clone();
                let o = SpawnOpts { cpu: Some(cpu), pin: true, ..SpawnOpts::default() };
                hogs.push(
                    spawn_with("sc-kt-pi-hog", Class::Normal, o, move || {
                        while !s.load(Ordering::Acquire) {
                            spin(2_000);
                            crate::cond_resched();
                        }
                    })
                    .map_err(|e| format!("{e:?}"))?,
                );
            }
        }
        Ok([load_round(n, Class::Interactive)?, load_round(n, Class::Background)?])
    })();
    stop.store(true, Ordering::Release);
    for h in &hogs {
        join(h, "hog")?;
    }
    let [(lent_ns, lent), (unlent_ns, _)] = run?;
    klog::event!(Info, "sc", "ktest.pi_under_load", cpus = n, hogs = 2 * n, holder_cpu_ms = 20, lent_us = lent_ns / 1000, unlent_us = unlent_ns / 1000);
    kassert!(lent, "the holder did not run as Interactive while the Interactive thread waited");
    Ok(())
}

/// One holder/waiter round under the hogs: how long the waiter waited, and whether the holder ran Interactive.
fn load_round(n: usize, waiter: Class) -> Result<(u64, bool), alloc::string::String> {
    static M: Mutex<()> = Mutex::named("sc.kt.pi.load", ());
    static HELD: AtomicBool = AtomicBool::new(false);
    static LENT: AtomicBool = AtomicBool::new(false);
    static WAIT: AtomicU64 = AtomicU64::new(0);
    HELD.store(false, Ordering::Release);
    LENT.store(false, Ordering::Release);
    WAIT.store(u64::MAX, Ordering::Release);
    let o = SpawnOpts { cpu: Some(n.saturating_sub(1)), pin: true, ..SpawnOpts::default() };
    let h = spawn_with("sc-kt-pi-load-h", Class::Background, o, || {
        let g = M.lock();
        HELD.store(true, Ordering::Release);
        let me = crate::current_thread();
        let start = me.as_ref().map_or(0, |t| t.runtime_ns());
        let end = time::now_ns() + 20 * SEC;
        while me.as_ref().map_or(u64::MAX, |t| t.runtime_ns()) - start < 20_000_000 && time::now_ns() < end {
            spin(2_000);
            crate::cond_resched();
        }
        LENT.store(me.as_ref().is_some_and(|t| t.class() == Class::Interactive), Ordering::Release);
        drop(g);
    })
    .map_err(|e| format!("{e:?}"))?;
    until("holder has the lock", || HELD.load(Ordering::Acquire))?;
    let w = spawn("sc-kt-pi-load-w", waiter, || {
        let t0 = time::now_ns();
        drop(M.lock());
        WAIT.store(time::now_ns() - t0, Ordering::Release);
    })
    .map_err(|e| format!("{e:?}"))?;
    join(&w, "waiter")?;
    join(&h, "holder")?;
    Ok((WAIT.load(Ordering::Acquire), LENT.load(Ordering::Acquire)))
}
