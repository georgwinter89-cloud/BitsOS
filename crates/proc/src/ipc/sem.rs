//! Semaphore arrays and semop (linux-7.3.1/ipc/sem.c). Linux's fine-grained per-semaphore locks are a performance
//! matter; here one spin lock guards an array, and the queues stay exactly Linux's (per semaphore, global, merged
//! while a complex operation waits), which is what decides who is woken first.
//!
//! A waiter's `Queue` is shared with the array: a waker stores the result in `status` under the lock and wakes the
//! queue's own wait queue; the waiter reads `status` without the lock (Linux's `q->status`).

use super::ids::{GetOps, Object, Perm, Table, ipcget};
use super::sem_undo::{UndoList, UndoSlot};
use super::{Caller, sleep_once};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::Errno;

klog::counter!(SEMOPS, "ipc.sem.ops");
klog::counter!(SLEEPS, "ipc.sem.sleeps");

/// `struct sembuf`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemBuf {
    pub num: u16,
    pub op: i16,
    pub flg: i16,
}

/// `q->status` while the operation is not done (-EINTR in Linux).
const PENDING: i32 = i32::MIN;

/// `struct sem_queue`.
pub(super) struct Queue {
    pub sops: Vec<SemBuf>,
    pub undo: Option<u64>,
    pub pid: u32,
    pub alter: bool,
    pub dupsop: bool,
    /// Index of the operation it waits at (`q->blocking`, GETNCNT and GETZCNT read it); set under the lock.
    pub blocking: AtomicUsize,
    status: AtomicI32,
    wq: WaitQueue,
}

impl Queue {
    /// `wake_up_sem_queue_prepare` and the wake together: `err` is 0 or a negative errno.
    pub fn finish(&self, err: i32) {
        self.status.store(err, Ordering::Release);
        self.wq.wake_all();
    }
    fn status(&self) -> i32 {
        self.status.load(Ordering::Acquire)
    }
}

/// `struct sem`.
pub(super) struct Sem {
    pub val: i32,
    pub pid: u32,
    pub otime: i64,
    pub alter: Vec<Arc<Queue>>,
    pub zero: Vec<Arc<Queue>>,
}

pub(super) struct SemInner {
    pub perm: Perm,
    pub ctime: i64,
    pub sems: Vec<Sem>,
    pub alter: Vec<Arc<Queue>>,
    pub zero: Vec<Arc<Queue>>,
    pub complex: usize,
    /// One entry per undo list that touched this array: (`UndoList::id`, semadj per semaphore). Linux's `list_id`.
    pub undo: Vec<(u64, Vec<i16>)>,
}

/// `struct sem_array`.
pub struct SemArray {
    pub nsems: usize,
    pub(super) inner: SpinLock<SemInner>,
}

impl Object for SemArray {
    fn with_perm<R>(&self, f: impl FnOnce(&mut Perm) -> R) -> R {
        f(&mut self.inner.lock().perm)
    }
}

impl SemArray {
    /// Whether the array is still there (`ipc_valid_object`).
    pub(super) fn alive(&self) -> bool {
        !self.inner.lock().perm.deleted
    }
}

/// The namespace's semaphores; `Ids::used` is `used_sems`.
pub struct SemTable {
    pub ids: Table<SemArray>,
}

impl SemTable {
    pub(super) const fn new() -> SemTable {
        SemTable { ids: Table::named("ipc.ids.sem") }
    }
}

impl Drop for SemTable {
    /// `sem_exit_ns`: an undo list may still name an array; it finds it gone.
    fn drop(&mut self) {
        let g = self.ids.read();
        for a in g.iter() {
            let dead = {
                let mut i = a.inner.lock();
                i.perm.deleted = true;
                i.free_queues();
                core::mem::take(&mut i.sems)
            };
            drop(dead);
        }
    }
}

/// `newary` up to `ipc_addid`.
fn new_array(key: i32, nsems: usize, semflg: i32, c: &Caller<'_>) -> Result<Arc<SemArray>, Errno> {
    let mut sems = Vec::new();
    sems.try_reserve_exact(nsems).map_err(super::nomem)?;
    sems.resize_with(nsems, || Sem { val: 0, pid: 0, otime: 0, alter: Vec::new(), zero: Vec::new() });
    let inner = SemInner { perm: Perm::new(key, semflg, &c.cred), ctime: super::now_secs(), sems, alter: Vec::new(), zero: Vec::new(), complex: 0, undo: Vec::new() };
    Ok(Arc::new(SemArray { nsems, inner: SpinLock::named("ipc.sem", inner) }))
}

/// semget (`ksys_semget`, sem.c:607).
pub fn semget(c: &Caller<'_>, key: i32, nsems: i32, semflg: i32) -> Result<i32, Errno> {
    let l = c.limits();
    if nsems < 0 || nsems > l.sem_at(SC_SEMMSL) {
        return Err(EINVAL);
    }
    let ops = GetOps {
        more_checks: &|a: &SemArray| if nsems as usize > a.nsems { Err(EINVAL) } else { Ok(()) },
        pre: &|g| {
            if nsems == 0 {
                Err(EINVAL)
            } else if g.used + nsems as u64 > l.sem_at(SC_SEMMNS).max(0) as u64 {
                Err(ENOSPC)
            } else {
                Ok(())
            }
        },
        build: &|| new_array(key, nsems as usize, semflg, c),
        used: nsems as u64,
        limit: l.sem_at(SC_SEMMNI),
    };
    ipcget(&c.ns.tables.sem.ids, c, key, semflg, &ops)
}

/// What one list of the arguments says (the scan at the top of `__do_semtimedop`, sem.c:2015).
struct OpsInfo {
    max: u16,
    undos: bool,
    alter: bool,
    dupsop: bool,
}

fn scan(sops: &[SemBuf]) -> OpsInfo {
    let (mut dup, mut i) = (0u64, OpsInfo { max: 0, undos: false, alter: false, dupsop: false });
    for s in sops {
        let mask = 1u64 << (s.num % 64);
        i.max = i.max.max(s.num);
        i.undos |= s.flg & SEM_UNDO != 0;
        i.dupsop |= dup & mask != 0;
        if s.op != 0 {
            i.alter = true;
            dup |= mask;
        }
    }
    i
}

/// `__do_semtimedop` (sem.c:1988) after the user copies, with `timeout` already checked: Ok or a positive errno.
/// Blocks with no lock held. A signal ends the wait with EINTR, never a restart (sem.c:2228).
pub fn semtimedop(c: &Caller<'_>, slot: &UndoSlot, semid: i32, sops: Vec<SemBuf>, timeout_ns: Option<u64>) -> Result<(), Errno> {
    let nsops = sops.len();
    if nsops < 1 || semid < 0 {
        return Err(EINVAL);
    }
    if nsops > c.limits().sem_at(SC_SEMOPM).max(0) as usize {
        return Err(E2BIG);
    }
    let end = timeout_ns.map(|t| time::now_ns().saturating_add(t));
    SEMOPS.inc();
    let info = scan(&sops);
    let table = &c.ns.tables.sem;
    let list: Option<Arc<UndoList>> = if info.undos { Some(super::sem_undo::find_alloc(c, slot, semid)?) } else { None };
    let a = table.ids.read().obtain_check(semid)?;
    if info.max as usize >= a.nsems {
        return Err(EFBIG);
    }
    let undo = list.as_ref().map(|l| l.id);
    let mut g = a.inner.lock();
    if !g.perm.permits(&c.ns, &c.cred, if info.alter { S_IWUGO } else { S_IRUGO }) {
        return Err(EACCES);
    }
    if g.perm.deleted || undo.is_some_and(|u| g.undo_at(u).is_none()) {
        return Err(EIDRM);
    }
    let pid = c.tgid();
    let mut blocking = 0;
    match g.perform(&sops, undo, pid, info.dupsop, &mut blocking) {
        0 => {
            if info.alter {
                g.smart_update(Some(&sops), true);
            } else {
                g.set_otime(Some(&sops));
            }
            g.settle();
            return Ok(());
        }
        r if r < 0 => {
            g.settle();
            return Err(-r as i64);
        }
        _ => {}
    }
    let q = Arc::new(Queue { sops, undo, pid, alter: info.alter, dupsop: info.dupsop, blocking: AtomicUsize::new(blocking), status: AtomicI32::new(PENDING), wq: WaitQueue::new() });
    g.enqueue(q.clone())?;
    g.settle();
    drop(g);
    SLEEPS.inc();
    // Holds what the waiter needs: its array stays (`a`), the undo list too (`list`).
    let _keep = list;
    loop {
        sleep_once(&q.wq, c, end, &|| q.status() != PENDING);
        let st = q.status();
        if st != PENDING {
            return if st == 0 { Ok(()) } else { Err(-st as i64) };
        }
        let timed_out = end.is_some_and(|e| time::now_ns() >= e);
        if !timed_out && !c.interrupted() {
            continue;
        }
        let mut g = a.inner.lock();
        let st = q.status();
        if st != PENDING {
            return if st == 0 { Ok(()) } else { Err(-st as i64) };
        }
        g.unlink(&q);
        g.settle();
        return Err(if timed_out { EAGAIN } else { EINTR });
    }
}

/// One line of `/proc/sysvipc/sem`, IPC_STAT's content.
pub struct SemStat {
    pub perm: Perm,
    pub otime: i64,
    pub ctime: i64,
    pub nsems: usize,
}

impl SemArray {
    pub(super) fn stat(&self) -> SemStat {
        let g = self.inner.lock();
        SemStat { perm: g.perm.clone(), otime: g.sems.iter().map(|s| s.otime).max().unwrap_or(0), ctime: g.ctime, nsems: self.nsems }
    }
}

#[cfg(test)]
impl Queue {
    pub(super) fn for_test(sops: Vec<SemBuf>) -> Arc<Queue> {
        let alter = sops.iter().any(|s| s.op != 0);
        Arc::new(Queue { sops, undo: None, pid: 1, alter, dupsop: false, blocking: AtomicUsize::new(0), status: AtomicI32::new(PENDING), wq: WaitQueue::new() })
    }
    pub(super) fn status_for_test(&self) -> i32 {
        self.status()
    }
    pub(super) fn pending_for_test(&self) -> bool {
        self.status() == PENDING
    }
}

#[cfg(test)]
impl SemArray {
    pub(super) fn for_test(nsems: usize) -> Arc<SemArray> {
        let mut sems = Vec::new();
        sems.resize_with(nsems, || Sem { val: 0, pid: 0, otime: 0, alter: Vec::new(), zero: Vec::new() });
        let perm = Perm::new(0, 0o600, &crate::cred::Cred::new(1000, 1000));
        let inner = SemInner { perm, ctime: 0, sems, alter: Vec::new(), zero: Vec::new(), complex: 0, undo: Vec::new() };
        Arc::new(SemArray { nsems, inner: SpinLock::named("ipc.sem", inner) })
    }
}
