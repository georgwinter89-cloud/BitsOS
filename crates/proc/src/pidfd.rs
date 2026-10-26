//! AB-12: process handles (pidfds), after linux-6.6/kernel/fork.c: `pidfd_fops` (:2091), `pidfd_poll` (:2075),
//! `pidfd_show_fdinfo` (:2007) and `__pidfd_prepare` (:2129). A pidfd holds the process's `Group` (Linux: its
//! `struct pid`), never a number: once the process is reaped and its pid reused, the pidfd still names the old
//! one (gone: ESRCH, fdinfo `Pid: -1`). The syscalls are in `syscall::pidfd`; CLONE_PIDFD makes one in
//! `clone.rs`.

use crate::{Group, Process};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::abi::{Attr, O_NONBLOCK};
use vfs::poll::{Events, PollQueue, Pollable};
use vfs::{Errno, File, OpenFile};

const O_RDWR: u64 = 2;

klog::counter!(PIDFDS, "proc.pidfds");

struct PidFd {
    group: Arc<Group>,
}

/// pidfd objects by address (Linux `file->f_op == &pidfd_fops`): which open files are pidfds and of whom.
static OBJECTS: SpinLock<BTreeMap<usize, Arc<Group>>> = SpinLock::named("proc.pidfd.objects", BTreeMap::new());

impl Drop for PidFd {
    fn drop(&mut self) {
        let gone = OBJECTS.lock().remove(&(self as *const PidFd as *const u8 as usize));
        drop(gone);
    }
}

/// The anonymous inode every anon file shares (eventfd's fstat answer).
fn anon_attr() -> Attr {
    static ANON: ksync::Once<Attr> = ksync::Once::new();
    *ANON.get_or_init(|| File::attr(&*vfs::eventfd::EventFd::new(0, 0)).unwrap_or_default())
}

impl File for PidFd {
    /// No read or write operation (no FMODE_CAN_READ/WRITE): `vfs_read`/`vfs_write` answer EINVAL.
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        0o600
    }
    /// `pidfd_poll`: EPOLLIN | EPOLLRDNORM once every thread ended (`thread_group_exited`), read fresh so
    /// epoll stays level-triggered.
    fn events(&self) -> Events {
        Events(if self.group.is_finished() { Events::IN | Events::RDNORM } else { 0 })
    }
    /// No llseek in `pidfd_fops`: FMODE_LSEEK is not set, lseek is ESPIPE.
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Err(ESPIPE))
    }
    fn attr(&self) -> Option<Attr> {
        Some(anon_attr())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.group.exit_poll)
    }
    fn can_poll(&self) -> bool {
        true
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn link_name(&self) -> Option<String> {
        Some("anon_inode:[pidfd]".to_string())
    }
}

impl Pollable for PidFd {
    fn events(&self) -> Events {
        File::events(self)
    }
    fn wait_queue(&self) -> &WaitQueue {
        &self.group.exit_poll
    }
}

/// `__pidfd_prepare`'s file: O_RDWR, plus O_NONBLOCK when asked (pidfd_open's PIDFD_NONBLOCK); the caller
/// installs it close-on-exec, as Linux always does.
pub fn new_file(g: &Arc<Group>, nonblock: bool) -> Arc<OpenFile> {
    let obj = Arc::new(PidFd { group: g.clone() });
    OBJECTS.lock().insert(Arc::as_ptr(&obj) as *const u8 as usize, g.clone());
    PIDFDS.inc();
    klog::event!(Debug, "proc", "pidfd.new", pid = g.tgid, nonblock = nonblock as u32);
    OpenFile::anon(obj, O_RDWR | if nonblock { O_NONBLOCK } else { 0 })
}

/// Linux `pidfd_pid`: the process an open file is a pidfd of; None for every other file (EBADF).
pub fn group_of(f: &OpenFile) -> Option<Arc<Group>> {
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    OBJECTS.lock().get(&addr).cloned()
}

/// The process behind a pidfd while it is not reaped (live or a zombie; Linux `pid_task(pid, PIDTYPE_PID)`):
/// the table's entry for its pid, when that entry is still this process.
pub fn process_of(g: &Arc<Group>) -> Option<Arc<Process>> {
    crate::find(g.tgid).filter(|p| Arc::ptr_eq(p.group(), g))
}

/// AB-7a: `pidfd_show_fdinfo` for `/proc/<pid>/fdinfo`: `Pid:` and `NSpid:` (one pid namespace), -1 once the
/// process is reaped. None for other files.
pub fn fdinfo(f: &OpenFile) -> Option<String> {
    let g = group_of(f)?;
    let nr: i64 = if process_of(&g).is_some() { g.tgid as i64 } else { -1 };
    Some(alloc::format!("Pid:\t{nr}\nNSpid:\t{nr}\n"))
}

/// pidfds open now (`proc.pidfds` counts every one made).
pub fn open_count() -> usize {
    OBJECTS.lock().len()
}
