//! `ask`: the caller waits (bounded, B4) while a user-space daemon asks Georg (permd, PM-3).
//!
//! A request is queued with a deadline, the listener (permd's wake-up) is told, and the caller sleeps on one
//! wait queue until `answer` sets its slot or the deadline passes; then the set's timeout decision applies
//! (the class default, deny when that is itself ask). Callers that cannot sleep (a spin lock held, interrupts
//! off) get the timeout decision at once. Until permd exists every ask ends at its deadline.
//!
//! Identical asks (same program, class and access) share one request: permd shows one prompt and its answer
//! releases every waiter. The request stays queued while any of its waiters still waits.

use crate::rules::{Decision, Learned};
use crate::{Access, AccessClass, Subject};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use ksync::{OnceRef, SpinLock, WaitQueue};

/// What permd shows Georg.
#[derive(Clone, Debug)]
pub struct AskInfo {
    pub id: u64,
    pub subject: Subject,
    pub class: AccessClass,
    /// The access in words ("Datei /home/georg/x lesen").
    pub what: String,
    /// On the wait clock (`ksync::now_ns`, host: process clock).
    pub timeout_ns: u64,
}

/// permd's side (PM-3): told about each new request; answers through `policy::answer`.
pub trait AskListener: Sync {
    fn asked(&self, info: &AskInfo);
}

static LISTENER: OnceRef<dyn AskListener> = OnceRef::new();

pub fn set_ask_listener(l: &'static dyn AskListener) -> bool {
    LISTENER.set(l)
}

struct Pending {
    info: AskInfo,
    learn: Learned,
    /// 0 waiting, 1 allow, 2 deny (written by `answer`, read by the waiter's condition without a lock).
    slot: Arc<AtomicU8>,
    /// Callers waiting on this request; the last one to time out takes it off the list.
    waiters: usize,
}

static PENDING: SpinLock<Vec<Pending>> = SpinLock::named("policy.asks", Vec::new());
static QUEUE: WaitQueue = WaitQueue::new();
static NEXT: AtomicU64 = AtomicU64::new(1);
/// Default 30 s: a person reads the prompt and decides.
static TIMEOUT_NS: AtomicU64 = AtomicU64::new(30_000_000_000);

klog::counter!(ASKED, "policy.asked");
klog::counter!(ANSWERED, "policy.ask_answered");
klog::counter!(TIMEOUTS, "policy.ask_timeouts");

pub fn set_ask_timeout_ns(ns: u64) {
    TIMEOUT_NS.store(ns, Ordering::Relaxed);
}

pub fn ask_timeout_ns() -> u64 {
    TIMEOUT_NS.load(Ordering::Relaxed)
}

pub fn pending_asks() -> Vec<AskInfo> {
    PENDING.lock().iter().map(|p| p.info.clone()).collect()
}

/// Georg's answer to request `id`: `Allow` or `Deny` (an `Ask` counts as deny); `always` also keeps it as a
/// rule for this program. False: no such request (answered already or timed out), or a second answer to the
/// same request that came in at the same time (its "always" rule is the same one and replaces itself).
pub fn answer(id: u64, d: Decision, always: bool) -> bool {
    let allow = d == Decision::Allow;
    let found = {
        let v = PENDING.lock();
        v.iter().find(|p| p.info.id == id).map(|p| (p.info.subject, p.learn.clone()))
    };
    let Some((subject, learn)) = found else {
        return false;
    };
    // The rule first (outside the list lock: the table lock is never taken under it): a released waiter that
    // repeats the access must find it and not ask again.
    if always {
        let mut l = learn;
        l.set_decision(if allow { Decision::Allow } else { Decision::Deny });
        crate::table::learn(subject.program, l);
    }
    let delivered = {
        let mut v = PENDING.lock();
        match v.iter().position(|p| p.info.id == id) {
            Some(i) => {
                // Under the list lock: a waiter whose deadline just passed takes the lock next and sees it.
                v.swap_remove(i).slot.store(if allow { 1 } else { 2 }, Ordering::Release);
                true
            }
            None => false,
        }
    };
    if !delivered {
        return false;
    }
    QUEUE.wake_all();
    ANSWERED.inc();
    klog::event!(Info, "policy", "ask.answered", id = id, program = subject.program.0, pid = subject.pid, allow = allow, always = always);
    true
}

/// Parks the caller until Georg answers or the deadline passes. Returns the final decision (Allow or Deny)
/// and whether it came from the deadline.
pub(crate) fn ask(s: &Subject, a: &Access<'_>, on_timeout: Decision, timeout: u64) -> (Decision, bool) {
    let class = a.class();
    ASKED.inc();
    let what = crate::describe(a);
    if ksync::preempt_count() > 0 || ksync::irq_lock_depth() > 0 {
        TIMEOUTS.inc();
        klog::event!(Warn, "policy", "ask.cannot_wait", program = s.program.0, pid = s.pid, what = what.as_str(), decision = crate::decision_name(on_timeout); "asked while a spin lock is held: the timeout decision applies at once");
        return (on_timeout, true);
    }
    let (info, slot, new) = {
        let mut v = PENDING.lock();
        match v.iter_mut().find(|p| p.info.subject.program == s.program && p.info.class == class && p.info.what == what) {
            Some(p) => {
                p.waiters += 1;
                (p.info.clone(), p.slot.clone(), false)
            }
            None => {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let info = AskInfo { id, subject: *s, class, what, timeout_ns: timeout };
                let slot = Arc::new(AtomicU8::new(0));
                v.push(Pending { info: info.clone(), learn: Learned::of(a, Decision::Allow), slot: slot.clone(), waiters: 1 });
                (info, slot, true)
            }
        }
    };
    if new {
        klog::event!(Info, "policy", "ask.queued", id = info.id, program = s.program.0, pid = s.pid, what = info.what.as_str(), timeout_ns = timeout);
        if let Some(l) = LISTENER.get() {
            l.asked(&info);
        }
    }
    wait(s, info, slot, on_timeout, timeout)
}

fn wait(s: &Subject, info: AskInfo, slot: Arc<AtomicU8>, on_timeout: Decision, timeout: u64) -> (Decision, bool) {
    let id = info.id;
    let answered = QUEUE.wait_until_timeout(|| slot.load(Ordering::Acquire) != 0, timeout);
    if !answered {
        // Off the list under its lock once no waiter is left: a later `answer` finds nothing; one that took
        // the lock first counts (the slot is read below, after the lock).
        let mut v = PENDING.lock();
        if let Some(i) = v.iter().position(|p| p.info.id == id) {
            v[i].waiters -= 1;
            if v[i].waiters == 0 {
                v.swap_remove(i);
            }
        }
    }
    match slot.load(Ordering::Acquire) {
        1 => (Decision::Allow, false),
        2 => (Decision::Deny, false),
        _ => {
            TIMEOUTS.inc();
            klog::event!(Info, "policy", "ask.timeout", id = id, program = s.program.0, pid = s.pid, what = info.what.as_str(), decision = crate::decision_name(on_timeout); "no answer by the deadline: class default");
            (on_timeout, true)
        }
    }
}

pub(crate) fn reason(s: &Subject, a: &Access<'_>, timed_out: bool) -> String {
    let how = if timed_out { "keine Antwort, Vorgabe: nie" } else { "Regel: nie" };
    format!("Programm {} darf nicht: {} ({how})", s.program.0, crate::describe(a))
}
