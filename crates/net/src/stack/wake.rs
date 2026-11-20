//! NW-7: per-connection wake-ups, Linux's `sk_data_ready`/`sk_write_space` on the socket's own wait queue
//! (linux-7.3.1/net/core/sock.c:3659 `sock_def_readable`). smoltcp (feature `async`) calls a connection's waker
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
pub(crate) struct WakeList {
    q: SpinLock<Vec<Arc<SockWake>>>,
    /// A round woke more connections than the list had room for.
    short: AtomicBool,
    /// One of those belongs to a program: it was woken at once and is in no list, so the round hears of it
    /// here (set and taken under `q`'s lock, like a place in the list).
    short_prog: AtomicBool,
}

/// Room the list starts with (one round takes `RX_BUDGET` frames).
const FIRST_ROOM: usize = 64;

impl WakeList {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(WakeList { q: SpinLock::named("net.iface.wakes", Vec::with_capacity(FIRST_ROOM)), short: AtomicBool::new(false), short_prog: AtomicBool::new(false) })
    }

    /// Wakes every queued connection (called with no card's lock held). Returns whether one of them, or one
    /// that found no place in the list since the last call, belongs to a program (its socket waits on
    /// `inet::READY`, which only the caller wakes).
    pub(crate) fn fire_all(&self) -> bool {
        let mut l = self.q.lock();
        let mut prog = self.short_prog.swap(false, Ordering::AcqRel);
        if l.is_empty() {
            return prog;
        }
        SOCK_WAKES.add(l.len() as u64);
        for w in l.drain(..) {
            prog |= w.prog.load(Ordering::Acquire);
            w.fire();
        }
        let room = l.capacity();
        drop(l);
        if self.short.swap(false, Ordering::AcqRel) {
            self.make_room(room * 2);
        }
        prog
    }

    /// Room for `want` connections, made with the lock dropped: the list is filled under a card's lock, where it
    /// must not grow (513 woken connections would ask for two pages there). Without memory it stays as it is.
    fn make_room(&self, want: usize) {
        let mut spare = ksync::Spare::new();
        loop {
            let mut l = self.q.lock();
            let more = want.saturating_sub(l.len());
            if spare.fits(&mut *l, more) {
                return;
            }
            drop(l);
            if spare.grow().is_err() {
                return;
            }
        }
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

    /// Called by smoltcp under the card's lock: only queues (once until fired), and never makes the list larger
    /// there. Without room in it the connection is woken at once (it then waits for the lock like before NW-7)
    /// and the round makes twice the room for the next time (`fire_all`). A program's connection sleeps on
    /// `inet::READY`, not on its own queue, so the round is told that one of them went this way.
    fn wake_by_ref(self: &Arc<Self>) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut l = self.list.q.lock();
        if l.len() < l.capacity() {
            l.push(self.clone());
        } else {
            if self.prog.load(Ordering::Acquire) {
                self.list.short_prog.store(true, Ordering::Release);
            }
            drop(l);
            self.list.short.store(true, Ordering::Release);
            self.fire();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::task::Waker;

    /// More connections wake in one round than the list holds: under the card's lock the list keeps its size
    /// and the ones without a place are woken at once; the round then doubles the room, and the same burst
    /// fits the next time.
    #[test]
    fn a_burst_never_grows_the_list_where_it_is_filled() {
        let list = WakeList::new();
        let socks: Vec<Arc<SockWake>> = (0..3 * FIRST_ROOM).map(|_| SockWake::new(list.clone())).collect();
        let wakers: Vec<Waker> = socks.iter().map(|s| Waker::from(s.clone())).collect();
        let mut room = list.q.lock().capacity();
        assert!(room >= FIRST_ROOM);
        for round in 0..4u64 {
            for w in &wakers {
                w.wake_by_ref();
            }
            let queued = room.min(socks.len());
            // A queued connection that wakes again is not queued twice.
            for w in &wakers[..queued] {
                w.wake_by_ref();
            }
            let l = list.q.lock();
            assert_eq!(l.capacity(), room, "round {round}: the list grew while it was filled");
            assert_eq!(l.len(), queued, "each connection once, as many as fit");
            drop(l);
            // The ones without a place were woken at once; the others wait for the round.
            let woken = socks.iter().filter(|s| s.seq() == round + 1).count();
            assert_eq!(woken, socks.len() - queued, "round {round}");
            assert!(!list.fire_all());
            assert!(socks.iter().all(|s| s.seq() == round + 1), "round {round}: every connection woken exactly once");
            let now = list.q.lock().capacity();
            assert!(if queued < socks.len() { now >= 2 * room } else { now == room }, "round {round}: {room} -> {now}");
            room = now;
        }
        assert!(room >= socks.len(), "the burst fits by now");
    }

    /// A program's connection that finds the list full is woken at once, but the program sleeps on
    /// `inet::READY`, which only the round wakes: the round must hear of it although it is in no list.
    #[test]
    fn a_program_connection_without_a_place_still_reaches_the_round() {
        let list = WakeList::new();
        let room = list.q.lock().capacity();
        let kernel: Vec<Arc<SockWake>> = (0..room).map(|_| SockWake::new(list.clone())).collect();
        let prog = SockWake::new(list.clone());
        prog.set_prog(true);
        for s in &kernel {
            Waker::from(s.clone()).wake_by_ref();
        }
        Waker::from(prog.clone()).wake_by_ref();
        assert_eq!(list.q.lock().len(), room, "the program's connection found no place");
        assert_eq!(prog.seq(), 1, "and was woken at once");
        assert!(list.fire_all(), "the round wakes inet::READY for it");
        assert!(!list.fire_all(), "once");
        // A kernel connection without a place does not wake the programs.
        let room = list.q.lock().capacity();
        let more: Vec<Arc<SockWake>> = (0..room + 1).map(|_| SockWake::new(list.clone())).collect();
        for s in &more {
            Waker::from(s.clone()).wake_by_ref();
        }
        assert_eq!(more[room].seq(), 1);
        assert!(!list.fire_all());
    }
}
