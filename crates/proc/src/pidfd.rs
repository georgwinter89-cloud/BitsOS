//! AB-12, L7-PROC: process handles (pidfds) as Linux 7.3.1's pidfs files: `pidfs_file_operations`
//! (linux-7.3.1/fs/pidfs.c:713), `pidfd_poll` (:306), `pidfd_show_fdinfo` (:272), `pidfs_file_release` (:693),
//! `pidfs_init_inode` (:1006) and `pidfd_prepare` (kernel/fork.c:1918). A pidfd holds the process's `Group`
//! (Linux: its `struct pid`), with PIDFD_THREAD one of its threads, never a number: once the process is reaped and
//! its pid reused, the pidfd still names the old one (gone: ESRCH, fdinfo `Pid: -1`, poll IN|HUP). Every pid has
//! its own pidfs inode number from one 64-bit counter that never repeats (`pidfs_alloc_ino`), so fstat tells
//! pidfds of different processes apart and two of one process alike. The syscalls and the ioctls are in
//! `syscall::pidfd`, the file handles (name_to_handle_at, open_by_handle_at) in `syscall::fhandle`; CLONE_PIDFD
//! makes one in `clone.rs`.

use crate::task::Thread;
use crate::{Group, Process};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::abi::{Attr, O_NONBLOCK};
use vfs::poll::{Events, PollQueue, Pollable};
use vfs::{Errno, File, OpenFile};

const O_RDWR: u64 = 2;
/// `PIDFD_AUTOKILL` (O_TRUNC), kept in `f_flags` beside PIDFD_THREAD (linux-7.3.1/fs/pidfs.c:961).
const PIDFD_AUTOKILL: u64 = 0o1000;

klog::counter!(PIDFDS, "proc.pidfds");

/// What a pidfd names: a process, or with PIDFD_THREAD one thread of it (`thread`: the tid and, for a thread
/// that is not the leader, the thread itself; a leader's id is followed by number while the process lives).
#[derive(Clone)]
pub struct Target {
    pub group: Arc<Group>,
    pub thread: Option<(u32, Option<Arc<Thread>>)>,
}

impl Target {
    /// PIDFD_THREAD.
    pub fn is_thread(&self) -> bool {
        self.thread.is_some()
    }
    /// Whether it names a thread that is not the leader (a leader's pid lives until the process is reaped).
    fn other_thread(&self) -> Option<(u32, &Arc<Thread>)> {
        let (tid, t) = self.thread.as_ref().filter(|(tid, _)| *tid != self.group.tgid)?;
        Some((*tid, t.as_ref()?))
    }
    /// Whether the pid still has its task (`pid_task(pid, PIDTYPE_PID)`): the leader's until the process is
    /// reaped, another thread's until that thread ended (it is released at once).
    pub fn attached(&self) -> bool {
        match self.other_thread() {
            Some((tid, t)) => crate::task::find_thread(tid).is_some_and(|x| Arc::ptr_eq(&x, t)),
            None => process_of(&self.group).is_some(),
        }
    }
    /// The task ended and is not a leader waiting for its other threads (`exit_state && !delay_group_leader`).
    fn ended(&self) -> bool {
        match self.other_thread() {
            Some((_, t)) => t.exit_requested() || self.group.is_finished(),
            None => self.group.is_finished(),
        }
    }
    /// The global id of the task (`pid_nr`).
    pub fn id(&self) -> u32 {
        self.thread.as_ref().map_or(self.group.tgid, |(tid, _)| *tid)
    }
    /// The thread a thread pidfd names while it is attached (for the leader's id: whoever holds it now).
    pub fn live_thread(&self) -> Option<Arc<Thread>> {
        let (tid, _) = self.thread.as_ref()?;
        if !self.attached() {
            return None;
        }
        match self.other_thread() {
            Some((_, t)) => Some(t.clone()),
            None => crate::task::find_thread(*tid).filter(|x| Arc::ptr_eq(x.group(), &self.group)),
        }
    }
    /// `PIDFD_INFO_EXIT`'s `exit_code` once the task was released (`pidfs_exit` keeps `tsk->exit_code`,
    /// linux-7.3.1/fs/pidfs.c:788): a thread that ended alone (its exit(2), or a sibling's execve) keeps its own
    /// code as a wait status also after the process ended; a thread the group's end took, and the process, the
    /// group's wait status.
    pub fn exit_code(&self) -> Option<u32> {
        if self.attached() {
            return None;
        }
        let group = self.group.status().map(|s| s.wait_status());
        match self.other_thread() {
            Some((_, t)) if t.exit_requested() || group.is_none() => Some((t.code() & 0xff) << 8),
            _ => group,
        }
    }
    /// The pidfs inode number of the task's pid.
    pub fn ino(&self) -> u64 {
        match self.other_thread() {
            Some((_, t)) => ino_cell(&t.pidfs_ino),
            None => ino_cell(&self.group.pidfs_ino),
        }
    }
}

/// `pidfs_alloc_ino`: 64-bit numbers from 1 up, never reused.
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

fn ino_cell(c: &AtomicU64) -> u64 {
    let v = c.load(Ordering::Acquire);
    if v != 0 {
        return v;
    }
    let n = NEXT_INO.fetch_add(1, Ordering::Relaxed);
    match c.compare_exchange(0, n, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => n,
        Err(won) => won,
    }
}

/// pidfs's device (one anonymous device, as Linux's pidfs super block).
fn pidfs_dev() -> u64 {
    static DEV: ksync::Once<u64> = ksync::Once::new();
    *DEV.get_or_init(vfs::dentry::anon_dev)
}

struct PidFd {
    t: Target,
    /// PIDFD_AUTOKILL (CLONE_PIDFD_AUTOKILL): SIGKILL to the process when the last reference to the file goes.
    autokill: bool,
}

/// pidfd objects by address (Linux `file->f_op == &pidfs_file_operations`): which open files are pidfds and of
/// whom, with the bits F_GETFL shows beyond the open file's status (PIDFD_THREAD, PIDFD_AUTOKILL).
static OBJECTS: SpinLock<BTreeMap<usize, (Target, u64)>> = SpinLock::named("proc.pidfd.objects", BTreeMap::new());

impl Drop for PidFd {
    fn drop(&mut self) {
        let gone = OBJECTS.lock().remove(&(self as *const PidFd as *const u8 as usize));
        drop(gone);
        // `pidfs_file_release`: the process (the TGID task) while it is not reaped.
        if self.autokill && process_of(&self.t.group).is_some_and(|p| !p.is_finished()) {
            klog::event!(Info, "proc", "pidfd.autokill", pid = self.t.group.tgid);
            self.t.group.kill(uapi::ab::signal::SIGKILL);
        }
    }
}

impl File for PidFd {
    /// No read or write operation (no FMODE_CAN_READ/WRITE): `vfs_read`/`vfs_write` answer EINVAL.
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    /// `pidfs_init_inode`: S_IRWXU and no file type.
    fn mode(&self) -> u32 {
        0o700
    }
    /// `pidfd_poll`: EPOLLIN | EPOLLRDNORM | EPOLLHUP once the pid has no task (reaped, or a thread released),
    /// EPOLLIN | EPOLLRDNORM once the task ended (a leader: once every thread ended); read fresh so epoll stays
    /// level-triggered.
    fn events(&self) -> Events {
        Events(if !self.t.attached() {
            Events::IN | Events::RDNORM | Events::HUP
        } else if self.t.ended() {
            Events::IN | Events::RDNORM
        } else {
            0
        })
    }
    /// No llseek in `pidfs_file_operations`: FMODE_LSEEK is not set, lseek is ESPIPE.
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Err(ESPIPE))
    }
    /// The pidfs inode (`anon_inode_getattr`): the pid's number, mode 0700, root's, one link.
    fn attr(&self) -> Option<Attr> {
        Some(Attr { dev: pidfs_dev(), ino: self.t.ino(), mode: 0o700, nlink: 1, blksize: 4096, ..Default::default() })
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.t.group.exit_poll)
    }
    fn can_poll(&self) -> bool {
        true
    }
    fn poll_exact(&self) -> bool {
        true
    }
    /// `pidfs_dname`: lsof expects the anon_inode name.
    fn link_name(&self) -> Option<String> {
        Some("anon_inode:[pidfd]".to_string())
    }
}

impl Pollable for PidFd {
    fn events(&self) -> Events {
        File::events(self)
    }
    fn wait_queue(&self) -> &WaitQueue {
        &self.t.group.exit_poll
    }
}

/// `pidfs_alloc_file`: O_RDWR, plus O_NONBLOCK when asked; `thread` (a tid and, unless it is the leader's, the
/// thread) makes a PIDFD_THREAD pidfd, `autokill` one that kills the process at its last close. The caller installs
/// it close-on-exec, as Linux always does.
pub fn new_file(g: &Arc<Group>, thread: Option<(u32, Option<Arc<Thread>>)>, nonblock: bool, autokill: bool) -> Arc<OpenFile> {
    let t = Target { group: g.clone(), thread };
    let shown = if t.is_thread() { uapi::ab::pidfd::PIDFD_THREAD } else { 0 } | if autokill { PIDFD_AUTOKILL } else { 0 };
    make(t, shown, if nonblock { O_NONBLOCK } else { 0 }, autokill)
}

fn make(t: Target, shown: u64, status: u64, autokill: bool) -> Arc<OpenFile> {
    let pid = t.group.tgid;
    let obj = Arc::new(PidFd { t: t.clone(), autokill });
    OBJECTS.lock().insert(Arc::as_ptr(&obj) as *const u8 as usize, (t, shown));
    PIDFDS.inc();
    klog::event!(Debug, "proc", "pidfd.new", pid = pid, shown = klog::Hex(shown), status = klog::Hex(status));
    OpenFile::anon(obj, O_RDWR | status)
}

/// L7-RESTE-AB 2: the pidfd open_by_handle_at makes (`pidfs_export_open` → `pidfs_dentry_open`,
/// linux-7.3.1/fs/pidfs.c:987, :951) from flags `pidfs_export_permission` let through: always O_RDWR; `f_flags`
/// keeps what the caller passed besides (O_WRONLY makes the access mode 3, O_CLOEXEC stays) and O_EXCL as
/// PIDFD_THREAD, which F_GETFL shows. The fd is close-on-exec only with O_CLOEXEC (the caller installs it).
pub fn open_handle(t: Target, oflags: u64) -> Arc<OpenFile> {
    use vfs::abi::{O_CLOEXEC, O_EXCL, O_WRONLY};
    make(t, oflags & (O_EXCL | O_CLOEXEC | O_WRONLY), oflags & O_NONBLOCK, false)
}

/// L7-RESTE-AB 2: the task whose pid has pidfs inode number `ino`, while it has one (`pidfs_ino_get_pid`,
/// linux-7.3.1/fs/pidfs.c:897: a pid leaves pidfs's table when its task is released, a process at the reap, any
/// other thread when it ended). A thread's pid gives that thread, a leader's the process; numbers never handed
/// out (0, or not yet asked for: the inode number is given at the first look) name nothing.
pub fn by_ino(ino: u64) -> Option<Target> {
    if ino == 0 {
        return None;
    }
    for p in crate::processes() {
        let g = p.group();
        let t = if g.pidfs_ino.load(Ordering::Acquire) == ino {
            Target { group: g.clone(), thread: None }
        } else if let Some(th) = g.threads().into_iter().find(|th| th.tid() != g.tgid && th.pidfs_ino.load(Ordering::Acquire) == ino) {
            Target { group: g.clone(), thread: Some((th.tid(), Some(th))) }
        } else {
            continue;
        };
        return t.attached().then_some(t);
    }
    None
}

/// Linux `pidfd_pid`: what an open file is a pidfd of; None for every other file (EBADF).
pub fn target_of(f: &OpenFile) -> Option<Target> {
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    OBJECTS.lock().get(&addr).map(|(t, _)| t.clone())
}

/// The process an open file is a pidfd of (also for a thread pidfd: its process).
pub fn group_of(f: &OpenFile) -> Option<Arc<Group>> {
    target_of(f).map(|t| t.group)
}

/// The bits F_GETFL shows for a pidfd beyond its status flags; None for every other file.
pub fn shown_flags(f: &OpenFile) -> Option<u64> {
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    OBJECTS.lock().get(&addr).map(|(_, s)| *s)
}

/// The process behind a pidfd while it is not reaped (live or a zombie; Linux `pid_task(pid, PIDTYPE_PID)`):
/// the table's entry for its pid, when that entry is still this process.
pub fn process_of(g: &Arc<Group>) -> Option<Arc<Process>> {
    crate::find(g.tgid).filter(|p| Arc::ptr_eq(p.group(), g))
}

/// Wakes the pidfd pollers of `g` (`do_notify_pidfd`, and `__unhash_process`'s `wake_up_all` with `hup`): a thread
/// ended or the process was reaped. Readiness itself is read fresh by `events`.
pub(crate) fn notify(g: &Group, hup: bool) {
    if g.exit_poll.waiter_count() != 0 {
        let e = Events::IN | Events::RDNORM | if hup { Events::HUP } else { 0 };
        g.exit_poll.wake_poll(e);
    }
}

/// AB-7a: `pidfd_show_fdinfo` for `/proc/<pid>/fdinfo`: `Pid:` and `NSpid:` (SX-2: every level), -1 once the pid
/// has no task. None for other files.
pub fn fdinfo(f: &OpenFile) -> Option<String> {
    let t = target_of(f)?;
    if !t.attached() {
        return Some("Pid:\t-1\nNSpid:\t-1\n".to_string());
    }
    // SX-2: /proc's pid namespace is the initial one; NSpid adds the numbers down to the task's own.
    let id = t.id();
    let mut ns = alloc::format!("{id}");
    for nr in t.group.pid_ns.chain(id).iter().rev() {
        ns.push_str(&alloc::format!("\t{nr}"));
    }
    Some(alloc::format!("Pid:\t{id}\nNSpid:\t{ns}\n"))
}

/// pidfds open now (`proc.pidfds` counts every one made).
pub fn open_count() -> usize {
    OBJECTS.lock().len()
}
