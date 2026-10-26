//! Work that may sleep on a device (the boot stick's FAT, a write-back), run in a thread of its own so the caller
//! waits a bounded time (B4): `reboot`'s write-back before the reset and the stick listing behind "next start"
//! (MC-4b fixer). A helper that misses the deadline runs on and its result is dropped; `Single` keeps a slow
//! device from collecting one stuck helper per call.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{SpinLock, WaitQueue};

pub enum Bounded<T> {
    Done(T),
    /// The deadline passed; the helper still runs.
    TimedOut,
    /// No thread could be started (no memory for its stack).
    NoThread,
    /// One of this kind still runs (`Single`): nothing was started.
    Busy,
}

struct Slot<T> {
    v: SpinLock<Option<T>>,
    done: AtomicBool,
    wq: WaitQueue,
}

/// Runs `f` in a new thread of class Normal and waits at most `timeout_ns` for it.
pub fn run<T: Send + 'static>(name: &str, timeout_ns: u64, f: impl FnOnce() -> T + Send + 'static) -> Bounded<T> {
    let slot = Arc::new(Slot { v: SpinLock::named("mcp.bounded", None), done: AtomicBool::new(false), wq: WaitQueue::new() });
    let s = slot.clone();
    let started = sched::spawn(name, sched::Class::Normal, move || {
        let v = f();
        *s.v.lock() = Some(v);
        s.done.store(true, Ordering::Release);
        s.wq.wake_all();
    });
    if started.is_err() {
        return Bounded::NoThread;
    }
    if !slot.wq.wait_until_timeout(|| slot.done.load(Ordering::Acquire), timeout_ns) {
        return Bounded::TimedOut;
    }
    match slot.v.lock().take() {
        Some(v) => Bounded::Done(v),
        None => Bounded::TimedOut,
    }
}

/// At most one helper of a kind at a time: a stick that hangs holds one thread, not one per `hw_info` call.
pub struct Single(AtomicBool);

impl Single {
    pub const fn new() -> Single {
        Single(AtomicBool::new(false))
    }

    pub fn run<T: Send + 'static>(&'static self, name: &str, timeout_ns: u64, f: impl FnOnce() -> T + Send + 'static) -> Bounded<T> {
        if self.0.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Bounded::Busy;
        }
        let r = run(name, timeout_ns, move || {
            let v = f();
            self.0.store(false, Ordering::Release);
            v
        });
        if matches!(r, Bounded::NoThread) {
            self.0.store(false, Ordering::Release);
        }
        r
    }
}

/// `mc.bounded` (Safe, rerunnable): a quick helper is waited for, a slow one is cut off at the deadline and runs
/// out by itself, and `Single` starts no second helper while the first still runs.
#[cfg(target_os = "none")]
mod ktests {
    use super::*;
    use ktest::{TestResult, kassert, ktest};

    const MS: u64 = 1_000_000;

    fn bounded_waits_and_cuts_off() -> TestResult {
        kassert!(matches!(run("mc-bounded-quick", 1000 * MS, || 7u32), Bounded::Done(7)), "a quick helper is waited for");
        let ran_out = Arc::new(AtomicBool::new(false));
        let r = ran_out.clone();
        let t0 = time::now_ns();
        let slow = run("mc-bounded-slow", 50 * MS, move || {
            sched::sleep_ns(300 * MS);
            r.store(true, Ordering::Release);
        });
        let waited = time::now_ns() - t0;
        kassert!(matches!(slow, Bounded::TimedOut), "a slow helper is cut off at the deadline");
        kassert!(waited < 250 * MS, "the caller waited {} ms for a 50 ms deadline", waited / MS);
        static ONE: Single = Single::new();
        let first = ONE.run("mc-bounded-one", 10 * MS, || sched::sleep_ns(200 * MS));
        kassert!(matches!(first, Bounded::TimedOut), "the first single helper is cut off");
        kassert!(matches!(ONE.run("mc-bounded-two", 10 * MS, || ()), Bounded::Busy), "no second helper while the first runs");
        // Both slow helpers end by themselves; then Single takes a new one again.
        let end = time::now_ns() + 2000 * MS;
        while !ran_out.load(Ordering::Acquire) && time::now_ns() < end {
            sched::sleep_ns(10 * MS);
        }
        kassert!(ran_out.load(Ordering::Acquire), "the cut-off helper ran out by itself");
        let mut again = Bounded::Busy;
        while time::now_ns() < end {
            again = ONE.run("mc-bounded-again", 1000 * MS, || 1u8);
            if !matches!(again, Bounded::Busy) {
                break;
            }
            sched::sleep_ns(10 * MS);
        }
        kassert!(matches!(again, Bounded::Done(1)), "Single takes a helper again once the first ended");
        Ok(())
    }
    ktest!("mc.bounded", Safe, bounded_waits_and_cuts_off);
}
