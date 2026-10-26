//! signalfd (AB-6), after linux-6.6/fs/signalfd.c: `signalfd4` (289) and `signalfd` (282) make or change an
//! anonymous file whose reads take the reading thread's pending signals of its mask (its own queue, then
//! its group's) as 128-byte `signalfd_siginfo` records; poll says EPOLLIN while one is pending. Wayland
//! compositors and many daemons block the signals and read them here.
//!
//! As on Linux the file belongs to no process: a reader always reads its own signals (after fork, the
//! child's). Poll wake-ups come from the group that made the file (`Group::sig_poll`), which is the reader in
//! every program known; a forked child polling an inherited signalfd sees its readiness only when it looks
//! again (decision in plan/notes/ab-AB-6.md).

use crate::SysCtx;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use proc::Group;
use proc::signal::{UNBLOCKABLE, signalfd_record};
use uapi::ab::sigabi::SIGSET_SIZE;
use uapi::ab::siginfo::{SFD_CLOEXEC, SFD_NONBLOCK, SIGNALFD_SIGINFO_SIZE};
use uapi::errno::*;
use vfs::abi::Attr;
use vfs::poll::{Events, PollQueue, Pollable};
use vfs::{Errno, File, OpenFile};

const O_RDWR: u64 = 2;

klog::counter!(SIGNALFDS, "ab.signalfds");

struct SignalFd {
    /// The signals it reads (SIGKILL and SIGSTOP never).
    mask: Arc<AtomicU64>,
    /// The group that made it: its queue wakes pollers.
    owner: Arc<Group>,
}

/// signalfd objects by address (`f_op == &signalfd_fops`), for signalfd4 on an existing fd.
static OBJECTS: SpinLock<BTreeMap<usize, Arc<AtomicU64>>> = SpinLock::named("ab.signalfd.objects", BTreeMap::new());

impl Drop for SignalFd {
    fn drop(&mut self) {
        let gone = OBJECTS.lock().remove(&(self as *const SignalFd as *const u8 as usize));
        drop(gone);
    }
}

impl SignalFd {
    fn mask(&self) -> u64 {
        self.mask.load(Ordering::Acquire)
    }

    /// `signalfd_read_iter`: as many records as fit (at least one: fewer than 128 bytes is EINVAL); only the
    /// first may wait. `sink(record) -> false` = the copy faulted.
    fn read_records(&self, room: usize, nonblock: bool, sink: &mut dyn FnMut(&[u8; 128]) -> bool) -> Result<usize, Errno> {
        let count = room / SIGNALFD_SIGINFO_SIZE;
        if count == 0 {
            return Err(EINVAL);
        }
        let Some(t) = proc::current_thread() else { return Err(EAGAIN) };
        let mut total = 0;
        let mut nb = nonblock;
        for _ in 0..count {
            match t.signalfd_dequeue(self.mask(), nb) {
                Ok(info) => {
                    if !sink(&signalfd_record(&info)) {
                        return if total > 0 { Ok(total) } else { Err(EFAULT) };
                    }
                    total += SIGNALFD_SIGINFO_SIZE;
                }
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            }
            nb = true;
        }
        Ok(total)
    }
}

impl File for SignalFd {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, true)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        let mut at = 0;
        self.read_records(buf.len(), nonblock, &mut |r| {
            buf[at..at + 128].copy_from_slice(r);
            at += 128;
            true
        })
    }
    fn read_iter(&self, to: &mut dyn vfs::node::IoSink, status: u64) -> Option<Result<usize, Errno>> {
        let room = to.left();
        Some(self.read_records(room, status & SFD_NONBLOCK != 0, &mut |r| to.copy(r) == 128))
    }
    /// No write operation: `vfs_write` answers EINVAL.
    fn write_stream(&self, _buf: &[u8], _nonblock: bool) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        0o600
    }
    /// `signalfd_poll`: EPOLLIN while the calling thread has a signal of the mask pending.
    fn events(&self) -> Events {
        let pending = match proc::current_thread() {
            Some(t) => t.has_pending_in(self.mask()),
            None => self.owner.signals.raw(|s| {
                let k = s.main_key();
                s.has_pending_in(k, self.mask())
            }),
        };
        Events(if pending { Events::IN } else { 0 })
    }
    /// `noop_llseek`.
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Ok(0))
    }
    fn attr(&self) -> Option<Attr> {
        Some(crate::timerfd::anon_attr())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.owner.sig_poll)
    }
    fn can_poll(&self) -> bool {
        true
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn link_name(&self) -> Option<String> {
        Some("anon_inode:[signalfd]".to_string())
    }
}

impl Pollable for SignalFd {
    fn events(&self) -> Events {
        File::events(self)
    }
    fn wait_queue(&self) -> &WaitQueue {
        &self.owner.sig_poll
    }
}

/// `do_signalfd4`.
/// AB-7a: `signalfd_show_fdinfo` (linux-6.6/fs/signalfd.c :232) for `/proc/<pid>/fdinfo`: the mask as given,
/// without SIGKILL and SIGSTOP (Linux never stores them). None for other files.
pub(crate) fn fdinfo(f: &OpenFile) -> Option<String> {
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    let mask = OBJECTS.lock().get(&addr)?.load(Ordering::Acquire) & !UNBLOCKABLE;
    Some(alloc::format!("sigmask:\t{mask:016x}\n"))
}

fn signalfd4(c: &mut SysCtx<'_>, ufd: i32, umask: u64, size: u64, flags: u64) -> i64 {
    if size != SIGSET_SIZE {
        return -EINVAL;
    }
    let mask = match c.proc.mm.read_u64(umask) {
        Ok(m) => m & !UNBLOCKABLE,
        Err(_) => return -EFAULT,
    };
    if flags & !(SFD_CLOEXEC | SFD_NONBLOCK) != 0 {
        return -EINVAL;
    }
    if ufd == -1 {
        let m = Arc::new(AtomicU64::new(mask));
        let obj = Arc::new(SignalFd { mask: m.clone(), owner: c.proc.group().clone() });
        OBJECTS.lock().insert(Arc::as_ptr(&obj) as *const u8 as usize, m);
        SIGNALFDS.inc();
        let f = OpenFile::anon(obj, O_RDWR | (flags & SFD_NONBLOCK));
        return c.proc.fds.install_flags(f, 0, flags & SFD_CLOEXEC != 0).map(|fd| fd as i64).unwrap_or_else(|e| -e);
    }
    if ufd < 0 {
        return -EBADF;
    }
    let f = match c.proc.fds.get(ufd as usize) {
        Ok(f) if !f.is_path_only() => f,
        _ => return -EBADF,
    };
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    let Some(m) = OBJECTS.lock().get(&addr).cloned() else { return -EINVAL };
    m.store(mask, Ordering::Release);
    // A reader waiting for the old mask looks again.
    c.proc.sig_poll.wake_all();
    ufd as i64
}

fn sys_signalfd4(c: &mut SysCtx<'_>) -> i64 {
    let [ufd, umask, size, flags, ..] = c.args;
    signalfd4(c, ufd as i32, umask, size, flags as u32 as u64)
}
crate::syscall_nr!(uapi::ab::nr_more::SIGNALFD4, "signalfd4", sys_signalfd4);

fn sys_signalfd(c: &mut SysCtx<'_>) -> i64 {
    let [ufd, umask, size, ..] = c.args;
    signalfd4(c, ufd as i32, umask, size, 0)
}
crate::syscall_nr!(uapi::ab::nr_more::SIGNALFD, "signalfd", sys_signalfd);
