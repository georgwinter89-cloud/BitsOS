//! An inotify instance (Linux's fsnotify group with `inotify_fops`): the event queue, read, poll and FIONREAD,
//! and the watch table by wd.
//!
//! - **Queue** (`fsnotify_insert_event`): an event equal to the last one queued (mask, wd, name; never after an
//!   IN_IGNORED) is merged; with `max_queued_events` queued the one IN_Q_OVERFLOW event (wd -1) goes in instead,
//!   once until it is read. A lost event for want of memory is an overflow too.
//! - **read** (`inotify_read`): whole events only, `struct inotify_event` with the name padded with zeros to a
//!   multiple of 16 bytes (name length + 1 rounded up); EINVAL when the first event does not fit; waits for one
//!   unless O_NONBLOCK (EAGAIN); a signal ends the wait (EINTR, restarted per SA_RESTART); an event whose copy
//!   faults is lost (EFAULT, or the bytes so far).
//! - **wd** (`idr_alloc_cyclic(.., 1, 0)`): the next free number after the last one handed out, from 1.

use super::{sys, Watch, ACTIVE, EVENT_SIZE, IN_IGNORED, IN_Q_OVERFLOW, OVERFLOWS};
use crate::abi::{self, Attr};
use crate::inode::Inode;
use crate::node::{File, IoSink};
use crate::poll::{Events, PollQueue, Pollable};
use crate::Errno;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;

const SLICE_NS: u64 = 50_000_000;

struct Event {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Option<Box<[u8]>>,
}

impl Event {
    /// The name's room in the record: length + 1 rounded up to 16 (`round_event_name_len`), 0 without one.
    fn pad_len(&self) -> usize {
        self.name.as_ref().map_or(0, |n| (n.len() + 1).div_ceil(EVENT_SIZE) * EVENT_SIZE)
    }
    fn size(&self) -> usize {
        EVENT_SIZE + self.pad_len()
    }
    fn same(&self, wd: i32, mask: u32, name: Option<&[u8]>) -> bool {
        self.mask & IN_IGNORED == 0 && self.mask == mask && self.wd == wd && self.name.as_deref() == name
    }
}

struct Queue {
    ev: VecDeque<Event>,
    overflow_queued: bool,
}

struct Table {
    by_wd: BTreeMap<i32, (Arc<Watch>, Arc<Inode>)>,
    last_wd: i32,
}

pub struct Group {
    q: SpinLock<Queue>,
    wq: PollQueue,
    table: SpinLock<Table>,
    /// Whose instance it is (the per-user limits, `ucounts`).
    pub(crate) uid: u32,
    /// `max_queued_events` when it was made.
    max_events: usize,
    /// Closed: queues nothing more (`group->shutdown`).
    shutdown: AtomicBool,
    /// Itself, for the watches' weak links.
    me: Weak<Group>,
}

impl Group {
    pub(crate) fn new(uid: u32, max_events: usize) -> Arc<Group> {
        Arc::new_cyclic(|me| Group {
            me: me.clone(),
            q: SpinLock::named("vfs.inotify.queue", Queue { ev: VecDeque::new(), overflow_queued: false }),
            wq: PollQueue::new(),
            table: SpinLock::named("vfs.inotify.watches", Table { by_wd: BTreeMap::new(), last_wd: 0 }),
            uid,
            max_events,
            shutdown: AtomicBool::new(false),
        })
    }

    /// Queues one event and wakes a reader (`fsnotify_insert_event` with `inotify_merge`).
    pub(crate) fn queue(&self, wd: i32, mask: u32, cookie: u32, name: Option<&[u8]>) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        {
            let mut q = self.q.lock();
            if q.ev.len() >= self.max_events {
                if q.overflow_queued {
                    return;
                }
                if !self.push_overflow(&mut q) {
                    return;
                }
            } else if q.ev.back().is_some_and(|l| l.same(wd, mask, name)) {
                return;
            } else {
                let name = match name {
                    Some(n) => {
                        let mut v = Vec::new();
                        if v.try_reserve_exact(n.len()).is_err() {
                            self.push_overflow(&mut q);
                            drop(q);
                            self.wq.wake_one();
                            return;
                        }
                        v.extend_from_slice(n);
                        Some(v.into_boxed_slice())
                    }
                    None => None,
                };
                if q.ev.try_reserve(1).is_err() {
                    self.push_overflow(&mut q);
                } else {
                    q.ev.push_back(Event { wd, mask, cookie, name });
                }
            }
        }
        self.wq.wake_one();
    }

    /// The one overflow event (if not queued already and if there is room for its record).
    fn push_overflow(&self, q: &mut Queue) -> bool {
        if q.overflow_queued || q.ev.try_reserve(1).is_err() {
            return false;
        }
        OVERFLOWS.inc();
        q.ev.push_back(Event { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: None });
        q.overflow_queued = true;
        true
    }

    /// Bytes of every queued record (FIONREAD).
    pub fn queued_bytes(&self) -> usize {
        self.q.lock().ev.iter().map(Event::size).sum()
    }
    pub fn queued(&self) -> usize {
        self.q.lock().ev.len()
    }

    /// The first event if its record fits into `room` (Err(EINVAL) if not), taken off the queue.
    fn take(&self, room: usize) -> Result<Option<Event>, Errno> {
        let mut q = self.q.lock();
        let Some(first) = q.ev.front() else { return Ok(None) };
        if first.size() > room {
            return Err(EINVAL);
        }
        let e = q.ev.pop_front();
        if e.as_ref().is_some_and(|e| e.mask == IN_Q_OVERFLOW && e.wd == -1) {
            q.overflow_queued = false;
        }
        Ok(e)
    }

    fn record(e: &Event) -> Vec<u8> {
        let mut b = Vec::with_capacity(e.size());
        b.extend_from_slice(&e.wd.to_le_bytes());
        b.extend_from_slice(&e.mask.to_le_bytes());
        b.extend_from_slice(&e.cookie.to_le_bytes());
        b.extend_from_slice(&(e.pad_len() as u32).to_le_bytes());
        if let Some(n) = &e.name {
            b.extend_from_slice(n);
            b.resize(e.size(), 0);
        }
        b
    }

    /// `inotify_read` into `to`.
    fn read_into(&self, to: &mut dyn IoSink, nonblock: bool) -> Result<usize, Errno> {
        let mut done = 0usize;
        loop {
            match self.take(to.left()) {
                Ok(Some(e)) => {
                    let rec = Self::record(&e);
                    if to.copy(&rec) < rec.len() {
                        return if done > 0 { Ok(done) } else { Err(EFAULT) };
                    }
                    done += rec.len();
                    continue;
                }
                Err(e) => return if done > 0 { Ok(done) } else { Err(e) },
                Ok(None) => {}
            }
            if done > 0 {
                return Ok(done);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if crate::dentry::interrupted() {
                return Err(EINTR);
            }
            self.wq.wait_until_timeout(&|| self.queued() > 0, SLICE_NS);
        }
    }

    /// The watch this group has on `inode`, if any.
    pub(crate) fn watch_on(&self, inode: &Inode) -> Option<Arc<Watch>> {
        inode.marks.of_group(self)
    }

    /// `inotify_update_watch`: a new watch on `inode` with `mask` (event bits and flags), or the existing one's
    /// mask replaced (or ORed with IN_MASK_ADD; EEXIST with IN_MASK_CREATE). Returns the wd.
    pub fn update_watch(&self, inode: &Arc<Inode>, mask: u32) -> Result<i32, Errno> {
        use super::{IN_ALL_EVENTS, IN_EXCL_UNLINK, IN_MASK_ADD, IN_MASK_CREATE, IN_ONESHOT};
        let bits = mask & (IN_ALL_EVENTS | IN_ONESHOT | IN_EXCL_UNLINK);
        let mut t = self.table.lock();
        if let Some(w) = self.watch_on(inode).filter(|w| !w.dead.load(Ordering::Acquire)) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(EEXIST);
            }
            let old = w.mask();
            w.mask.store(if mask & IN_MASK_ADD != 0 { old | bits } else { bits }, Ordering::Release);
            return Ok(w.wd);
        }
        sys::take_watch(self.uid)?;
        let Some(wd) = Self::next_wd(&t) else {
            sys::give_watch(self.uid);
            return Err(ENOSPC);
        };
        let w = Arc::new(Watch { wd, mask: AtomicU32::new(bits), group: self.me.clone(), dead: AtomicBool::new(false) });
        if let Err(e) = inode.marks.add(w.clone()) {
            sys::give_watch(self.uid);
            return Err(e);
        }
        ACTIVE.fetch_add(1, Ordering::Relaxed);
        t.by_wd.insert(wd, (w, inode.clone()));
        t.last_wd = wd;
        Ok(wd)
    }

    /// The next free wd after the last one handed out (1..=i32::MAX, wrapping to 1).
    fn next_wd(t: &Table) -> Option<i32> {
        let mut wd = t.last_wd;
        for _ in 0..=t.by_wd.len() {
            wd = if wd == i32::MAX { 1 } else { wd + 1 };
            if !t.by_wd.contains_key(&wd) {
                return Some(wd);
            }
        }
        None
    }

    /// inotify_rm_watch: EINVAL for a wd it does not have.
    pub(crate) fn remove_watch(&self, wd: i32) -> Result<(), Errno> {
        let (w, inode) = self.table.lock().by_wd.get(&wd).cloned().ok_or(EINVAL)?;
        super::end(&w, &inode);
        Ok(())
    }

    /// Takes `wd` out of the table (an ended watch): the entry, holding the inode's pin, for the caller to drop
    /// after every lock. The user's watch count goes back here.
    pub(crate) fn forget(&self, wd: i32) -> Option<(Arc<Watch>, Arc<Inode>)> {
        let e = self.table.lock().by_wd.remove(&wd);
        if e.is_some() {
            sys::give_watch(self.uid);
        }
        e
    }

    /// Watches alive (tests, fdinfo).
    pub fn watches(&self) -> usize {
        self.table.lock().by_wd.len()
    }
}

impl Drop for Group {
    /// `fsnotify_destroy_group`: every watch ends without an event (nobody reads them), the instance count
    /// goes back.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let t = core::mem::take(&mut self.table.get_mut().by_wd);
        for (_, (w, inode)) in t {
            if !w.dead.swap(true, Ordering::AcqRel) {
                inode.marks.remove(&w);
                ACTIVE.fetch_sub(1, Ordering::Relaxed);
                sys::give_watch(self.uid);
            }
        }
        sys::give_instance(self.uid);
        self.wq.wake_poll(Events::HUP);
    }
}

impl File for Group {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, true)
    }
    /// No write method: EINVAL (`FMODE_CAN_WRITE` is not set).
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.read_into(&mut crate::node::BufSink::new(buf), nonblock)
    }
    fn read_iter(&self, to: &mut dyn IoSink, status: u64) -> Option<Result<usize, Errno>> {
        Some(self.read_into(to, status & abi::O_NONBLOCK != 0))
    }
    fn write_stream(&self, _buf: &[u8], _nonblock: bool) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    /// `inotify_ioctl`: FIONREAD (an int); anything else ENOTTY.
    fn ioctl(&self, cmd: u64, _arg: u64) -> Result<Vec<u8>, Errno> {
        match cmd {
            uapi::ioctl::FIONREAD => Ok((self.queued_bytes() as i32).to_le_bytes().to_vec()),
            _ => Err(ENOTTY),
        }
    }
    fn mode(&self) -> u32 {
        0o600
    }
    /// `inotify_poll`: EPOLLIN | EPOLLRDNORM while an event waits.
    fn events(&self) -> Events {
        Events(if self.queued() > 0 { Events::IN | Events::RDNORM } else { 0 })
    }
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Ok(0))
    }
    fn attr(&self) -> Option<Attr> {
        Some(crate::eventfd::anon_attr())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.wq)
    }
    fn can_poll(&self) -> bool {
        true
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:inotify"))
    }
    fn as_inotify(&self) -> Option<&Group> {
        Some(self)
    }
}

impl Pollable for Group {
    fn events(&self) -> Events {
        File::events(self)
    }
    fn wait_queue(&self) -> &WaitQueue {
        &self.wq
    }
}
