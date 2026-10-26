//! Deadlines of sleeping threads: a vector sorted by (deadline, thread id), earliest last, each with the CPU the
//! thread went to sleep on, plus the earliest deadline as an atomic, so "anything due?" costs one load. SC-4:
//! the sleep timers (`timers`, one `HrTimer` per CPU armed at the earliest deadline of the sleepers that went to
//! sleep there; INT-A2a2) wake expired sleepers; busy threads' preemption points only catch overdue ones
//! (`timers::fallback`). `expire` is IRQ-safe (CP-5 fires hrtimers from the LAPIC interrupt): the lock is
//! an `IrqSpinLock` taken with `try_lock`, and wake-ups from a handler go onto the CPUs' lock-free wake lists.
//!
//! Nothing allocates on the sleep path: `spawn` reserves one entry per thread (`reserve`), and a thread has at
//! most one deadline at a time. Should the room be missing anyway, `add` refuses and the caller's timed wait
//! turns into a yield (it re-checks its condition and time), never an abort.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::IrqSpinLock;

/// Descending by (deadline, id): the earliest is last. The third field is the CPU whose sleep timer covers it.
static SET: IrqSpinLock<Vec<(u64, u64, usize)>> = IrqSpinLock::named("sched", Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(u64::MAX);

klog::counter!(pub NO_ROOM, "sched.sleepers_no_room");

/// Room for one more sleeper per thread (`spawn`, thread context). `Err`: no memory.
pub(crate) fn reserve() -> Result<(), ()> {
    let threads = crate::table::len() + 1;
    let mut s = SET.lock();
    let extra = threads.saturating_sub(s.len());
    s.try_reserve(extra).map_err(|_| ())
}

fn pos(s: &[(u64, u64, usize)], key: (u64, u64)) -> usize {
    s.partition_point(|e| (e.0, e.1) > key)
}

/// Registers a deadline covered by `cpu`'s sleep timer. False if there is no room (see the module doc; the
/// caller logs it).
pub(crate) fn add(deadline: u64, id: u64, cpu: usize) -> bool {
    {
        let mut s = SET.lock();
        if s.len() == s.capacity() && s.try_reserve(1).is_err() {
            drop(s);
            NO_ROOM.inc();
            return false;
        }
        let i = pos(&s, (deadline, id));
        s.insert(i, (deadline, id, cpu));
    }
    NEXT.fetch_min(deadline, Ordering::AcqRel);
    true
}

/// The earliest deadline `cpu`'s sleep timer covers (u64::MAX: none), `None` when the lock stayed busy (the
/// caller retries a little later). IRQ-safe (bounded lock), a walk from the earliest entry.
pub(crate) fn next_of(cpu: usize) -> Option<u64> {
    let s = lock_bounded()?;
    Some(s.iter().rev().find(|e| e.2 == cpu).map_or(u64::MAX, |e| e.0))
}

/// The sleeper's own clean-up after its block returned (whatever woke it). Removing the earliest deadline
/// (a timed wait whose condition came true first) moves `NEXT` on, so neither the sleep timer nor the
/// fallback keeps chasing a deadline nobody waits for.
pub(crate) fn remove(deadline: u64, id: u64) {
    let mut s = SET.lock();
    let i = pos(&s, (deadline, id));
    if s.get(i).is_some_and(|e| (e.0, e.1) == (deadline, id)) {
        s.remove(i);
        if i == s.len() {
            // Inside the lock, as in `expire`: an `add` is either in `s` already or lowers NEXT after this.
            NEXT.store(s.last().map_or(u64::MAX, |e| e.0), Ordering::Release);
        }
    }
}

/// The earliest deadline (u64::MAX: none).
pub(crate) fn next() -> u64 {
    NEXT.load(Ordering::Acquire)
}

/// What one `expire` pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Expired {
    /// Sleepers this pass woke.
    pub woken: usize,
    /// It gave up on a lock held elsewhere for too long (the caller retries a little later).
    pub contended: bool,
}

/// `try_lock` attempts before `expire` gives up: `add`/`remove` hold the lock for one insert or remove, so a
/// short spin almost always gets it; a bounded one keeps an interrupt handler from waiting on a stuck CPU.
const LOCK_TRIES: u32 = 1024;

fn lock_bounded() -> Option<ksync::IrqSpinLockGuard<'static, Vec<(u64, u64, usize)>>> {
    for _ in 0..LOCK_TRIES {
        if let Some(g) = SET.try_lock() {
            return Some(g);
        }
        core::hint::spin_loop();
    }
    None
}

/// Wakes every sleeper whose deadline passed, one at a time (nothing allocated). Cheap when nothing is due.
/// The lock is taken with a bounded spin (IRQ-safe); on contention the pass ends and says so. `irq`: called
/// from a hard-interrupt handler.
pub(crate) fn expire(now: u64, irq: bool) -> Expired {
    let mut r = Expired::default();
    if now < NEXT.load(Ordering::Acquire) {
        return r;
    }
    loop {
        let due = {
            let Some(mut s) = lock_bounded() else {
                r.contended = true;
                return r;
            };
            let due = s.last().copied().filter(|e| e.0 <= now);
            if due.is_some() {
                s.pop();
            }
            // Inside the lock: an `add` either went in before (and is seen here) or lowers NEXT after this store.
            NEXT.store(s.last().map_or(u64::MAX, |e| e.0), Ordering::Release);
            due
        };
        let Some((d, id, _)) = due else { return r };
        if let Some(t) = crate::table::get(id)
            && t.sleep_deadline.load(Ordering::Acquire) == d
            && crate::run::wake_thread(&t, irq)
        {
            r.woken += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pos;
    use alloc::vec::Vec;

    #[test]
    fn earliest_is_last() {
        let mut s: Vec<(u64, u64, usize)> = Vec::new();
        for k in [(5, 1), (3, 2), (9, 3), (3, 1), (7, 4)] {
            let i = pos(&s, k);
            s.insert(i, (k.0, k.1, k.1 as usize % 2));
        }
        assert_eq!(s, [(9, 3, 1), (7, 4, 0), (5, 1, 1), (3, 2, 0), (3, 1, 1)]);
        assert_eq!(s[pos(&s, (5, 1))], (5, 1, 1));
    }
}
