//! `C: Pollable` (owner FS) and the poll core of FS-4: readiness of any file-like object and the queues
//! poll/select/epoll wait on.
//!
//! **Readiness** is Linux's poll mask (`File::events`; most objects report IN/OUT and the VFS adds RDNORM/WRNORM
//! as Linux's files do, `File::poll_exact` objects report the whole mask themselves: eventfd, epoll).
//!
//! **`PollQueue`** is Linux's `wait_queue_head_t` for poll: a `WaitQueue` (threads that block in the object's
//! own read/write wait there, through `Deref`) plus *subscribers*: callbacks (`PollWaker`) that poll tables
//! and epoll items hang on it, Linux's `wait_queue_entry_t` with `func`. A wake (`wake_poll(key)`) wakes the
//! threads and calls every subscriber whose interest meets the key (0 = any, as Linux's keyless wakes). An
//! object keeps one queue per direction when Linux does (a pipe's `rd_wait`/`wr_wait`) and names them in
//! `File::poll_wait`, as `f_op->poll` calls `poll_wait` for each.
//!
//! **No lost wake-up** (the same argument as `ksync::WaitQueue`): a subscriber is put on the queue *before*
//! the object's readiness is read (`subscribe` ends with a SeqCst fence), and a waker changes the state
//! *before* it looks for subscribers (`notify` starts with a SeqCst fence). Either the reader sees the new
//! state, or the waker sees the subscriber and calls it. A waiter that subscribes after the event therefore
//! still sees it through `events()`.
//!
//! **Subscriber callbacks** may run with interrupts off and in a hard-interrupt handler (an hrtimer's timerfd):
//! they never allocate, free or sleep and take only `IrqSpinLock`s. The subscriber list lives under an
//! `IrqSpinLock`; it grows outside the lock, and a removed subscriber's `Arc` is dropped outside it. A queue
//! holds a strong reference to each subscriber, so a callback's target never goes away under it.
//!
//! **Nested wakes** (epoll in epoll): an epoll item's callback wakes its epoll's own `PollQueue`, whose
//! subscribers may be items of an outer epoll. Linux takes the inner lock with `spin_lock_irqsave_nested`;
//! here a wake at depth > 0 takes the inner queue's lock with `try_lock` in a loop, which the lock checker does
//! not count as a same-class nesting. It cannot deadlock: a wake only ever descends from a watched object to
//! the epolls watching it, and `epoll_ctl` refuses loops and chains deeper than Linux's `EP_MAX_NESTS`.
//!
//! Objects that expose only a plain `WaitQueue` (`File::poll_wait` names nothing although `File::can_poll`)
//! are looked at again every `LEGACY_SLICE_NS` by a sleeping poll/select (epoll: the same); change requests
//! ask their owners to switch to a `PollQueue` (a drop-in: same `wait_until`, `wake_all`, `wake_one`).

use crate::node::File;
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
use ksync::{IrqSpinLock, WaitQueue};
use uapi::errno::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Events(pub u32);

impl Events {
    pub const IN: u32 = 0x001;
    pub const PRI: u32 = 0x002;
    pub const OUT: u32 = 0x004;
    pub const ERR: u32 = 0x008;
    pub const HUP: u32 = 0x010;
    pub const NVAL: u32 = 0x020;
    pub const RDNORM: u32 = 0x040;
    pub const RDBAND: u32 = 0x080;
    pub const WRNORM: u32 = 0x100;
    pub const WRBAND: u32 = 0x200;
    pub const MSG: u32 = 0x400;
    pub const RDHUP: u32 = 0x2000;
    pub fn contains(self, bits: u32) -> bool {
        self.0 & bits != 0
    }
}

/// How long a sleeping poll/select/epoll_wait looks at the program's kill state at the latest (B4: no wait
/// without a bound; a kill also interrupts the sleep directly where the scheduler kicks the thread).
pub const KILL_SLICE_NS: u64 = 50_000_000;
/// How often a sleeping poll looks again at objects that have no `PollQueue` yet (the AB-3 slice).
pub const LEGACY_SLICE_NS: u64 = 10_000_000;

pub trait Pollable: Send + Sync {
    fn events(&self) -> Events;
    /// Woken whenever `events()` may have changed. G0 stub objects return a queue nobody wakes and report
    /// "always ready".
    fn wait_queue(&self) -> &WaitQueue;
}

/// Blocks until `p` reports any of `want` (or the timeout passes). Returns the events seen.
pub fn wait_for(p: &dyn Pollable, want: u32, timeout_ns: Option<u64>) -> Events {
    let ready = || p.events().contains(want | Events::ERR | Events::HUP);
    match timeout_ns {
        None => p.wait_queue().wait_until(ready),
        Some(t) => {
            p.wait_queue().wait_until_timeout(ready, t);
        }
    }
    p.events()
}

/// A poll table entry or an epoll item: called by `PollQueue` wakes whose key meets its interest.
pub trait PollWaker: Send + Sync {
    /// `key`: the wake's poll bits (0: any change). `depth`: 0 for a wake by the object itself, n for a wake
    /// passed on by n epolls. Runs with the queue's lock held and maybe in a hard-interrupt handler: no
    /// allocation, no freeing, no sleeping, only `IrqSpinLock`s.
    /// Returns whether it really woke a waiter (Linux's return value of a wait entry's `func`): only exclusive
    /// subscribers' answers count (`PollQueue::subscribe_exclusive`).
    fn poll_wake(&self, key: u32, depth: u32) -> bool;
}

struct Sub {
    id: u64,
    /// The bits the subscriber waits for (always with ERR|HUP): wakes with another non-zero key pass it by.
    interest: u32,
    /// Linux `WQ_FLAG_EXCLUSIVE` (an EPOLLEXCLUSIVE item): a wake stops after the first exclusive subscriber
    /// that woke someone (`__wake_up_common` with nr_exclusive 1); `wake_all` reaches them all.
    exclusive: bool,
    w: Arc<dyn PollWaker>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh subscription id (one per poll-table entry or epoll item; the same id on each queue of a file).
pub fn new_sub_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

klog::counter!(POLL_NO_MEMORY, "vfs.poll.no_memory");
klog::counter!(POLL_LEGACY, "vfs.poll.legacy_items");

/// File types (`S_IFMT >> 12`) already reported by `note_legacy`.
static LEGACY_SEEN: AtomicU32 = AtomicU32::new(0);

/// A poll table or epoll item met an object that may change but names no `PollQueue` (B6): counted each time,
/// logged once per file type, so the desk sees which lane's objects are still looked at in slices.
pub(crate) fn note_legacy(f: &dyn File) {
    POLL_LEGACY.inc();
    let ty = (f.mode() & uapi::S_IFMT) >> 12;
    let bit = 1u32 << (ty & 31);
    if LEGACY_SEEN.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return;
    }
    let kind = match f.mode() & uapi::S_IFMT {
        crate::abi::S_IFSOCK => "socket",
        uapi::S_IFCHR => "char device",
        uapi::S_IFIFO => "fifo",
        _ => "other",
    };
    let name = f.link_name().unwrap_or_default();
    klog::event!(Info, "vfs", "poll.legacy", kind = kind, name = name.as_str(); "an object without a PollQueue is watched: looked at again every 10 ms until its owner names one");
}

/// The queue an object wakes when its readiness may have changed (module doc).
pub struct PollQueue {
    wq: WaitQueue,
    /// Wakes so far (tests; an edge counter).
    seq: AtomicU64,
    subs: IrqSpinLock<Vec<Sub>>,
    /// `subs.len()`, kept under the lock; read without it by wakers (fast path).
    nsubs: AtomicUsize,
}

impl PollQueue {
    pub const fn new() -> Self {
        PollQueue { wq: WaitQueue::new(), seq: AtomicU64::new(0), subs: IrqSpinLock::named("vfs.poll.subs", Vec::new()), nsubs: AtomicUsize::new(0) }
    }

    /// Wakes every thread and every subscriber, exclusive ones included (Linux `wake_up_all`, key 0). Safe in
    /// interrupt handlers.
    pub fn wake_all(&self) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.wq.wake_all();
        self.notify(0, 0, 0);
    }

    /// Wakes one thread, every plain subscriber and the first exclusive one that wakes someone (Linux
    /// `wake_up`).
    pub fn wake_one(&self) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.wq.wake_one();
        self.notify(0, 0, 1);
    }

    /// Wakes the threads and the subscribers whose interest meets `key`, of the exclusive ones only the first
    /// that wakes someone (Linux `wake_up_poll(q, key)`).
    pub fn wake_poll(&self, key: u32) {
        self.wake_nested(key, 0);
    }

    /// A wake passed on by an epoll (`depth` epolls between the object and this queue).
    pub(crate) fn wake_nested(&self, key: u32, depth: u32) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.wq.wake_all();
        self.notify(key, depth, 1);
    }

    /// Threads waiting plus subscribers (a waker may skip the wake when this is 0, after changing its state).
    pub fn waiter_count(&self) -> usize {
        fence(Ordering::SeqCst);
        self.wq.waiter_count() + self.nsubs.load(Ordering::Relaxed)
    }

    /// Subscribers now (tests, diagnostics).
    pub fn subscribers(&self) -> usize {
        self.nsubs.load(Ordering::Relaxed)
    }

    /// Wakes so far.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// `__wake_up_common`: every plain subscriber the key concerns, then exclusive ones (Linux keeps them at the
    /// tail) until `nr_exclusive` of them woke someone (0: all).
    fn notify(&self, key: u32, depth: u32, nr_exclusive: usize) {
        // Pairs with `subscribe`'s fence (module doc): the caller's state change before our look at `nsubs`.
        fence(Ordering::SeqCst);
        if self.nsubs.load(Ordering::Relaxed) == 0 {
            return;
        }
        let g = if depth == 0 {
            self.subs.lock()
        } else {
            // Nested epoll wake (module doc): ordered by the epoll graph, not by the lock checker.
            loop {
                if let Some(g) = self.subs.try_lock() {
                    break g;
                }
                core::hint::spin_loop();
            }
        };
        let concerns = |s: &Sub| key == 0 || key & s.interest != 0;
        for s in g.iter().filter(|s| !s.exclusive && concerns(s)) {
            s.w.poll_wake(key, depth);
        }
        let mut left = nr_exclusive;
        for s in g.iter().filter(|s| s.exclusive && concerns(s)) {
            if s.w.poll_wake(key, depth) && nr_exclusive != 0 {
                left -= 1;
                if left == 0 {
                    break;
                }
            }
        }
    }

    /// Hangs `w` on the queue under `id` (Linux `add_wait_queue` of a poll entry). ENOMEM when no room can be
    /// had. The caller reads the object's readiness only after this returned (no lost wake-up).
    pub fn subscribe(&self, id: u64, interest: u32, w: Arc<dyn PollWaker>) -> Result<(), Errno> {
        self.subscribe_as(id, interest, w, false)
    }

    /// `add_wait_queue_exclusive` (an EPOLLEXCLUSIVE epoll item): see `Sub::exclusive`.
    pub fn subscribe_exclusive(&self, id: u64, interest: u32, w: Arc<dyn PollWaker>) -> Result<(), Errno> {
        self.subscribe_as(id, interest, w, true)
    }

    fn subscribe_as(&self, id: u64, interest: u32, w: Arc<dyn PollWaker>, exclusive: bool) -> Result<(), Errno> {
        let mut w = Some(w);
        loop {
            let need = {
                let mut g = self.subs.lock();
                if g.len() < g.capacity() {
                    if let Some(w) = w.take() {
                        g.push(Sub { id, interest, exclusive, w });
                    }
                    self.nsubs.store(g.len(), Ordering::Relaxed);
                    break;
                }
                g.capacity().max(2) * 2
            };
            // More room, made outside the lock; the old list is moved over (no allocation under the lock)
            // and its memory freed outside.
            let mut fresh: Vec<Sub> = Vec::new();
            if fresh.try_reserve_exact(need).is_err() {
                POLL_NO_MEMORY.inc();
                klog::event!(Warn, "vfs", "poll.no_memory", subscribers = need / 2; "no memory to hang one more waiter on a poll queue: ENOMEM");
                return Err(ENOMEM);
            }
            let old = {
                let mut g = self.subs.lock();
                if g.capacity() >= need {
                    fresh
                } else {
                    fresh.extend(g.drain(..));
                    core::mem::replace(&mut *g, fresh)
                }
            };
            drop(old);
        }
        // Pairs with `notify`'s fence (module doc).
        fence(Ordering::SeqCst);
        Ok(())
    }

    /// Takes the subscription `id` off (if it is there); its callback is not called any more afterwards.
    pub fn unsubscribe(&self, id: u64) {
        let gone = {
            let mut g = self.subs.lock();
            let gone = g.iter().position(|s| s.id == id).map(|i| g.remove(i));
            self.nsubs.store(g.len(), Ordering::Relaxed);
            gone
        };
        drop(gone);
    }
}

impl Default for PollQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Threads wait on a `PollQueue` like on any `WaitQueue` (`wait_until`, `wait_until_timeout`). Wake it through
/// its own `wake_*` methods (they reach the subscribers too), never through the inner queue.
impl Deref for PollQueue {
    type Target = WaitQueue;
    fn deref(&self) -> &WaitQueue {
        &self.wq
    }
}

/// `f`'s poll mask as Linux's `vfs_poll` gives it: objects that report IN/OUT get RDNORM/WRNORM beside them as
/// Linux's files report them (`File::poll_exact` objects say everything themselves).
pub fn file_mask(f: &dyn File) -> u32 {
    let m = f.events().0;
    if f.poll_exact() {
        return m;
    }
    let mut m = m & !(Events::RDNORM | Events::WRNORM);
    if m & Events::IN != 0 {
        m |= Events::RDNORM;
    }
    if m & Events::OUT != 0 {
        m |= Events::WRNORM;
    }
    m
}

/// Monotonic nanoseconds (the scheduler's clock = CLOCK_MONOTONIC; host tests: the process clock).
pub fn now() -> u64 {
    if let Some(n) = ksync::now_ns() {
        return n;
    }
    host_now()
}

#[cfg(test)]
fn host_now() -> u64 {
    extern crate std;
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

#[cfg(not(test))]
fn host_now() -> u64 {
    0
}

/// The waker of one poll/select call (`poll_wqueues`): any wake of a subscribed queue marks it and wakes the
/// caller.
struct TableWaker {
    woken: AtomicBool,
    wq: WaitQueue,
}

impl PollWaker for TableWaker {
    fn poll_wake(&self, _key: u32, _depth: u32) -> bool {
        self.woken.store(true, Ordering::SeqCst);
        self.wq.wake_all();
        true
    }
}

/// One poll/select call's registrations (Linux `struct poll_wqueues`): every file it looked at while
/// registering is subscribed with the caller's interest; all of them are taken off when the table goes.
pub struct PollTable {
    w: Arc<TableWaker>,
    regs: Vec<(Arc<dyn File>, u64)>,
    registering: bool,
    /// A file that may change but has no `PollQueue` (looked at again every `LEGACY_SLICE_NS`).
    legacy: bool,
    /// Linux's `table.error`: a registration that found no memory (the call answers ENOMEM if nothing is
    /// ready).
    pub error: Option<Errno>,
}

impl PollTable {
    /// `register`: whether the call may sleep (a zero timeout never registers, as `_qproc = NULL`).
    pub fn new(register: bool) -> Self {
        PollTable { w: Arc::new(TableWaker { woken: AtomicBool::new(false), wq: WaitQueue::new() }), regs: Vec::new(), registering: register, legacy: false, error: None }
    }

    /// `vfs_poll(f, pt)` with the key `interest`: subscribes first (while registering), then reads the mask.
    pub fn poll(&mut self, f: &Arc<dyn File>, interest: u32) -> u32 {
        if self.registering {
            self.register(f, interest);
        }
        file_mask(&**f)
    }

    fn register(&mut self, f: &Arc<dyn File>, interest: u32) {
        if self.regs.try_reserve(1).is_err() {
            POLL_NO_MEMORY.inc();
            self.error = Some(ENOMEM);
            return;
        }
        let id = new_sub_id();
        let mut any = false;
        let mut err = None;
        let w: Arc<dyn PollWaker> = self.w.clone();
        f.poll_wait(&mut |q| {
            any = true;
            if let Err(e) = q.subscribe(id, interest, w.clone()) {
                err = Some(e);
            }
        });
        if any {
            self.regs.push((f.clone(), id));
        } else if f.can_poll() {
            note_legacy(&**f);
            self.legacy = true;
        }
        if let Some(e) = err {
            self.error = Some(e);
        }
    }

    /// Stops registering (something is ready: Linux clears `_qproc`).
    pub fn stop_registering(&mut self) {
        self.registering = false;
    }

    /// Forgets earlier wakes before a scan (a wake during the scan makes the next sleep return at once).
    pub fn arm(&self) {
        self.w.woken.store(false, Ordering::SeqCst);
    }

    /// Sleeps until a subscribed queue was woken, `interrupted()` (the program is being ended or, from AB-6 on, a
    /// signal waits for its handler), `end` passed or a slice ended.
    /// Returns whether `end` has passed (Linux `timed_out`).
    pub fn sleep(&self, end: Option<u64>, interrupted: &dyn Fn() -> bool) -> bool {
        let t0 = now();
        if end.is_some_and(|e| t0 >= e) {
            return true;
        }
        let slice = if self.legacy { LEGACY_SLICE_NS } else { KILL_SLICE_NS };
        let t = end.map_or(slice, |e| (e - t0).min(slice));
        let w = &self.w;
        w.wq.wait_until_timeout(|| w.woken.load(Ordering::SeqCst) || interrupted(), t);
        end.is_some_and(|e| now() >= e)
    }
}

impl Drop for PollTable {
    /// `poll_freewait`: every subscription goes.
    fn drop(&mut self) {
        for (f, id) in self.regs.drain(..) {
            f.poll_wait(&mut |q| q.unsubscribe(id));
        }
    }
}
