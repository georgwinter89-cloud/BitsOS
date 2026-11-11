//! Message queues (linux-7.3.1/ipc/msg.c). The text of a message is one vector; Linux's segments only avoid large
//! allocations. A queue's bytes are bounded by `q_qbytes` and its messages too (`msg_fits_inqueue`), as on Linux.

use super::ids::{GetOps, Object, Perm, Table, ipcget};
use super::{Caller, sleep_once};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::ab::siginfo::ERESTARTNOHAND;
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::Errno;

/// `struct msg_msg`.
pub struct Msg {
    pub mtype: i64,
    pub text: Vec<u8>,
}

/// What a sleeping receiver finds (`msr->r_msg`).
enum Slot {
    Waiting,
    Failed(Errno),
    Got(Msg),
}

/// `struct msg_receiver`.
struct Receiver {
    mode: Mode,
    mtype: i64,
    maxsize: u64,
    tid: u32,
    slot: SpinLock<Slot>,
    /// `expunge_all(-EAGAIN)`: look at the queue again.
    again: AtomicBool,
    wq: WaitQueue,
}

impl Receiver {
    fn set(&self, s: Slot) {
        *self.slot.lock() = s;
        self.wq.wake_all();
    }
    fn waiting(&self) -> bool {
        matches!(*self.slot.lock(), Slot::Waiting)
    }
}

/// `struct msg_sender`.
struct Sender {
    size: u64,
    woken: AtomicBool,
    wq: WaitQueue,
}

impl Sender {
    fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        self.wq.wake_all();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Any,
    Equal,
    NotEqual,
    LessEqual,
    Number,
}

/// `testmsg`.
fn matches(m: &Msg, mtype: i64, mode: Mode) -> bool {
    match mode {
        Mode::Any | Mode::Number => true,
        Mode::LessEqual => m.mtype <= mtype,
        Mode::Equal => m.mtype == mtype,
        Mode::NotEqual => m.mtype != mtype,
    }
}

pub(super) struct QueueInner {
    pub perm: Perm,
    pub stime: i64,
    pub rtime: i64,
    pub ctime: i64,
    pub cbytes: u64,
    pub qbytes: u64,
    pub lspid: u32,
    pub lrpid: u32,
    pub messages: VecDeque<Msg>,
    receivers: VecDeque<Arc<Receiver>>,
    senders: VecDeque<Arc<Sender>>,
}

impl QueueInner {
    /// `msg_fits_inqueue`.
    fn fits(&self, size: u64) -> bool {
        size.saturating_add(self.cbytes) <= self.qbytes && 1 + self.messages.len() as u64 <= self.qbytes
    }
    /// `ss_wakeup`: senders that fit now are woken in order; one that does not goes to the end, and the walk stops
    /// when it comes round.
    pub(super) fn wake_senders(&mut self) {
        let (mut i, mut stop): (usize, Option<Arc<Sender>>) = (0, None);
        while let Some(s) = self.senders.get(i).cloned() {
            if stop.as_ref().is_some_and(|t| Arc::ptr_eq(t, &s)) {
                break;
            }
            if !self.fits(s.size) {
                stop.get_or_insert_with(|| s.clone());
                self.senders.remove(i);
                self.senders.push_back(s);
                continue;
            }
            s.wake();
            i += 1;
        }
    }
    /// `expunge_all(-EAGAIN)` (IPC_SET): the receivers look again.
    pub(super) fn wake_receivers(&mut self) {
        for r in &self.receivers {
            r.again.store(true, Ordering::Release);
            r.wq.wake_all();
        }
    }
    /// `freeque`'s wake-ups: EIDRM for all who sleep. Bytes and counts leave the namespace's sums.
    pub(super) fn free(&mut self, totals: &Totals) {
        for r in self.receivers.drain(..) {
            r.set(Slot::Failed(EIDRM));
        }
        for s in self.senders.drain(..) {
            s.wake();
        }
        totals.hdrs.fetch_sub(self.messages.len() as u64, Ordering::Relaxed);
        totals.bytes.fetch_sub(self.cbytes, Ordering::Relaxed);
    }
    /// `pipelined_send`: a waiting receiver takes the message directly; true when one did. `m` stays with the caller
    /// otherwise. A receiver whose buffer is too small gets E2BIG and leaves.
    fn pipelined(&mut self, m: &mut Option<Msg>) -> bool {
        let mut i = 0;
        while let Some(r) = self.receivers.get(i).cloned() {
            let Some(msg) = m.as_ref() else { return false };
            if !matches(msg, r.mtype, r.mode) {
                i += 1;
                continue;
            }
            self.receivers.remove(i);
            if r.maxsize < msg.text.len() as u64 {
                r.set(Slot::Failed(E2BIG));
                continue;
            }
            self.lrpid = r.tid;
            self.rtime = super::now_secs();
            if let Some(msg) = m.take() {
                r.set(Slot::Got(msg));
            }
            return true;
        }
        false
    }
    /// `find_msg`: the index of the message a receiver of this mode takes.
    fn find(&self, mtype: i64, mode: Mode) -> Option<usize> {
        let (mut want, mut found, mut count) = (mtype, None, 0i64);
        for (i, m) in self.messages.iter().enumerate() {
            if !matches(m, want, mode) {
                continue;
            }
            match mode {
                Mode::LessEqual if m.mtype != 1 => {
                    want = m.mtype - 1;
                    found = Some(i);
                }
                Mode::Number => {
                    if want == count {
                        return Some(i);
                    }
                }
                _ => return Some(i),
            }
            count += 1;
        }
        found
    }
}

/// `struct msg_queue`.
pub struct MsgQueue {
    pub(super) inner: SpinLock<QueueInner>,
}

impl Object for MsgQueue {
    fn with_perm<R>(&self, f: impl FnOnce(&mut Perm) -> R) -> R {
        f(&mut self.inner.lock().perm)
    }
}

/// `percpu_msg_hdrs` and `percpu_msg_bytes`.
pub struct Totals {
    pub hdrs: AtomicU64,
    pub bytes: AtomicU64,
}

pub struct MsgTable {
    pub ids: Table<MsgQueue>,
    pub totals: Totals,
}

impl MsgTable {
    pub(super) const fn new() -> MsgTable {
        MsgTable { ids: Table::named("ipc.ids.msg"), totals: Totals { hdrs: AtomicU64::new(0), bytes: AtomicU64::new(0) } }
    }
}

/// msgget (`newque`, msg.c:86).
pub fn msgget(c: &Caller<'_>, key: i32, flg: i32) -> Result<i32, Errno> {
    let l = c.limits();
    let ops = GetOps {
        more_checks: &|_: &MsgQueue| Ok(()),
        pre: &|_| Ok(()),
        build: &|| {
            let inner = QueueInner {
                perm: Perm::new(key, flg, &c.cred),
                stime: 0,
                rtime: 0,
                ctime: super::now_secs(),
                cbytes: 0,
                qbytes: l.msgmnb.load(Ordering::Relaxed) as i64 as u64,
                lspid: 0,
                lrpid: 0,
                messages: VecDeque::new(),
                receivers: VecDeque::new(),
                senders: VecDeque::new(),
            };
            Ok(Arc::new(MsgQueue { inner: SpinLock::named("ipc.msg", inner) }))
        },
        used: 0,
        limit: l.msgmni.load(Ordering::Relaxed),
    };
    ipcget(&c.ns.tables.msg.ids, c, key, flg, &ops)
}

/// The checks of `do_msgsnd` before it reads the text (msg.c:612): the caller reads `size` bytes after them.
pub fn snd_check(c: &Caller<'_>, id: i32, mtype: i64, size: u64) -> Result<(), Errno> {
    if size > c.limits().msgmax.load(Ordering::Relaxed) as i64 as u64 || (size as i64) < 0 || id < 0 || mtype < 1 {
        return Err(EINVAL);
    }
    Ok(())
}

/// `do_msgsnd` from the message on. Sleeps with no lock held; a signal ends the wait with ERESTARTNOHAND.
pub fn msgsnd(c: &Caller<'_>, id: i32, msg: Msg, flg: i32) -> Result<(), Errno> {
    let table = &c.ns.tables.msg;
    let q = table.ids.read().obtain_check(id)?;
    let size = msg.text.len() as u64;
    let mut msg = Some(msg);
    let mut g = q.inner.lock();
    loop {
        if !g.perm.permits(&c.ns, &c.cred, S_IWUGO) {
            return Err(EACCES);
        }
        if g.perm.deleted {
            return Err(EIDRM);
        }
        if g.fits(size) {
            break;
        }
        if flg & IPC_NOWAIT != 0 {
            return Err(EAGAIN);
        }
        g.senders.try_reserve(1).map_err(super::nomem)?;
        let s = Arc::new(Sender { size, woken: AtomicBool::new(false), wq: WaitQueue::new() });
        g.senders.push_back(s.clone());
        drop(g);
        while !s.woken.load(Ordering::Acquire) && !c.interrupted() {
            sleep_once(&s.wq, c, None, &|| s.woken.load(Ordering::Acquire));
        }
        g = q.inner.lock();
        if g.perm.deleted {
            return Err(EIDRM);
        }
        if let Some(i) = g.senders.iter().position(|x| Arc::ptr_eq(x, &s)) {
            g.senders.remove(i);
        }
        if c.interrupted() {
            return Err(ERESTARTNOHAND);
        }
    }
    g.messages.try_reserve(1).map_err(super::nomem)?;
    g.lspid = c.tgid();
    g.stime = super::now_secs();
    if !g.pipelined(&mut msg)
        && let Some(m) = msg
    {
        g.cbytes += size;
        g.messages.push_back(m);
        table.totals.bytes.fetch_add(size, Ordering::Relaxed);
        table.totals.hdrs.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// The arguments of msgrcv after `convert_mode`.
pub struct RcvArgs {
    pub id: i32,
    pub bufsz: u64,
    pub mtype: i64,
    pub flg: i32,
}

fn convert_mode(mtype: &mut i64, flg: i32) -> Mode {
    if flg & MSG_COPY != 0 {
        return Mode::Number;
    }
    if *mtype == 0 {
        return Mode::Any;
    }
    if *mtype < 0 {
        *mtype = mtype.checked_neg().unwrap_or(i64::MAX);
        return Mode::LessEqual;
    }
    if flg & MSG_EXCEPT != 0 { Mode::NotEqual } else { Mode::Equal }
}

/// `do_msgrcv` after its first checks. `copy_len`: MSG_COPY's `min(bufsz, msgmax)`: the found message is cloned, not
/// taken, and may not be longer (EINVAL, `copy_msg`). Answers the message; the glue writes it with no lock held.
pub fn msgrcv(c: &Caller<'_>, a: &RcvArgs, copy_len: Option<u64>) -> Result<Msg, Errno> {
    let table = &c.ns.tables.msg;
    let (flg, bufsz) = (a.flg, a.bufsz);
    let mut mtype = a.mtype;
    let mode = convert_mode(&mut mtype, flg);
    let q = table.ids.read().obtain_check(a.id)?;
    loop {
        let mut g = q.inner.lock();
        if !g.perm.permits(&c.ns, &c.cred, S_IRUGO) {
            return Err(EACCES);
        }
        if g.perm.deleted {
            return Err(EIDRM);
        }
        if let Some(i) = g.find(mtype, mode) {
            let len = g.messages[i].text.len() as u64;
            if bufsz < len && flg & MSG_NOERROR == 0 {
                return Err(E2BIG);
            }
            if let Some(room) = copy_len {
                if len > room {
                    return Err(EINVAL);
                }
                let mut text = Vec::new();
                text.try_reserve_exact(len as usize).map_err(super::nomem)?;
                text.extend_from_slice(&g.messages[i].text);
                return Ok(Msg { mtype: g.messages[i].mtype, text });
            }
            let Some(m) = g.messages.remove(i) else { return Err(EIDRM) };
            g.rtime = super::now_secs();
            g.lrpid = c.tgid();
            g.cbytes -= len;
            table.totals.bytes.fetch_sub(len, Ordering::Relaxed);
            table.totals.hdrs.fetch_sub(1, Ordering::Relaxed);
            g.wake_senders();
            return Ok(m);
        }
        if flg & IPC_NOWAIT != 0 {
            return Err(ENOMSG);
        }
        g.receivers.try_reserve(1).map_err(super::nomem)?;
        let maxsize = if flg & MSG_NOERROR != 0 { i32::MAX as u64 } else { bufsz };
        let r = Arc::new(Receiver { mode, mtype, maxsize, tid: c.tid, slot: SpinLock::named("ipc.msg.slot", Slot::Waiting), again: AtomicBool::new(false), wq: WaitQueue::new() });
        g.receivers.push_back(r.clone());
        drop(g);
        let done = || !r.waiting() || r.again.load(Ordering::Acquire);
        while !done() && !c.interrupted() {
            sleep_once(&r.wq, c, None, &done);
        }
        let mut g = q.inner.lock();
        match core::mem::replace(&mut *r.slot.lock(), Slot::Waiting) {
            Slot::Got(m) => return Ok(m),
            Slot::Failed(e) => return Err(e),
            Slot::Waiting => {}
        }
        if let Some(i) = g.receivers.iter().position(|x| Arc::ptr_eq(x, &r)) {
            g.receivers.remove(i);
        }
        if c.interrupted() {
            return Err(ERESTARTNOHAND);
        }
    }
}

impl Drop for MsgTable {
    /// `msg_exit_ns`: nobody sleeps here (a sleeper's process holds the namespace).
    fn drop(&mut self) {
        let g = self.ids.read();
        for q in g.iter() {
            let mut i = q.inner.lock();
            i.perm.deleted = true;
            i.messages.clear();
        }
    }
}
