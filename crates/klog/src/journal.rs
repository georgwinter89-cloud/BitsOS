//! RB-8 (B6): the hand-shake between the kernel and the journal service `bitsjournal`, here because every path
//! that ends this kernel (kexec's Reload, platform's and MCP's reboot) depends on klog, while the reader node
//! `/dev/bitsos/events` lives in klogd (which depends on kexec).
//!
//! The service reads the records through the node and tells it after each durable write (fsync) how far it is
//! (`note_ack(next)`: every record with `seq < next` is on /data). `wait_drained` asks it to write what it has
//! now (the node shows the reader a flush request) and waits a bounded time until the service says so. Without a
//! journal attached (nobody acked in this boot, or every acking reader closed) it returns at once. Plain atomics:
//! no lock, no allocation; the caller's `sleep` is the only wait.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// How long a Reload or reboot waits for the journal (B4: bounded; the service usually answers in < 100 ms).
pub const DRAIN_NS: u64 = 2_000_000_000;
/// Poll step of the wait.
const STEP_NS: u64 = 10_000_000;

/// Every record below this sequence number is durable in the journal (this boot's numbers).
static ACKED: AtomicU64 = AtomicU64::new(0);
/// Readers of the node that acked at least once and are still open.
static ATTACHED: AtomicU32 = AtomicU32::new(0);
/// Target + 1 of the pending flush request (0: none).
static REQUEST: AtomicU64 = AtomicU64::new(0);
static DRAINS: AtomicU64 = AtomicU64::new(0);
static TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// A reader acked for the first time (the node calls it once per open file).
pub fn attach() {
    ATTACHED.fetch_add(1, Ordering::AcqRel);
}

/// An acking reader closed.
pub fn detach() {
    let mut n = ATTACHED.load(Ordering::Acquire);
    while n > 0 {
        match ATTACHED.compare_exchange_weak(n, n - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(now) => n = now,
        }
    }
}

pub fn attached() -> u32 {
    ATTACHED.load(Ordering::Acquire)
}

/// The service made every record with `seq < next` durable. Clamped to klog's next number: an ack from the
/// future (a bogus or hostile writer) must not make later drains end at once.
pub fn note_ack(next: u64) {
    ACKED.fetch_max(next.min(crate::next_seq()), Ordering::AcqRel);
}

pub fn acked() -> u64 {
    ACKED.load(Ordering::Acquire)
}

/// The sequence number the journal is asked to reach, if a drain is waiting.
pub fn request() -> Option<u64> {
    REQUEST.load(Ordering::Acquire).checked_sub(1)
}

/// (drains asked, drains that timed out) since boot.
pub fn stats() -> (u64, u64) {
    (DRAINS.load(Ordering::Relaxed), TIMEOUTS.load(Ordering::Relaxed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drain {
    /// No journal attached: nothing to wait for.
    NoJournal,
    /// The journal wrote everything up to `target`.
    Done { target: u64, waited_ns: u64 },
    /// It did not within `max_ns`; `acked` is how far it got.
    Timeout { target: u64, acked: u64, waited_ns: u64 },
}

/// Asks the journal to make every record logged so far durable and waits at most `max_ns` (thread context; the
/// caller's `sleep` waits, e.g. `sched::sleep_ns`). `what` names the caller in the event (B6).
pub fn wait_drained(what: &'static str, max_ns: u64, sleep: &dyn Fn(u64)) -> Drain {
    if attached() == 0 {
        crate::event!(Info, "klog", "journal.drain_skipped", what = what; "no journal attached");
        return Drain::NoJournal;
    }
    DRAINS.fetch_add(1, Ordering::Relaxed);
    let target = crate::next_seq();
    REQUEST.store(target + 1, Ordering::Release);
    let t0 = crate::now();
    let r = loop {
        let waited = crate::now().saturating_sub(t0);
        let a = acked();
        if a >= target {
            break Drain::Done { target, waited_ns: waited };
        }
        if waited >= max_ns || attached() == 0 {
            break Drain::Timeout { target, acked: a, waited_ns: waited };
        }
        sleep(STEP_NS.min(max_ns - waited));
    };
    // A later request may have replaced ours; only ours is cleared.
    let _ = REQUEST.compare_exchange(target + 1, 0, Ordering::AcqRel, Ordering::Acquire);
    match r {
        Drain::Done { waited_ns, .. } => {
            crate::event!(Info, "klog", "journal.drained", what = what, target = target, waited_ms = waited_ns / 1_000_000)
        }
        Drain::Timeout { acked, waited_ns, .. } => {
            TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            crate::event!(Warn, "klog", "journal.drain_timeout", what = what, target = target, acked = acked,
                waited_ms = waited_ns / 1_000_000; "the journal did not write the last records in time: going on without them");
        }
        Drain::NoJournal => {}
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_journal_nothing_waits_and_an_ack_ends_the_wait() {
        // The crate's test lock: the other tests expect a quiet ring while they run.
        let _g = crate::tests::serial_limited();
        assert_eq!(wait_drained("test", DRAIN_NS, &|_| panic!("must not sleep")), Drain::NoJournal);
        attach();
        let target = crate::next_seq();
        let slept = core::cell::Cell::new(0u32);
        let r = wait_drained("test", DRAIN_NS, &|_| {
            assert_eq!(request(), Some(target));
            slept.set(slept.get() + 1);
            note_ack(target + 5);
        });
        assert!(matches!(r, Drain::Done { target: t, .. } if t == target), "{r:?}");
        note_ack(u64::MAX);
        assert!(acked() <= crate::next_seq(), "an ack is never beyond the newest record");
        assert_eq!((slept.get(), request()), (1, None));
        detach();
        assert_eq!(attached(), 0);
        detach();
        assert_eq!(attached(), 0, "never below zero");
    }
}
