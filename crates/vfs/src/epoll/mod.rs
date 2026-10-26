//! epoll (FS-4), after linux-6.6/fs/eventpoll.c.
//!
//! An `Epoll` is Linux's `struct eventpoll` behind an anonymous file ("[eventpoll]", F_GETFL O_RDWR, fstat
//! the anon inode, `/proc/<pid>/fd` "anon_inode:[eventpoll]"). Its items (`EpItem` = `struct epitem`) are keyed
//! by **(open file description, fd number)** as Linux's `epoll_filefd`: closing one fd of a dup'd file keeps the
//! registration (it goes when the open file description itself goes: `OpenFile`'s `Drop` calls
//! `release_file`, Linux `eventpoll_release_file`), EPOLL_CTL_DEL by the closed number is EBADF and by the other
//! number ENOENT.
//!
//! An item hangs on every `PollQueue` its file names (`File::poll_wait`) with itself as the callback
//! (`ep_poll_callback`): a wake whose key meets the item's events puts it on the ready list (once) and wakes one
//! `epoll_wait` sleeper (exclusive, as Linux) and the epoll's own `PollQueue` (poll/select of the epoll fd, outer
//! epolls). `epoll_wait` (`ep_poll` + `ep_send_events`) polls each ready item again and reports
//! `mask & events`; level-triggered items go back to the end of the list, EPOLLET items leave it until the next
//! wake, EPOLLONESHOT items are disabled until EPOLL_CTL_MOD. An item added or modified while its file is ready is
//! queued at once (`ep_insert`/`ep_modify`), so a waiter that registers after the event still sees it.
//!
//! Item states (under the ready lock): `IDLE` (not queued), `READY` (on the ready list), `SCAN` (taken by a harvest:
//! a wake meanwhile sets `rearm`, Linux's `ovflist`, and the item goes back on the list after the scan). The ready
//! list never allocates in a callback: its room is kept at least the number of items (each is on it at most once).
//!
//! Locks: `items` (`ep->mtx`, a sleeping mutex, held by ctl and by a harvest while it copies events to the
//! program: `vfs.epoll` > `mm.as`), `ready` (`ep->lock`, an IrqSpinLock taken by callbacks), the global `NEST`
//! mutex (`epnested_mutex`) around the loop and path checks of nested epolls. An epoll's readiness for an outer
//! epoll or poll() (`__ep_eventpoll_poll`) reads only its ready list, without its mutex, so no two `vfs.epoll`
//! mutexes are ever nested; an answer "not ready" given while a harvest had the items is made good by a wake of
//! the epoll's `PollQueue` when the harvest puts items back (`readiness`, `done_scan`). Items, files and epolls
//! are dropped only after every lock is let go.
//!
//! Nesting (`ep_loop_check`, `reverse_path_check`): adding an epoll into another refuses a loop or a chain deeper
//! than `EP_MAX_NESTS` below it with ELOOP; adding anything below an epoll that is itself watched checks the paths
//! up from each file: deeper than `EP_MAX_NESTS` or more wake-up paths than Linux's `path_limits` give EINVAL.
//! EPOLLEXCLUSIVE is accepted with Linux's rules (ADD only, not on epolls, only IN/OUT/ERR/HUP/WAKEUP/ET besides);
//! such an item hangs on its file's queues as an exclusive subscriber, so a wake stops after the first exclusive
//! item whose epoll had a sleeper for that key (`add_wait_queue_exclusive`; `wake_all` still reaches every
//! one). EPOLLWAKEUP is
//! dropped for callers other than root (no CAP_BLOCK_SUSPEND) and has no effect otherwise (no suspend).

use crate::fd::OpenFile;
use crate::node::File;
use crate::poll::{self, Events, PollQueue, PollWaker};
use crate::Errno;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use ksync::{IrqSpinLock, Mutex, SpinLock, WaitQueue};
use uapi::errno::*;

mod nest;
use nest::{loop_check, reverse_path_check};

pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLL_CTL_MOD: i32 = 3;
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub const EPOLLWAKEUP: u32 = 1 << 29;
pub const EPOLLONESHOT: u32 = 1 << 30;
pub const EPOLLET: u32 = 1 << 31;
pub const EPOLL_CLOEXEC: u64 = crate::abi::O_CLOEXEC;
/// `EP_PRIVATE_BITS`: never reported, never in a poll mask.
const PRIVATE: u32 = EPOLLWAKEUP | EPOLLONESHOT | EPOLLET | EPOLLEXCLUSIVE;
/// `EPOLLEXCLUSIVE_OK_BITS`.
const EXCLUSIVE_OK: u32 = Events::IN | Events::OUT | Events::ERR | Events::HUP | EPOLLWAKEUP | EPOLLET | EPOLLEXCLUSIVE;
/// Linux's nesting bound (a bound against hostile chains, not a count of things: R1 allows it).
pub const EP_MAX_NESTS: u32 = 4;
/// `EP_MAX_EVENTS` = INT_MAX / sizeof(struct epoll_event) (12 bytes, packed on x86-64).
pub const EP_MAX_EVENTS: i64 = i32::MAX as i64 / 12;

const IDLE: u8 = 0;
const READY: u8 = 1;
const SCAN: u8 = 2;

klog::counter!(EPOLLS, "vfs.epolls");
klog::counter!(EPOLL_ITEMS, "vfs.epoll.items");
klog::counter!(EPOLL_NO_ROOM, "vfs.epoll.no_room");

/// Serialises the loop and path checks of nested epolls (`epnested_mutex`).
static NEST: Mutex<()> = Mutex::named("vfs.epoll.nest", ());
/// `loop_check_gen`: an epoll whose `visit` equals it is being walked by a ctl holding `NEST`.
static LOOP_GEN: AtomicU64 = AtomicU64::new(1);

/// Ends a full check's generation when dropped (before `NEST` is let go).
struct GenEnd;

impl Drop for GenEnd {
    fn drop(&mut self) {
        LOOP_GEN.fetch_add(1, Ordering::SeqCst);
    }
}

/// One watched (open file, fd) of one epoll.
pub(crate) struct EpItem {
    /// Strong: a callback's epoll never goes away under it (the cycle is broken by DEL, close and the epoll's
    /// own release, which take every item out).
    ep: Arc<EpInner>,
    me: Weak<EpItem>,
    key: (usize, i32),
    file: Arc<dyn File>,
    /// The open file description (its link list names this item; unlinked at DEL and at the epoll's end).
    owner: Weak<OpenFile>,
    /// The subscription id on the file's queues.
    id: u64,
    events: AtomicU32,
    data: AtomicU64,
    /// IDLE / READY / SCAN, changed under `ep.ready`.
    state: AtomicU8,
    rearm: AtomicBool,
    /// Taken out (DEL, close, the epoll's end): callbacks and harvests pass it by.
    dead: AtomicBool,
    /// The file names no `PollQueue` but may change: looked at again by every sleeping epoll_wait slice.
    legacy: bool,
    /// A legacy item's mask at the last look (an EPOLLET legacy item is queued only for new bits).
    last: AtomicU32,
}

impl EpItem {
    /// `ep_item_poll`: the file's mask limited to the item's events (an epoll file: its ready-list check).
    fn poll(&self) -> u32 {
        poll::file_mask(&*self.file) & self.events.load(Ordering::SeqCst)
    }
}

impl PollWaker for EpItem {
    /// `ep_poll_callback`. The answer counts only for an EPOLLEXCLUSIVE item (`PollQueue::subscribe_exclusive`):
    /// it woke an epoll_wait sleeper of its epoll with a key it waits for (linux-6.6/fs/eventpoll.c:1214).
    fn poll_wake(&self, key: u32, depth: u32) -> bool {
        let ev = self.events.load(Ordering::SeqCst);
        // Disabled (EPOLLONESHOT after its event) or a key that does not concern the item.
        if ev & !PRIVATE == 0 || (key != 0 && key & ev == 0) || self.dead.load(Ordering::SeqCst) {
            return ev & EPOLLEXCLUSIVE == 0;
        }
        let had_sleepers = self.ep.queue(self, depth);
        if ev & EPOLLEXCLUSIVE == 0 {
            return true;
        }
        had_sleepers
            && match key & (Events::IN | Events::OUT) {
                Events::IN => ev & Events::IN != 0,
                Events::OUT => ev & Events::OUT != 0,
                0 => true,
                _ => false,
            }
    }
}

/// Where a harvest puts its events (the program's array, or a kernel buffer).
pub trait EventSink {
    /// One `struct epoll_event`; false when it could not be written (a fault).
    fn put(&mut self, events: u32, data: u64) -> bool;
}

impl EventSink for Vec<(u32, u64)> {
    fn put(&mut self, events: u32, data: u64) -> bool {
        if self.try_reserve(1).is_err() {
            return false;
        }
        self.push((events, data));
        true
    }
}

pub(crate) struct EpInner {
    /// `ep->mtx` + the rb-tree: items sorted by key.
    items: Mutex<Vec<Arc<EpItem>>>,
    /// `ep->rdllist` under `ep->lock`.
    ready: IrqSpinLock<VecDeque<Arc<EpItem>>>,
    /// A callback found no room on the ready list (should not happen: room is kept); the next harvest looks at
    /// every item.
    lost: AtomicBool,
    /// `ep->wq`: epoll_wait sleepers.
    wq: WaitQueue,
    /// `ep->poll_wait`: poll/select of this epoll and outer epolls.
    pq: PollQueue,
    /// Items whose file has no `PollQueue` (looked at again in slices).
    legacy: AtomicUsize,
    /// The loop check's visit mark (Linux `ep->gen`) and the depth found then (`ep->loop_check_depth`).
    visit: AtomicU64,
    depth: AtomicU32,
    /// Its own open file (`ep->file`): the epolls watching this one hang on that file's links.
    file: SpinLock<Weak<OpenFile>>,
    /// A harvest has its items in SCAN (set and cleared under `ready`).
    in_scan: AtomicBool,
    /// Bumped under `ready` when a harvest starts and when it ends (`readiness` notices a harvest meanwhile).
    scan_gen: AtomicU64,
    /// `readiness` answered "not ready" while a harvest had the items: the harvest's end wakes `pq` (Linux's
    /// `__ep_eventpoll_poll` would have waited for `ep->mtx` and seen the items put back).
    owed: AtomicBool,
}

impl EpInner {
    /// Puts `it` on the ready list unless it is there (a callback: no allocation, no freeing) and wakes the
    /// waiters. Like `ep_poll_callback` it wakes also when the item was queued already: an outer epoll's EPOLLET
    /// item sees one edge per wake of the inner one.
    /// Returns whether epoll_wait sleepers were there (Linux `waitqueue_active(&ep->wq)`, for EPOLLEXCLUSIVE).
    fn queue(&self, it: &EpItem, depth: u32) -> bool {
        {
            let mut r = self.ready.lock();
            match it.state.load(Ordering::SeqCst) {
                IDLE => {
                    if r.len() < r.capacity() {
                        match it.me.upgrade() {
                            Some(me) => {
                                r.push_back(me);
                                it.state.store(READY, Ordering::SeqCst);
                            }
                            None => return false,
                        }
                    } else {
                        self.lost.store(true, Ordering::SeqCst);
                        EPOLL_NO_ROOM.inc();
                    }
                }
                SCAN => it.rearm.store(true, Ordering::SeqCst),
                _ => {}
            }
        }
        self.wake(depth)
    }

    /// Wakes one epoll_wait sleeper and, when someone watches this epoll, its `poll_wait` queue (key EPOLLIN).
    /// Returns whether there were sleepers.
    fn wake(&self, depth: u32) -> bool {
        let had = self.wq.waiter_count() > 0;
        self.wq.wake_one();
        self.pq.wake_nested(Events::IN, depth + 1);
        had
    }

    fn has_ready(&self) -> bool {
        !self.ready.lock().is_empty() || self.lost.load(Ordering::SeqCst)
    }

    /// Room on the ready list for `n` items (and a few being taken out meanwhile), made outside its lock.
    fn reserve_ready(&self, n: usize) -> Result<(), Errno> {
        let n = n + n / 8 + 4;
        let cap = self.ready.lock().capacity();
        if cap >= n {
            return Ok(());
        }
        let mut fresh: VecDeque<Arc<EpItem>> = VecDeque::new();
        fresh.try_reserve_exact(n.max(cap * 2)).map_err(|_| ENOMEM)?;
        let old = {
            let mut r = self.ready.lock();
            if r.capacity() >= n {
                fresh
            } else {
                fresh.extend(r.drain(..));
                core::mem::replace(&mut *r, fresh)
            }
        };
        drop(old);
        Ok(())
    }

    /// Takes `it` off the ready list if it is there (the caller drops what comes back after the lock). Only a
    /// READY item is searched for (interrupts are off under this lock: an idle or scanned item costs O(1)).
    fn unqueue(&self, it: &Arc<EpItem>) -> Option<Arc<EpItem>> {
        let mut r = self.ready.lock();
        if it.state.load(Ordering::SeqCst) != READY {
            return None;
        }
        let pos = r.iter().position(|x| Arc::ptr_eq(x, it))?;
        it.state.store(IDLE, Ordering::SeqCst);
        r.remove(pos)
    }

    /// `__ep_eventpoll_poll`: EPOLLIN|EPOLLRDNORM when an item on the ready list is really ready; the items in
    /// front of it that are not are taken off. Needs no mutex (module doc): Linux waits for `ep->mtx` while a
    /// harvest has the items; here an answer "not ready" given during a harvest is made good by a wake of `pq`
    /// when the harvest puts items back (`owed`), and a harvest that ran during the look makes it look again.
    fn readiness(&self) -> u32 {
        const READY_MASK: u32 = Events::IN | Events::RDNORM;
        for _ in 0..4 {
            if self.lost.load(Ordering::SeqCst) {
                return READY_MASK;
            }
            let (snap, seen_gen): (Vec<Arc<EpItem>>, u64) = loop {
                let n = {
                    let r = self.ready.lock();
                    if r.is_empty() {
                        if self.in_scan.load(Ordering::SeqCst) {
                            self.owed.store(true, Ordering::SeqCst);
                        }
                        return 0;
                    }
                    r.len()
                };
                let mut v = Vec::new();
                if v.try_reserve_exact(n).is_err() {
                    // No memory for a look: say ready (a spurious wake-up, never a lost one).
                    return READY_MASK;
                }
                let r = self.ready.lock();
                if r.len() > n {
                    continue;
                }
                v.extend(r.iter().cloned());
                break (v, self.scan_gen.load(Ordering::SeqCst));
            };
            let mut found = false;
            let mut stale = 0;
            for it in &snap {
                if !it.dead.load(Ordering::SeqCst) && it.poll() != 0 {
                    found = true;
                    break;
                }
                stale += 1;
            }
            if found {
                return READY_MASK;
            }
            let mut gone: Vec<Arc<EpItem>> = Vec::new();
            let settled = {
                let mut r = self.ready.lock();
                if gone.try_reserve_exact(stale).is_ok() {
                    for it in &snap[..stale] {
                        if it.state.load(Ordering::SeqCst) == READY
                            && let Some(pos) = r.iter().position(|x| Arc::ptr_eq(x, it))
                            && let Some(x) = r.remove(pos)
                        {
                            it.state.store(IDLE, Ordering::SeqCst);
                            gone.push(x);
                        }
                    }
                }
                if self.in_scan.load(Ordering::SeqCst) {
                    self.owed.store(true, Ordering::SeqCst);
                    true
                } else {
                    self.scan_gen.load(Ordering::SeqCst) == seen_gen
                }
            };
            drop(gone);
            drop(snap);
            if settled {
                return 0;
            }
            // A harvest started and ended during the look: the items it put back were not in the snapshot.
        }
        // Harvests one after another all the time: say ready (spurious at worst, never a lost wake-up).
        READY_MASK
    }

    /// Puts a scanned item back on the ready list (in front or behind). Never grows the list under its lock: an
    /// item taken out of the map but not yet off the list (a DEL finishing outside the mutex) can leave it full;
    /// then the item stays IDLE and the next harvest looks at every item (`lost`).
    fn requeue(&self, r: &mut VecDeque<Arc<EpItem>>, it: &Arc<EpItem>, front: bool) {
        if it.dead.load(Ordering::SeqCst) {
            it.state.store(IDLE, Ordering::SeqCst);
            return;
        }
        if r.len() >= r.capacity() {
            it.state.store(IDLE, Ordering::SeqCst);
            self.lost.store(true, Ordering::SeqCst);
            EPOLL_NO_ROOM.inc();
            return;
        }
        it.state.store(READY, Ordering::SeqCst);
        if front {
            r.push_front(it.clone());
        } else {
            r.push_back(it.clone());
        }
    }

    fn find(items: &[Arc<EpItem>], key: (usize, i32)) -> Result<usize, usize> {
        items.binary_search_by(|it| it.key.cmp(&key))
    }

    /// Looks again at the items of files without a `PollQueue` (their changes wake nobody): a ready
    /// level-triggered one is queued, an EPOLLET one only when bits appeared since the last look (its edge).
    fn queue_legacy(&self) {
        let items = self.items.lock();
        for it in items.iter().filter(|it| it.legacy && !it.dead.load(Ordering::SeqCst)) {
            let m = it.poll();
            let before = it.last.swap(m, Ordering::SeqCst);
            let et = it.events.load(Ordering::SeqCst) & EPOLLET != 0;
            if m != 0 && (!et || m & !before != 0) {
                self.queue(it, 0);
            }
        }
    }

    /// `ep_send_events` (the caller checked the arguments): reports up to `max` events into `sink`.
    fn send_events(&self, sink: &mut dyn EventSink, max: usize, interrupted: &dyn Fn() -> bool) -> Result<usize, Errno> {
        if interrupted() {
            return Err(EINTR);
        }
        let items = self.items.lock();
        // ep_start_scan: every queued item (all of them after a lost wake) moves to the scan list. Room for what
        // is queued now (all items after a lost wake), made outside the ready lock; items queued after this look
        // stay on the list behind the scanned ones, where Linux's ovflist would put them too.
        let queued = self.ready.lock().len();
        let lost_before = self.lost.load(Ordering::SeqCst);
        let room = if lost_before { queued + items.len() } else { queued } + 8;
        let mut tx: Vec<Arc<EpItem>> = Vec::new();
        if tx.try_reserve_exact(room).is_err() {
            return Err(ENOMEM);
        }
        let lost_room;
        {
            let mut r = self.ready.lock();
            self.in_scan.store(true, Ordering::SeqCst);
            self.scan_gen.fetch_add(1, Ordering::SeqCst);
            // A lost wake found after the room was measured waits for the next harvest (it stays set).
            let all = self.lost.load(Ordering::SeqCst) && tx.capacity() >= r.len() + items.len();
            if all {
                self.lost.store(false, Ordering::SeqCst);
            }
            lost_room = all;
            while tx.len() < tx.capacity() {
                let Some(it) = r.pop_front() else { break };
                it.state.store(SCAN, Ordering::SeqCst);
                it.rearm.store(false, Ordering::SeqCst);
                tx.push(it);
            }
            if all {
                for it in items.iter().filter(|it| it.state.load(Ordering::SeqCst) == IDLE) {
                    it.state.store(SCAN, Ordering::SeqCst);
                    it.rearm.store(false, Ordering::SeqCst);
                    tx.push(it.clone());
                }
            }
        }
        if lost_room {
            // Logged here, not in the callback (which may run in an interrupt handler).
            klog::event!(Warn, "vfs", "epoll.lost_room", items = items.len(); "a wake found no room on the ready list: every item is looked at");
        }
        let mut res = 0usize;
        let mut fault = false;
        let mut done = 0usize;
        // Per processed item: whether it goes back on the list after the scan (level-triggered) or leaves it.
        let mut keep: Vec<bool> = Vec::new();
        if keep.try_reserve_exact(tx.len()).is_err() {
            self.done_scan(&tx, 0, &[], false);
            drop(items);
            return Err(ENOMEM);
        }
        for it in &tx {
            if res >= max {
                break;
            }
            if it.dead.load(Ordering::SeqCst) {
                keep.push(false);
                done += 1;
                continue;
            }
            let revents = it.poll();
            if revents == 0 {
                keep.push(false);
                done += 1;
                continue;
            }
            if !sink.put(revents, it.data.load(Ordering::SeqCst)) {
                fault = true;
                break;
            }
            res += 1;
            done += 1;
            let ev = it.events.load(Ordering::SeqCst);
            if ev & EPOLLONESHOT != 0 {
                it.events.store(ev & PRIVATE, Ordering::SeqCst);
                keep.push(false);
            } else {
                keep.push(ev & EPOLLET == 0);
            }
        }
        self.done_scan(&tx, done, &keep, true);
        drop(items);
        // Items taken out meanwhile may hold the last references to their files: dropped with no lock held.
        drop(tx);
        if fault && res == 0 {
            return Err(EFAULT);
        }
        Ok(res)
    }

    /// `ep_done_scan`: unprocessed items (from `done` on) go back in front, items woken during the scan behind
    /// what callbacks queued meanwhile, level-triggered ones at the end.
    fn done_scan(&self, tx: &[Arc<EpItem>], done: usize, keep: &[bool], wake: bool) {
        let mut r = self.ready.lock();
        for it in tx[done..].iter().rev() {
            self.requeue(&mut r, it, true);
        }
        for (it, _) in tx[..done].iter().zip(keep).filter(|(it, k)| !**k && it.rearm.load(Ordering::SeqCst)) {
            self.requeue(&mut r, it, false);
        }
        for (it, k) in tx[..done].iter().zip(keep) {
            if it.state.load(Ordering::SeqCst) != SCAN {
                continue;
            }
            if *k {
                self.requeue(&mut r, it, false);
            } else {
                it.state.store(IDLE, Ordering::SeqCst);
            }
        }
        self.in_scan.store(false, Ordering::SeqCst);
        self.scan_gen.fetch_add(1, Ordering::SeqCst);
        let owed = self.owed.swap(false, Ordering::SeqCst);
        let more = !r.is_empty() || self.lost.load(Ordering::SeqCst);
        drop(r);
        if more {
            if wake {
                self.wq.wake_one();
            }
            if owed {
                // Someone looked at this epoll during the scan and was told "not ready" (`readiness`).
                self.pq.wake_nested(Events::IN, 1);
            }
        }
    }
}

/// The epoll object behind an epoll fd.
pub struct Epoll {
    inner: Arc<EpInner>,
}

/// One item taken out of an epoll under its mutex, detached and released after every lock (`detach`).
struct Removed(Arc<EpItem>);

impl Epoll {
    pub fn new() -> Arc<Epoll> {
        EPOLLS.inc();
        Arc::new(Epoll {
            inner: Arc::new(EpInner {
                items: Mutex::named("vfs.epoll", Vec::new()),
                ready: IrqSpinLock::named("vfs.epoll.ready", VecDeque::new()),
                lost: AtomicBool::new(false),
                wq: WaitQueue::new(),
                pq: PollQueue::new(),
                legacy: AtomicUsize::new(0),
                visit: AtomicU64::new(0),
                depth: AtomicU32::new(0),
                file: SpinLock::named("vfs.epoll.file", Weak::new()),
                in_scan: AtomicBool::new(false),
                scan_gen: AtomicU64::new(0),
                owed: AtomicBool::new(false),
            }),
        })
    }

    /// Items now (tests, diagnostics).
    pub fn len(&self) -> usize {
        self.inner.items.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `do_epoll_ctl` after the fds were looked up: `me` is the epoll's open file, `tf` the target's, `fd` its
    /// number; `event` = (events, data) for ADD and MOD; `root` = CAP_BLOCK_SUSPEND for EPOLLWAKEUP.
    pub fn ctl(me: &Arc<OpenFile>, op: i32, tf: &Arc<OpenFile>, fd: i32, event: (u32, u64), root: bool) -> Result<(), Errno> {
        // Files kept alive until every lock is let go (a last reference dropped under a lock could re-enter).
        let mut keep: Vec<Arc<OpenFile>> = Vec::new();
        let mut removed: Option<Removed> = None;
        let r = Self::ctl_locked(me, op, tf, fd, event, root, &mut keep, &mut removed);
        if let Some(Removed(it)) = removed {
            detach(&it, &mut keep);
        }
        drop(keep);
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn ctl_locked(me: &Arc<OpenFile>, op: i32, tf: &Arc<OpenFile>, fd: i32, event: (u32, u64), root: bool, keep: &mut Vec<Arc<OpenFile>>, removed: &mut Option<Removed>) -> Result<(), Errno> {
        if !tf.file.can_poll() {
            return Err(EPERM);
        }
        let has_event = op != EPOLL_CTL_DEL;
        let (mut events, data) = event;
        if has_event && events & EPOLLWAKEUP != 0 && !root {
            events &= !EPOLLWAKEUP;
        }
        let Some(ep) = me.file.as_epoll() else { return Err(EINVAL) };
        if Arc::ptr_eq(me, tf) {
            return Err(EINVAL);
        }
        let target_ep = tf.file.as_epoll();
        if has_event && events & EPOLLEXCLUSIVE != 0 {
            if op == EPOLL_CTL_MOD {
                return Err(EINVAL);
            }
            if op == EPOLL_CTL_ADD && (target_ep.is_some() || events & !EXCLUSIVE_OK != 0) {
                return Err(EINVAL);
            }
        }
        let inner = &ep.inner;
        // Nested topologies are checked under the global mutex (`epnested_mutex`), taken before `ep->mtx`. Linux
        // decides under `ep->mtx` (linux-6.6/fs/eventpoll.c:2246): an ADD into an epoll that is watched, or that
        // another ctl's loop check is walking right now (`ep->gen == loop_check_gen`), or of an epoll needs the
        // full check; the first look is made without the mutex and made again under it.
        let mut full_check = op == EPOLL_CTL_ADD && (!me.ep_links_empty() || target_ep.is_some());
        let mut check_files: Vec<Arc<OpenFile>> = Vec::new();
        let (_nest, mut items) = loop {
            // GenEnd first: the generation moves on before the mutex is let go (Linux `loop_check_gen++`).
            let nest = full_check.then(|| (GenEnd, NEST.lock()));
            if let Some(t) = target_ep
                && full_check
            {
                let visit = LOOP_GEN.fetch_add(1, Ordering::SeqCst) + 1;
                if loop_check(inner, &t.inner, visit, &mut check_files).is_err() {
                    klog::event!(Debug, "vfs", "epoll.loop_refused"; "an epoll inside another would close a loop or nest too deep: ELOOP");
                    keep.append(&mut check_files);
                    return Err(ELOOP);
                }
            }
            let items = inner.items.lock();
            if !full_check && op == EPOLL_CTL_ADD && (!me.ep_links_empty() || inner.visit.load(Ordering::SeqCst) == LOOP_GEN.load(Ordering::SeqCst)) {
                drop(items);
                full_check = true;
                continue;
            }
            break (nest, items);
        };
        let key = (Arc::as_ptr(tf) as usize, fd);
        let at = EpInner::find(&items, key);
        let r = match (op, at) {
            (EPOLL_CTL_ADD, Ok(_)) => Err(EEXIST),
            (EPOLL_CTL_ADD, Err(pos)) => {
                if full_check && target_ep.is_none() {
                    check_files.push(tf.clone());
                }
                Self::insert(inner, &mut items, pos, tf, key, (events | Events::ERR | Events::HUP, data), &check_files, keep, removed)
            }
            (EPOLL_CTL_DEL, Ok(pos)) => {
                let it = items.remove(pos);
                it.dead.store(true, Ordering::SeqCst);
                *removed = Some(Removed(it));
                Ok(())
            }
            (EPOLL_CTL_MOD, Ok(pos)) => {
                let it = items[pos].clone();
                if it.events.load(Ordering::SeqCst) & EPOLLEXCLUSIVE != 0 {
                    Err(EINVAL)
                } else {
                    Self::modify(inner, &it, (events | Events::ERR | Events::HUP, data));
                    Ok(())
                }
            }
            (EPOLL_CTL_DEL | EPOLL_CTL_MOD, Err(_)) => Err(ENOENT),
            _ => Err(EINVAL),
        };
        drop(items);
        keep.append(&mut check_files);
        r
    }

    /// `ep_insert` (under `ep->mtx`; `pos` is the key's place).
    #[allow(clippy::too_many_arguments)]
    fn insert(inner: &Arc<EpInner>, items: &mut Vec<Arc<EpItem>>, pos: usize, tf: &Arc<OpenFile>, key: (usize, i32), event: (u32, u64), check_files: &[Arc<OpenFile>], keep: &mut Vec<Arc<OpenFile>>, removed: &mut Option<Removed>) -> Result<(), Errno> {
        inner.reserve_ready(items.len() + 1)?;
        items.try_reserve(1).map_err(|_| ENOMEM)?;
        let mut queues = false;
        tf.file.poll_wait(&mut |_| queues = true);
        let legacy = !queues;
        if legacy {
            poll::note_legacy(&*tf.file);
        }
        let it = Arc::new_cyclic(|me| EpItem {
            ep: inner.clone(),
            me: me.clone(),
            key,
            file: tf.file.clone(),
            owner: Arc::downgrade(tf),
            id: poll::new_sub_id(),
            events: AtomicU32::new(event.0),
            data: AtomicU64::new(event.1),
            state: AtomicU8::new(IDLE),
            rearm: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            legacy,
            last: AtomicU32::new(0),
        });
        // attach_epitem: the open file names its items (close removes them).
        tf.link_ep(Arc::downgrade(&it))?;
        items.insert(pos, it.clone());
        EPOLL_ITEMS.inc();
        if legacy {
            inner.legacy.fetch_add(1, Ordering::SeqCst);
        }
        // reverse_path_check (with the new link in place, as Linux).
        if !check_files.is_empty() && reverse_path_check(check_files, keep).is_err() {
            klog::event!(Debug, "vfs", "epoll.paths_refused"; "too many or too deep wake-up paths through nested epolls: EINVAL");
            let it = items.remove(pos);
            it.dead.store(true, Ordering::SeqCst);
            *removed = Some(Removed(it));
            return Err(EINVAL);
        }
        // Hang on the file's queues, then look at it (no lost wake-up: poll module doc).
        let mut err = None;
        let w: Arc<dyn PollWaker> = it.clone();
        let exclusive = event.0 & EPOLLEXCLUSIVE != 0;
        tf.file.poll_wait(&mut |q| {
            let r = if exclusive { q.subscribe_exclusive(it.id, u32::MAX, w.clone()) } else { q.subscribe(it.id, u32::MAX, w.clone()) };
            if let Err(e) = r {
                err = Some(e);
            }
        });
        drop(w);
        if let Some(e) = err {
            let it = items.remove(pos);
            it.dead.store(true, Ordering::SeqCst);
            *removed = Some(Removed(it));
            return Err(e);
        }
        if it.poll() != 0 {
            inner.queue(&it, 0);
        }
        Ok(())
    }

    /// `ep_modify`: new events and data, then the item is queued if its file is ready for them now.
    fn modify(inner: &EpInner, it: &Arc<EpItem>, event: (u32, u64)) {
        it.events.store(event.0, Ordering::SeqCst);
        it.data.store(event.1, Ordering::SeqCst);
        core::sync::atomic::fence(Ordering::SeqCst);
        if it.poll() != 0 {
            inner.queue(it, 0);
        }
    }

    /// `ep_poll`: waits for events (`end`: None = no end, Some(0) = do not wait) and reports up to `max`;
    /// `interrupted()` (the caller is being ended) ends the wait with EINTR.
    pub fn wait(&self, sink: &mut dyn EventSink, max: usize, end: Option<u64>, interrupted: &dyn Fn() -> bool) -> Result<usize, Errno> {
        self.wait_sig(sink, max, end, interrupted, &|| false)
    }

    /// `ep_poll` with Linux's two checks: `fatal()` (`fatal_signal_pending`, the caller is being ended) ends
    /// even a harvest with EINTR; `signal()` (`signal_pending`: a signal waits for its handler) ends the wait
    /// with EINTR only when no event is there (events first, linux-6.6/fs/eventpoll.c:1898).
    pub fn wait_sig(&self, sink: &mut dyn EventSink, max: usize, end: Option<u64>, fatal: &dyn Fn() -> bool, signal: &dyn Fn() -> bool) -> Result<usize, Errno> {
        let interrupted = fatal;
        let inner = &self.inner;
        let mut timed_out = end == Some(0);
        let legacy = || inner.legacy.load(Ordering::SeqCst) > 0;
        if legacy() {
            inner.queue_legacy();
        }
        let mut eavail = inner.has_ready();
        loop {
            if eavail {
                let n = inner.send_events(sink, max, interrupted)?;
                if n > 0 {
                    return Ok(n);
                }
            }
            if timed_out {
                return Ok(0);
            }
            if interrupted() || signal() {
                return Err(EINTR);
            }
            let now = poll::now();
            if end.is_some_and(|e| now >= e) {
                timed_out = true;
            } else {
                let slice = if legacy() { poll::LEGACY_SLICE_NS } else { poll::KILL_SLICE_NS };
                let t = end.map_or(slice, |e| (e - now).min(slice));
                inner.wq.wait_until_timeout(|| inner.has_ready() || interrupted() || signal(), t);
                timed_out = end.is_some_and(|e| poll::now() >= e);
            }
            if legacy() {
                inner.queue_legacy();
            }
            eavail = inner.has_ready();
        }
    }

    /// The epoll's end (`ep_clear_and_put`): every item goes.
    fn clear(&self) {
        let items = core::mem::take(&mut *self.inner.items.lock());
        let mut keep = Vec::new();
        for it in &items {
            it.dead.store(true, Ordering::SeqCst);
        }
        // The ready list goes first, as a whole: each detach then finds its item idle (no search per item).
        let queued = {
            let mut r = self.inner.ready.lock();
            for it in r.iter() {
                it.state.store(IDLE, Ordering::SeqCst);
            }
            core::mem::take(&mut *r)
        };
        for it in &items {
            detach(it, &mut keep);
        }
        let rest = core::mem::take(&mut *self.inner.ready.lock());
        drop(queued);
        drop(rest);
        drop(items);
        drop(keep);
    }
}

/// Takes a dead item off its file's queues, the ready list and its open file's links (no lock held).
fn detach(it: &Arc<EpItem>, keep: &mut Vec<Arc<OpenFile>>) {
    let id = it.id;
    it.file.poll_wait(&mut |q| q.unsubscribe(id));
    let gone = it.ep.unqueue(it);
    drop(gone);
    if it.legacy {
        it.ep.legacy.fetch_sub(1, Ordering::SeqCst);
    }
    if let Some(of) = it.owner.upgrade() {
        of.unlink_ep(it);
        let _ = keep.try_reserve(1);
        keep.push(of);
    }
}

/// `eventpoll_release_file`: the open file description `links` belonged to is gone; its items leave their
/// epolls.
pub(crate) fn release_file(links: Vec<Weak<EpItem>>) {
    let mut keep = Vec::new();
    for w in links {
        let Some(it) = w.upgrade() else { continue };
        let removed = {
            let mut items = it.ep.items.lock();
            match EpInner::find(&items, it.key) {
                Ok(pos) if Arc::ptr_eq(&items[pos], &it) => {
                    let x = items.remove(pos);
                    x.dead.store(true, Ordering::SeqCst);
                    Some(x)
                }
                _ => None,
            }
        };
        if let Some(x) = removed {
            detach(&x, &mut keep);
        }
    }
    drop(keep);
}

impl Drop for Epoll {
    fn drop(&mut self) {
        self.clear();
    }
}

impl File for Epoll {
    /// linux-6.6/fs/eventpoll.c:904 `ep_show_fdinfo`: one line per item in key order, the watched file's position
    /// read unlocked, `sdev` as the kernel's dev_t (MAJOR << 20 | MINOR).
    fn show_fdinfo(&self, out: &mut alloc::string::String) {
        use core::fmt::Write;
        let items = self.inner.items.lock();
        for it in items.iter() {
            let (pos, ino, dev) = match it.owner.upgrade() {
                Some(of) => {
                    let a = of.stat();
                    (of.pos_racy(), a.ino, a.dev)
                }
                None => (0, 0, 0),
            };
            let sdev = (((dev >> 8) & 0xfff) << 20) | (dev & 0xff) | ((dev >> 12) & 0xfff00);
            let _ = write!(
                out,
                "tfd: {:8} events: {:8x} data: {:16x}  pos:{} ino:{:x} sdev:{:x}\n",
                it.key.1,
                it.events.load(Ordering::Relaxed),
                it.data.load(Ordering::Relaxed),
                pos as i64,
                ino,
                sdev
            );
        }
    }
    /// No read operation: `vfs_read` answers EINVAL.
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    /// Anonymous inodes have no type bits.
    fn mode(&self) -> u32 {
        0o600
    }
    /// `noop_llseek`.
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Ok(0))
    }
    fn events(&self) -> Events {
        Events(self.inner.readiness())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.inner.pq)
    }
    fn can_poll(&self) -> bool {
        true
    }
    /// `ep_eventpoll_poll` reports EPOLLIN|EPOLLRDNORM itself.
    fn poll_exact(&self) -> bool {
        true
    }
    fn attr(&self) -> Option<crate::abi::Attr> {
        Some(crate::eventfd::anon_attr())
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:[eventpoll]"))
    }
    fn as_epoll(&self) -> Option<&Epoll> {
        Some(self)
    }
}

/// epoll_create1's open file (O_RDWR, as `anon_inode_getfile(.., O_RDWR | (flags & O_CLOEXEC))`).
pub fn new_epoll() -> Arc<OpenFile> {
    let ep = Epoll::new();
    let inner = ep.inner.clone();
    let of = OpenFile::anon(ep, crate::abi::O_RDWR);
    *inner.file.lock() = Arc::downgrade(&of);
    of
}
