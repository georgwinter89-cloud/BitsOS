//! NW-7: per-connection wake-ups, Linux's `sk_data_ready`/`sk_write_space` on the socket's own wait queue
//! (linux-6.6/net/core/sock.c:3315 `sock_def_readable`). smoltcp (feature `async`) calls a connection's waker
//! when data arrived, room came free or the state changed; the waker only queues the connection on the stack's
//! list, and the poll round wakes the list once no card's lock is held. Before NW-7 every round with
//! traffic woke every waiting kernel connection, and each one took the card's lock to look: with 64 downloads on
//! 16 CPUs that kept the lock busy and a new connection (MCP) waited seconds.

use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};

klog::counter!(pub SOCK_WAKES, "net.sock_wakes");

/// A wait for a change is cut into slices this long (B4); a slice that ends without a wake just looks again,
/// so a change smoltcp does not report (none known) costs at most this much.
pub(crate) const WAKE_SLICE_NS: u64 = 100_000_000;

/// The connections smoltcp woke since the last round.
pub(crate) struct WakeList(SpinLock<Vec<Arc<SockWake>>>);

impl WakeList {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(WakeList(SpinLock::named("net.iface.wakes", Vec::new())))
    }

    /// Wakes every queued connection (called with no card's lock held). Returns whether one of them belongs to
    /// a program (its socket waits on `inet::READY`).
    pub(crate) fn fire_all(&self) -> bool {
        let mut l = self.0.lock();
        if l.is_empty() {
            return false;
        }
        SOCK_WAKES.add(l.len() as u64);
        let mut prog = false;
        for w in l.drain(..) {
            prog |= w.prog.load(Ordering::Acquire);
            w.fire();
        }
        prog
    }
}

/// One connection's wake-up: a change counter its waiters compare against and the queue they sleep on.
pub(crate) struct SockWake {
    list: Arc<WakeList>,
    /// A program's socket: its waits are on `inet::READY`, which the round wakes when this one fired.
    prog: AtomicBool,
    queued: AtomicBool,
    seq: AtomicU64,
    wq: WaitQueue,
}

impl SockWake {
    pub(crate) fn new(list: Arc<WakeList>) -> Arc<Self> {
        Arc::new(SockWake { list, prog: AtomicBool::new(false), queued: AtomicBool::new(false), seq: AtomicU64::new(0), wq: WaitQueue::new() })
    }

    pub(crate) fn set_prog(&self, prog: bool) {
        self.prog.store(prog, Ordering::Release);
    }

    /// The change counter: read it before looking at the connection, then `wait` for it to move.
    pub(crate) fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Wakes the waiters now (a reset by another thread, the round's list); a program's socket also its queue.
    pub(crate) fn fire(&self) {
        self.queued.store(false, Ordering::Release);
        self.seq.fetch_add(1, Ordering::AcqRel);
        if self.wq.waiter_count() > 0 {
            self.wq.wake_all();
        }
    }

    /// Sleeps until the counter moved past `seen`, `stop()` holds, or `timeout_ns` passed.
    pub(crate) fn wait(&self, seen: u64, timeout_ns: u64, stop: impl Fn() -> bool) -> bool {
        self.wq.wait_until_timeout(|| self.seq() != seen || stop(), timeout_ns.max(1))
    }
}

impl Wake for SockWake {
    fn wake(self: Arc<Self>) {
        Self::wake_by_ref(&self);
    }

    /// Called by smoltcp under the card's lock: only queues (once until fired). Without room in the list the
    /// connection is woken at once (it then waits for the lock like before NW-7).
    fn wake_by_ref(self: &Arc<Self>) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut l = self.list.0.lock();
        if l.try_reserve(1).is_ok() {
            l.push(self.clone());
        } else {
            drop(l);
            self.fire();
        }
    }
}
