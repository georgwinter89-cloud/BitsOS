//! SX-IPC: Linux's SysV IPC (reference linux-7.3.1/ipc/: sem.c, shm.c, msg.c, msgutil.c, util.c, namespace.c).
//! Steam's `tier0` asks for semaphores, X11's MIT-SHM and games for segments.
//!
//! Everything lives in an IPC namespace (`ns::misc::IpcNs::tables`) and goes when it dies; a mapped segment holds
//! its namespace, as Linux's `shm_file_data` does. Limits are Linux's defaults; behind them the tables grow as
//! needed (`ids`), so no size is fixed here (R1, Georg 14.10.).
//!
//! - `ids`: ids with sequence numbers, keys, modes and owners (util.c).
//! - `sem`, `sem_wake`, `sem_ctl`, `sem_undo`: semaphores with Linux's queues, wake order and SEM_UNDO (sem.c).
//! - `shm`, `shm_map`: segments on the memfd tmpfs (`vfs::memfd`), mapped through MM's normal paths (shm.c).
//! - `msg`, `msg_ctl`: message queues (msg.c).
//! - `text`: the lines of `/proc/sysvipc/*`.
//!
//! User memory is only touched by the syscall glue (`syscall::sysv`), never under a lock here.
//!
//! Locks: `ipc.ids.{sem,shm,msg}` (one per table, Linux's `ids->rwsem`, a read/write spin lock: nothing sleeps under
//! it and tables grow with `try_reserve`) > `ipc.sem`, `ipc.shm`, `ipc.msg` (one per object, Linux's
//! `kern_ipc_perm.lock`) > `ipc.undo` (a list's arrays), `ipc.msg.slot` (a receiver's hand-over); wake-ups run under
//! the object lock (the scheduler's lock sits inside every subsystem lock, as for futex buckets). MM's `mm.as` is
//! outside all of them: a VMA of a segment opens and closes under it (`shm_map`), and no `ipc.*` lock is held while
//! calling MM.
//!
//! Approximation (with the namespaces' own, `ns`): V2 keeps `nsproxy` per thread group, so the IPC namespace is the
//! group's, not the thread's.

pub mod ids;
pub mod msg;
pub mod msg_ctl;
pub mod sem;
pub mod sem_ctl;
pub mod sem_undo;
mod sem_wake;
pub mod shm;
pub mod shm_map;
pub mod text;
#[cfg(test)]
mod tests;

use crate::Process;
use crate::cred::Cred;
use crate::group::Group;
use crate::ns::misc::IpcNs;
use crate::task::Thread;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::ab::sysv::*;
use vfs::Errno;

/// A sleeping IPC call looks at its end conditions at least this often. A signal's kick wakes a program's thread at
/// once; the slice bounds the wait of a kernel thread calling for a process (ktests), which no kick reaches.
const SLICE_NS: u64 = 250_000_000;

/// The limits of one namespace (`/proc/sys/kernel`, each writable as Linux allows).
pub struct Limits {
    /// semmsl, semmns, semopm, semmni (`sem_ctls`).
    pub sem: [AtomicI32; 4],
    pub shmmax: AtomicU64,
    pub shmall: AtomicU64,
    pub shmmni: AtomicI32,
    pub shm_rmid_forced: AtomicI32,
    pub msgmax: AtomicI32,
    pub msgmnb: AtomicI32,
    pub msgmni: AtomicI32,
}

impl Limits {
    fn linux_defaults() -> Limits {
        Limits {
            sem: [AtomicI32::new(SEMMSL), AtomicI32::new(SEMMNS), AtomicI32::new(SEMOPM), AtomicI32::new(SEMMNI)],
            shmmax: AtomicU64::new(SHMMAX),
            shmall: AtomicU64::new(SHMALL),
            shmmni: AtomicI32::new(SHMMNI),
            shm_rmid_forced: AtomicI32::new(0),
            msgmax: AtomicI32::new(MSGMAX),
            msgmnb: AtomicI32::new(MSGMNB),
            msgmni: AtomicI32::new(MSGMNI),
        }
    }
    pub(crate) fn sem_at(&self, i: usize) -> i32 {
        self.sem[i].load(Ordering::Relaxed)
    }
}

/// What one IPC namespace holds (`sem_init_ns`, `shm_init_ns`, `msg_init_ns`).
pub struct Tables {
    pub limits: Limits,
    pub sem: sem::SemTable,
    pub shm: shm::ShmTable,
    pub msg: msg::MsgTable,
}

impl Tables {
    pub(crate) fn new() -> Tables {
        Tables { limits: Limits::linux_defaults(), sem: sem::SemTable::new(), shm: shm::ShmTable::new(), msg: msg::MsgTable::new() }
    }
}

/// Who calls: the namespace and credentials of the group, as `current` gives them to Linux's IPC code.
pub struct Caller<'a> {
    pub group: &'a Group,
    pub ns: Arc<IpcNs>,
    pub cred: Arc<Cred>,
    /// The calling thread's id (a receiver's `task_pid`); the group's id when a kernel thread calls for it.
    pub tid: u32,
    /// Stable through an execve that changes `tid` (a segment's `shm_creator`); 0 for a kernel thread.
    pub thread_key: u64,
}

impl<'a> Caller<'a> {
    pub fn new(p: &'a Process, t: Option<&Thread>) -> Caller<'a> {
        Caller { group: p.group(), ns: p.nsset().ipc.clone(), cred: p.cred(), tid: t.map_or(p.tgid, Thread::tid), thread_key: t.map_or(0, Thread::sig_key) }
    }
    pub fn tgid(&self) -> u32 {
        self.group.tgid
    }
    /// `pid_vnr`: a global id as the caller's pid namespace numbers it (0: not visible there).
    pub fn vnr(&self, id: u32) -> u32 {
        self.group.vnr(id)
    }
    fn interrupted(&self) -> bool {
        self.group.signal_pending()
    }
    pub(crate) fn limits(&self) -> &Limits {
        &self.ns.tables.limits
    }
}

/// What a thread carries for SysV IPC (`task_struct.sysvsem`, `sysvshm`).
pub struct ThreadIpc {
    pub undo: sem_undo::UndoSlot,
    /// The namespaces it made segments in (Linux's `shm_clist` entries name their namespace).
    shm_ns: SpinLock<Vec<Weak<IpcNs>>>,
}

impl ThreadIpc {
    pub(crate) const fn new() -> ThreadIpc {
        ThreadIpc { undo: sem_undo::UndoSlot::new(), shm_ns: SpinLock::named("ipc.shmns", Vec::new()) }
    }
    /// A segment was made in `ns`; its thread will visit it at exit. No room: only `shm_rmid_forced` misses it.
    pub fn made_segment_in(&self, ns: &Arc<IpcNs>) {
        let mut g = self.shm_ns.lock();
        if !g.iter().any(|w| w.as_ptr() == Arc::as_ptr(ns)) && g.try_reserve(1).is_ok() {
            g.push(Arc::downgrade(ns));
        }
    }
    /// `exit_shm`: its segments lose their creator (`shm_rmid_forced` destroys unattached ones).
    pub(crate) fn exit_shm(&self, key: u64) {
        let list = core::mem::take(&mut *self.shm_ns.lock());
        for w in list {
            if let Some(ns) = w.upgrade() {
                shm::creator_gone(&ns, key);
            }
        }
    }
}

/// What the end of a thread does here (`exit_sem` and `exit_shm` of `do_exit`).
pub(crate) fn thread_exit(t: &Thread) {
    let (ipc, tgid) = (t.ipc(), t.group().tgid);
    sem_undo::exit_sem(&ipc.undo, tgid);
    ipc.exit_shm(t.sig_key());
}

/// unshare(CLONE_SYSVSEM) or a new IPC namespace; CLONE_NEWIPC also orphans its segments in the old one
/// (kernel/fork.c:3320). setns(CLONE_NEWIPC) passes `orphan_segments` false (kernel/nsproxy.c:556).
pub(crate) fn leave_undo(t: &Thread, orphan_segments: bool) {
    let ipc = t.ipc();
    sem_undo::exit_sem(&ipc.undo, t.group().tgid);
    if orphan_segments {
        ipc.exit_shm(t.sig_key());
    }
}

/// `ktime_get_real_seconds`.
pub(crate) fn now_secs() -> i64 {
    (time::wall::realtime_ns().unwrap_or_else(time::now_ns) / 1_000_000_000) as i64
}

/// Sleeps on `wq` until `done()`, `end` (monotonic) passed or the caller has a signal to look at; one slice at most.
fn sleep_once(wq: &WaitQueue, c: &Caller<'_>, end: Option<u64>, done: &dyn Fn() -> bool) {
    let slice = match end {
        Some(e) => e.saturating_sub(time::now_ns()).min(SLICE_NS),
        None => SLICE_NS,
    };
    if slice > 0 {
        wq.wait_until_timeout(|| done() || c.interrupted(), slice);
    }
}

fn nomem<T>(_: T) -> Errno {
    uapi::errno::ENOMEM
}
