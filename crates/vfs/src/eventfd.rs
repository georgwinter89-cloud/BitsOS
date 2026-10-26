//! eventfd (FS-3), after linux-6.6/fs/eventfd.c: a 64-bit counter behind an fd.
//!
//! read takes the whole count (EFD_SEMAPHORE: 1) and leaves 0 (count - 1), waiting while it is 0 (EAGAIN with
//! O_NONBLOCK); a read of fewer than 8 bytes is EINVAL, the value is copied after it was taken (a fault then
//! loses it, as Linux). write adds an 8-byte value: fewer than 8 bytes EINVAL, then EFAULT, then
//! 0xffff_ffff_ffff_ffff EINVAL; it waits while the sum would pass 0xffff_ffff_ffff_fffe. poll: POLLIN while
//! the count is above 0, POLLOUT while it is below 0xffff_ffff_ffff_fffe, POLLERR at 0xffff_ffff_ffff_ffff
//! (only `signal` from the kernel can reach it). Reads see the whole iovec (`read_iter`), writes one segment
//! per call (eventfd has a plain `write`, so writev loops as `do_loop_readv_writev`). The file is an anonymous
//! inode: fstat shows mode 0600 without a type, the one root-owned inode of anon_inodefs; F_GETFL = O_RDWR plus
//! O_NONBLOCK; lseek answers 0 (`noop_llseek`), pread/pwrite ESPIPE.

use crate::abi::{self, Attr};
use crate::inode::{Cred, Meta};
use crate::node::{File, IoSink, IoSource};
use crate::poll::{Events, PollQueue, Pollable};
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;

pub const EFD_SEMAPHORE: u64 = 1;
pub const EFD_CLOEXEC: u64 = abi::O_CLOEXEC;
pub const EFD_NONBLOCK: u64 = abi::O_NONBLOCK;
/// `EFD_FLAGS_SET`: every other bit of eventfd2's flags is EINVAL.
pub const EFD_FLAGS: u64 = EFD_SEMAPHORE | EFD_CLOEXEC | EFD_NONBLOCK;
const SLICE_NS: u64 = 50_000_000;

klog::counter!(EVENTFDS, "vfs.eventfds");

/// anon_inodefs: one device and one inode shared by every anonymous file (`anon_inode_inode`, made by the
/// kernel at boot: root's, mode 0600 without a type).
static ANON_DEV: ksync::Once<u64> = ksync::Once::new();
static ANON_INODE: ksync::Once<Meta> = ksync::Once::new();

pub(crate) fn anon_attr() -> Attr {
    let dev = *ANON_DEV.get_or_init(crate::dentry::anon_dev);
    ANON_INODE.get_or_init(|| Meta::new(1, 0o600, Cred::ROOT)).attr(dev)
}

pub struct EventFd {
    count: SpinLock<u64>,
    semaphore: bool,
    /// `ctx->id`: the smallest id not in use (Linux's `eventfd_ida`), shown in fdinfo.
    id: u32,
    /// `ctx->wqh` (FS-4: a `PollQueue`, keyed wakes as `eventfd_write`/`eventfd_read` make them).
    wq: PollQueue,
}

impl EventFd {
    pub fn new(initial: u32, flags: u64) -> Arc<EventFd> {
        EVENTFDS.inc();
        Arc::new(EventFd { count: SpinLock::named("vfs.eventfd", initial as u64), semaphore: flags & EFD_SEMAPHORE != 0, id: ids_take(), wq: PollQueue::new() })
    }

    /// `wake_up_locked_poll(&ctx->wqh, key)`.
    fn wake(&self, key: u32) {
        self.wq.wake_poll(key);
    }

    pub fn count(&self) -> u64 {
        *self.count.lock()
    }

    /// `eventfd_signal` for kernel users: adds `n`, saturating at u64::MAX (then POLLERR); returns what was
    /// added.
    pub fn signal(&self, n: u64) -> u64 {
        let added = {
            let mut c = self.count.lock();
            let add = n.min(u64::MAX - *c);
            *c += add;
            add
        };
        // `eventfd_signal_mask`: EPOLLIN (with the caller's mask; none here).
        self.wake(Events::IN);
        added
    }

    fn wait(&self, cond: impl Fn() -> bool) -> Result<(), Errno> {
        loop {
            if cond() {
                return Ok(());
            }
            if crate::dentry::interrupted() {
                return Err(EINTR);
            }
            self.wq.wait_until_timeout(&cond, SLICE_NS);
        }
    }

    /// `eventfd_read`: the value taken.
    fn take(&self, left: usize, nonblock: bool) -> Result<u64, Errno> {
        if left < 8 {
            return Err(EINVAL);
        }
        loop {
            {
                let mut c = self.count.lock();
                if *c > 0 {
                    let v = if self.semaphore { 1 } else { *c };
                    *c -= v;
                    drop(c);
                    self.wake(Events::OUT);
                    return Ok(v);
                }
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.wait(|| *self.count.lock() > 0)?;
        }
    }

    /// `eventfd_write` of the value `v`.
    fn add(&self, v: u64, nonblock: bool) -> Result<(), Errno> {
        if v == u64::MAX {
            return Err(EINVAL);
        }
        loop {
            {
                let mut c = self.count.lock();
                if u64::MAX - *c > v {
                    *c += v;
                    drop(c);
                    self.wake(Events::IN);
                    return Ok(());
                }
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.wait(|| u64::MAX - *self.count.lock() > v)?;
        }
    }

    /// One plain `write` of `len` bytes (the current segment).
    fn write_one(&self, from: &mut dyn IoSource, len: usize, nonblock: bool) -> Result<usize, Errno> {
        if len < 8 {
            return Err(EINVAL);
        }
        let mut b = [0u8; 8];
        if from.copy(&mut b) < 8 {
            return Err(EFAULT);
        }
        self.add(u64::from_le_bytes(b), nonblock).map(|_| 8)
    }
}

/// Ids in use (`eventfd_ida`: `ida_simple_get(.., 0, 0, ..)` hands out the smallest free one).
static IDS: SpinLock<alloc::collections::BTreeSet<u32>> = SpinLock::named("vfs.eventfd.ids", alloc::collections::BTreeSet::new());

fn ids_take() -> u32 {
    let mut s = IDS.lock();
    let id = s.iter().enumerate().find(|&(i, &v)| i as u32 != v).map_or(s.len() as u32, |(i, _)| i as u32);
    s.insert(id);
    id
}

impl File for EventFd {
    /// linux-6.6/fs/eventfd.c:295 `eventfd_show_fdinfo` (`%16llx` pads with spaces).
    fn show_fdinfo(&self, out: &mut alloc::string::String) {
        use core::fmt::Write;
        let c = *self.count.lock();
        let _ = write!(out, "eventfd-count: {c:16x}\neventfd-id: {}\neventfd-semaphore: {}\n", self.id, self.semaphore as u8);
    }
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, true)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write_stream(buf, true)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        let v = self.take(buf.len(), nonblock)?;
        buf[..8].copy_from_slice(&v.to_le_bytes());
        Ok(8)
    }
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        let mut src = crate::node::BufSource::new(buf);
        self.write_one(&mut src, buf.len(), nonblock)
    }
    fn read_iter(&self, to: &mut dyn IoSink, status: u64) -> Option<Result<usize, Errno>> {
        let r = self.take(to.left(), status & abi::O_NONBLOCK != 0).and_then(|v| if to.copy(&v.to_le_bytes()) == 8 { Ok(8) } else { Err(EFAULT) });
        Some(r)
    }
    /// `do_loop_readv_writev`: one `write` per segment, stopping at the first short one.
    fn write_iter(&self, from: &mut dyn IoSource, status: u64) -> Option<Result<usize, Errno>> {
        let nonblock = status & abi::O_NONBLOCK != 0;
        let mut total = 0usize;
        loop {
            let len = from.seg_left();
            match self.write_one(from, len, nonblock) {
                Ok(n) => {
                    total += n;
                    if n != len || from.left() == 0 {
                        return Some(Ok(total));
                    }
                }
                Err(e) => return Some(if total > 0 { Ok(total) } else { Err(e) }),
            }
        }
    }
    /// Anonymous inodes have no type bits.
    fn mode(&self) -> u32 {
        0o600
    }
    fn events(&self) -> Events {
        let c = *self.count.lock();
        let mut m = 0;
        if c > 0 {
            m |= Events::IN;
        }
        if c == u64::MAX {
            m |= Events::ERR;
        }
        if u64::MAX - 1 > c {
            m |= Events::OUT;
        }
        Events(m)
    }
    /// `noop_llseek`: the position (always 0) without an error.
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Ok(0))
    }
    fn attr(&self) -> Option<Attr> {
        Some(anon_attr())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.wq)
    }
    /// eventfd has `f_op->poll` (a nameless anon inode: the default would say no).
    fn can_poll(&self) -> bool {
        true
    }
    /// `eventfd_poll` reports EPOLLIN/EPOLLOUT/EPOLLERR only (no RDNORM/WRNORM).
    fn poll_exact(&self) -> bool {
        true
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:[eventfd]"))
    }
}

impl Drop for EventFd {
    /// `eventfd_release` wakes pollers with EPOLLHUP (an epoll set holding the last reference sees it go).
    fn drop(&mut self) {
        self.wake(Events::HUP);
        IDS.lock().remove(&self.id);
    }
}

impl Pollable for EventFd {
    fn events(&self) -> Events {
        File::events(self)
    }
    fn wait_queue(&self) -> &WaitQueue {
        &self.wq
    }
}

/// eventfd2's open file: `flags` checked by the caller (EFD_FLAGS); F_GETFL = O_RDWR | O_NONBLOCK as given.
pub fn new_eventfd(initial: u32, flags: u64) -> Arc<crate::fd::OpenFile> {
    crate::fd::OpenFile::anon(EventFd::new(initial, flags), abi::O_RDWR | (flags & abi::O_NONBLOCK))
}
