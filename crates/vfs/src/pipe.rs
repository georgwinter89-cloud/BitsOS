//! Pipes and FIFOs (FS-3), after linux-6.6/fs/pipe.c.
//!
//! A `Pipe` is Linux's `pipe_inode_info`: a ring of page buffers (`Buf`, 4 KiB each, filled from offset 0,
//! read from `offset`) of at most `max` slots (`max_usage`; 16 by default = 64 KiB, F_SETPIPE_SZ changes it).
//! The layout follows Linux byte for byte, because programs can see it: a write first merges its last
//! `len % 4096` bytes' worth into the newest buffer when they fit behind its data (`CAN_MERGE`), the rest goes
//! page by page into new slots; so many small writes fill a pipe to exactly 64 KiB, while 100-byte writes stop
//! at 40 per page (64000 bytes). A write of at most PIPE_BUF (4096) bytes is atomic by that construction (it
//! either merges whole or takes one fresh page). O_DIRECT on the writing file makes every write its own
//! packet (`PACKET`): a read takes at most one packet and drops what it does not take.
//!
//! The ends are `PipeEnd`s (one per open file: `struct file` with FMODE_READ/FMODE_WRITE). Readers and writers
//! are counted per end and released when the last reference of an open file goes (`Drop` = `pipe_release`):
//! a read then sees EOF, a write EPIPE plus SIGPIPE (through the hook the process layer installs,
//! `set_sigpipe`). `pipe()` makes an anonymous pipe with its own inode (pipefs: S_IFIFO 0600, the creator's
//! ids); a FIFO (`Fifo`, made by mknod/mkfifo on tmpfs or devtmpfs) makes its pipe at the first open and drops
//! it (data included) when the last end closes, as `fifo_open`/`put_pipe_info`, with Linux's open rules
//! (O_RDONLY waits for a writer unless O_NONBLOCK; O_WRONLY waits for a reader, or ENXIO with O_NONBLOCK;
//! O_RDWR never waits).
//!
//! Locks and waits: `ring` is a sleeping mutex (Linux's `pipe->mutex`) held while bytes are copied to and
//! from the program (lock order `vfs.pipe` > `mm.as`, like `vfs.file.pos`). Waiting happens without it on
//! Linux's two queues (FS-4: `PollQueue`s, so poll/select/epoll hang on them too): `rd` for readers and FIFO
//! openers, woken by writes with the key IN|RDNORM, `wr` for writers, woken by reads of a full pipe with
//! OUT|WRNORM; the last close of a side wakes both without a key. Conditions read the lock-free mirrors
//! `used`/`max`/`readers`/`writers`. Waits
//! run in 50 ms slices and end with EINTR once the program was killed (B4: no unbounded wait; until AB-6
//! delivers signals, a pending signal with a handler does not interrupt).

use crate::abi::{self, Attr};
use crate::inode::{Cred, Inode, Meta};
use crate::node::{BufSink, BufSource, File, IoSink, IoSource};
use crate::poll::{Events, PollQueue, Pollable};
use crate::Errno;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ksync::{Mutex, SpinLock, WaitQueue};
use uapi::errno::*;

/// `PIPE_BUF`: writes up to this size are atomic.
pub const PIPE_BUF: usize = 4096;
const PAGE: usize = 4096;
/// `PIPE_DEF_BUFFERS`: a new pipe holds 16 pages (64 KiB).
pub const DEF_BUFFERS: usize = 16;
/// `/proc/sys/fs/pipe-max-size` at boot (Linux's default): the largest F_SETPIPE_SZ without CAP_SYS_RESOURCE.
pub const MAX_SIZE: u64 = 1 << 20;
/// `/proc/sys/fs/pipe-user-pages-soft` and `-hard` at boot (Linux's defaults; stored, not enforced: R1, notes).
pub const USER_PAGES_SOFT: u64 = 16 * 1024;
pub const USER_PAGES_HARD: u64 = 0;

/// `pipe_max_size` (root may change it through /proc/sys/fs/pipe-max-size; always a power of two >= a page).
static PIPE_MAX_SIZE: AtomicU64 = AtomicU64::new(MAX_SIZE);
static USER_PAGES_SOFT_NOW: AtomicU64 = AtomicU64::new(USER_PAGES_SOFT);
static USER_PAGES_HARD_NOW: AtomicU64 = AtomicU64::new(USER_PAGES_HARD);

pub fn max_size() -> u64 {
    PIPE_MAX_SIZE.load(Ordering::Relaxed)
}

/// A write of pipe-max-size (`do_proc_dopipe_max_size_conv`): the value as an unsigned int through
/// `round_pipe_size`; 0 there (above 2^31) is EINVAL.
pub fn set_max_size(v: u64) -> Result<(), Errno> {
    let size = round_pipe_size(v as u32).ok_or(EINVAL)?;
    let old = PIPE_MAX_SIZE.swap(size, Ordering::Relaxed);
    klog::event!(Info, "vfs", "pipe.max_size", from = old, to = size);
    Ok(())
}

/// `/proc/sys/fs/pipe-user-pages-hard` (`hard`) or `-soft`: any unsigned long, stored and shown.
pub(crate) fn user_pages_knob(hard: bool) -> crate::sysctl::Knob {
    if hard {
        crate::sysctl::Knob {
            get: || USER_PAGES_HARD_NOW.load(Ordering::Relaxed),
            set: |v| {
                USER_PAGES_HARD_NOW.store(v, Ordering::Relaxed);
                Ok(())
            },
            uint: false,
        }
    } else {
        crate::sysctl::Knob {
            get: || USER_PAGES_SOFT_NOW.load(Ordering::Relaxed),
            set: |v| {
                USER_PAGES_SOFT_NOW.store(v, Ordering::Relaxed);
                Ok(())
            },
            uint: false,
        }
    }
}

/// `round_pipe_size`: None above 2^31, at least a page, a power of two.
fn round_pipe_size(v: u32) -> Option<u64> {
    if v > 1 << 31 {
        return None;
    }
    Some((v as u64).max(PAGE as u64).next_power_of_two())
}

/// Slots of a new pipe (`alloc_pipe_info`): 16, or fewer when pipe-max-size is below 64 KiB and the creator is
/// not privileged.
pub(crate) fn initial_slots(max_size: u64, privileged: bool) -> usize {
    if (DEF_BUFFERS * PAGE) as u64 > max_size && !privileged { (max_size / PAGE as u64).max(1) as usize } else { DEF_BUFFERS }
}

/// One wait slice: a killed program leaves a blocked read, write or FIFO open within this time.
const SLICE_NS: u64 = 50_000_000;

pub const F_SETPIPE_SZ: u64 = 1031;
pub const F_GETPIPE_SZ: u64 = 1032;
/// `O_NOTIFICATION_PIPE` (= O_EXCL): pipe2 accepts the bit; without watch queues (CONFIG_WATCH_QUEUE) the
/// answer is ENOPKG, as such a Linux kernel gives.
pub const O_NOTIFICATION_PIPE: u64 = abi::O_EXCL;
const ENOPKG: Errno = 65;

klog::counter!(PIPES, "vfs.pipes");
klog::counter!(EPIPES, "vfs.pipe.epipe");
klog::counter!(NO_MEMORY, "vfs.pipe.no_memory");

/// SIGPIPE for the calling thread (the process layer installs it: vfs-sys; AB-6 delivers it to a handler).
static SIGPIPE: ksync::Once<fn()> = ksync::Once::new();

/// Installs the SIGPIPE hook (the first call wins).
pub fn set_sigpipe(f: fn()) {
    SIGPIPE.get_or_init(|| f);
}

fn send_sigpipe() {
    EPIPES.inc();
    if let Some(f) = SIGPIPE.get() {
        f();
    }
}

/// pipefs: one anonymous device for every pipe's inode; inode numbers from Linux's `get_next_ino` series.
static PIPEFS_DEV: ksync::Once<u64> = ksync::Once::new();
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

fn pipefs_dev() -> u64 {
    *PIPEFS_DEV.get_or_init(crate::dentry::anon_dev)
}

/// One page of a pipe.
struct Buf {
    page: Vec<u8>,
    offset: usize,
    len: usize,
    /// `PIPE_BUF_FLAG_CAN_MERGE`: a later write may append behind the data.
    merge: bool,
    /// `PIPE_BUF_FLAG_PACKET` (O_DIRECT writer): a read takes it whole or the part that fits, never more.
    packet: bool,
}

struct Ring {
    bufs: VecDeque<Buf>,
    /// `max_usage`: slots a writer may fill.
    max: usize,
    /// `tmp_page`: a page kept from the last emptied buffer for the next write.
    spare: Option<Vec<u8>>,
}

impl Ring {
    fn full(&self) -> bool {
        self.bufs.len() >= self.max
    }
    fn bytes(&self) -> usize {
        self.bufs.iter().map(|b| b.len).sum()
    }
    fn page(&mut self) -> Option<Vec<u8>> {
        if let Some(p) = self.spare.take() {
            return Some(p);
        }
        let mut p = Vec::new();
        p.try_reserve_exact(PAGE).ok()?;
        p.resize(PAGE, 0);
        Some(p)
    }
}

pub struct Pipe {
    ring: Mutex<Ring>,
    /// Mirrors of the ring for conditions and poll (written under `ring`).
    used: AtomicUsize,
    max: AtomicUsize,
    readers: AtomicU32,
    writers: AtomicU32,
    /// Opens for reading / writing ever (`r_counter`, `w_counter`): a FIFO opener waits for a change.
    r_counter: AtomicU32,
    w_counter: AtomicU32,
    /// Linux's `rd_wait`: readers, FIFO openers, and poll/epoll of ends that read (FS-4).
    rd: PollQueue,
    /// Linux's `wr_wait`: writers waiting for room, and poll/epoll of ends that write.
    wr: PollQueue,
    /// The pipefs inode of an anonymous pipe (fstat, file times). A FIFO's inode is its tmpfs node.
    meta: Option<Meta>,
    /// A FIFO's tmpfs inode: a write updates its times (`file_update_time`).
    node: Option<Weak<Inode>>,
}

impl Pipe {
    fn new(meta: Option<Meta>, node: Option<Weak<Inode>>, privileged: bool) -> Arc<Pipe> {
        PIPES.inc();
        let slots = initial_slots(max_size(), privileged);
        Arc::new(Pipe {
            ring: Mutex::named("vfs.pipe", Ring { bufs: VecDeque::new(), max: slots, spare: None }),
            used: AtomicUsize::new(0),
            max: AtomicUsize::new(slots),
            readers: AtomicU32::new(0),
            writers: AtomicU32::new(0),
            r_counter: AtomicU32::new(1),
            w_counter: AtomicU32::new(1),
            rd: PollQueue::new(),
            wr: PollQueue::new(),
            meta,
            node,
        })
    }

    /// Readers (and FIFO openers) may go on: `wake_up_interruptible_sync_poll(&pipe->rd_wait, EPOLLIN |
    /// EPOLLRDNORM)`.
    fn wake_readers(&self) {
        self.rd.wake_poll(Events::IN | Events::RDNORM);
    }
    /// Writers may go on: `wake_up_interruptible_sync_poll(&pipe->wr_wait, EPOLLOUT | EPOLLWRNORM)`.
    fn wake_writers(&self) {
        self.wr.wake_poll(Events::OUT | Events::WRNORM);
    }

    fn is_empty(&self) -> bool {
        self.used.load(Ordering::SeqCst) == 0
    }
    fn is_full(&self) -> bool {
        self.used.load(Ordering::SeqCst) >= self.max.load(Ordering::SeqCst)
    }
    /// `pipe_readable`: data, or no writer left.
    fn readable(&self) -> bool {
        !self.is_empty() || self.writers.load(Ordering::SeqCst) == 0
    }
    /// `pipe_writable`: a free slot, or no reader left.
    fn writable(&self) -> bool {
        !self.is_full() || self.readers.load(Ordering::SeqCst) == 0
    }

    /// Some end is open (a FIFO's pipe without one is dead: the next open makes a new one).
    fn live(&self) -> bool {
        self.readers.load(Ordering::SeqCst) != 0 || self.writers.load(Ordering::SeqCst) != 0
    }

    fn sync_mirror(&self, r: &Ring) {
        self.used.store(r.bufs.len(), Ordering::SeqCst);
        self.max.store(r.max, Ordering::SeqCst);
    }

    /// Sleeps on `q` (in slices) until `cond`; EINTR when the program was killed meanwhile.
    fn wait(q: &PollQueue, cond: impl Fn() -> bool) -> Result<(), Errno> {
        loop {
            if cond() {
                return Ok(());
            }
            if crate::dentry::interrupted() {
                return Err(EINTR);
            }
            q.wait_until_timeout(&cond, SLICE_NS);
        }
    }

    /// Bytes buffered now (FIONREAD).
    pub fn bytes(&self) -> usize {
        self.ring.lock().bytes()
    }

    /// Capacity in bytes (F_GETPIPE_SZ).
    pub fn capacity(&self) -> usize {
        self.max.load(Ordering::SeqCst) * PAGE
    }

    /// `pipe_read`.
    fn read(&self, to: &mut dyn IoSink, nonblock: bool) -> Result<usize, Errno> {
        if to.left() == 0 {
            return Ok(0);
        }
        let mut ring = self.ring.lock();
        let mut was_full = ring.full();
        loop {
            let mut ret = 0usize;
            let mut fault = false;
            let mut total = to.left();
            while let Some(b) = ring.bufs.front_mut() {
                let chars = b.len.min(total);
                let got = to.copy(&b.page[b.offset..b.offset + chars]);
                if got < chars {
                    fault = true;
                    break;
                }
                ret += chars;
                b.offset += chars;
                b.len -= chars;
                if b.packet {
                    // A packet is read once: what did not fit is dropped.
                    total = chars;
                    b.len = 0;
                }
                if b.len == 0 {
                    if let Some(old) = ring.bufs.pop_front() {
                        ring.spare.get_or_insert(old.page);
                    }
                }
                total -= chars;
                if total == 0 {
                    break;
                }
            }
            self.sync_mirror(&ring);
            if fault && ret == 0 {
                drop(ring);
                if was_full {
                    self.wake_writers();
                }
                return Err(EFAULT);
            }
            if ret > 0 || fault || self.writers.load(Ordering::SeqCst) == 0 {
                drop(ring);
                if was_full {
                    self.wake_writers();
                }
                return Ok(ret);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            drop(ring);
            // Nothing read: writers stuck on a full pipe (a zero-length buffer was all there was) may go on.
            if was_full {
                self.wake_writers();
            }
            Self::wait(&self.rd, || self.readable())?;
            ring = self.ring.lock();
            was_full = ring.full();
        }
    }

    /// `pipe_write`: `packet` = O_DIRECT on the writing file.
    fn write(&self, from: &mut dyn IoSource, nonblock: bool, packet: bool) -> Result<usize, Errno> {
        let total = from.left();
        if total == 0 {
            return Ok(0);
        }
        let mut ring = self.ring.lock();
        if self.readers.load(Ordering::SeqCst) == 0 {
            drop(ring);
            send_sigpipe();
            return Err(EPIPE);
        }
        let mut ret: Result<usize, Errno> = Ok(0);
        let mut sigpipe = false;
        let mut done = 0usize;
        // Merge the odd part into the newest buffer when it fits behind its data.
        let chars = total & (PAGE - 1);
        let mut finished = false;
        if chars != 0
            && let Some(b) = ring.bufs.back_mut()
        {
            let at = b.offset + b.len;
            if b.merge && at + chars <= PAGE {
                let got = from.copy(&mut b.page[at..at + chars]);
                if got < chars {
                    ret = Err(EFAULT);
                    finished = true;
                } else {
                    b.len += got;
                    done = got;
                    finished = from.left() == 0;
                }
            }
        }
        while !finished {
            if self.readers.load(Ordering::SeqCst) == 0 {
                sigpipe = true;
                if done == 0 {
                    ret = Err(EPIPE);
                }
                break;
            }
            if !ring.full() {
                let Some(page) = ring.page() else {
                    NO_MEMORY.inc();
                    klog::event!(Warn, "vfs", "pipe.no_memory", written = done; "no page for the pipe: the write stops");
                    if done == 0 {
                        ret = Err(ENOMEM);
                    }
                    break;
                };
                if ring.bufs.try_reserve(1).is_err() {
                    ring.spare = Some(page);
                    NO_MEMORY.inc();
                    if done == 0 {
                        ret = Err(ENOMEM);
                    }
                    break;
                }
                // The mirrors move after the copy (at the end, or before a wait): poll never sees a slot whose
                // bytes are still being copied.
                ring.bufs.push_back(Buf { page, offset: 0, len: 0, merge: !packet, packet });
                let want = from.left().min(PAGE);
                let copied = match ring.bufs.back_mut() {
                    Some(b) => from.copy(&mut b.page[..want]),
                    None => 0,
                };
                if copied < want {
                    // Linux keeps the slot with length 0 and drops the partial page.
                    if done == 0 {
                        ret = Err(EFAULT);
                    }
                    break;
                }
                if let Some(b) = ring.bufs.back_mut() {
                    b.len = copied;
                }
                done += copied;
                if from.left() == 0 {
                    break;
                }
                continue;
            }
            if nonblock {
                if done == 0 {
                    ret = Err(EAGAIN);
                }
                break;
            }
            if crate::dentry::interrupted() {
                if done == 0 {
                    ret = Err(EINTR);
                }
                break;
            }
            self.sync_mirror(&ring);
            drop(ring);
            // Readers drain what is there while this writer waits for room.
            self.wake_readers();
            if let Err(e) = Self::wait(&self.wr, || self.writable()) {
                if done == 0 {
                    ret = Err(e);
                }
                ring = self.ring.lock();
                break;
            }
            ring = self.ring.lock();
        }
        self.sync_mirror(&ring);
        drop(ring);
        // Every write wakes the readers. Deliberate: Linux wakes on every write too once poll/epoll looked at
        // the pipe (`was_empty || pipe->poll_usage`, linux-6.6/fs/pipe.c:594), and only then can anyone see the edge
        // (EPOLLET counts one per write, as on Linux).
        self.wake_readers();
        if done > 0 {
            self.touch_times();
        }
        if sigpipe {
            send_sigpipe();
        }
        if done > 0 { Ok(done) } else { ret.map(|_| 0) }
    }

    fn touch_times(&self) {
        if let Some(m) = &self.meta {
            m.touch_mtime();
        } else if let Some(i) = self.node.as_ref().and_then(|w| w.upgrade()) {
            i.meta.touch_mtime();
        }
    }

    /// `pipe_set_size`: `arg` as fcntl's int; `privileged` = CAP_SYS_RESOURCE (uid 0 here).
    fn set_size(&self, arg: u32, privileged: bool) -> Result<i64, Errno> {
        let size = round_pipe_size(arg).ok_or(EINVAL)?;
        let slots = (size / PAGE as u64) as usize;
        let mut ring = self.ring.lock();
        if slots > ring.max && size > max_size() && !privileged {
            return Err(EPERM);
        }
        // pipe_resize_ring: never below what is buffered.
        if slots < ring.bufs.len() {
            return Err(EBUSY);
        }
        let old = ring.max;
        ring.max = slots;
        self.sync_mirror(&ring);
        drop(ring);
        if slots != old {
            klog::event!(Debug, "vfs", "pipe.resize", from_bytes = old * PAGE, to_bytes = slots * PAGE);
        }
        // More room for writers (`pipe_resize_ring`: `wake_up_interruptible(&pipe->wr_wait)`).
        self.wr.wake_all();
        Ok((slots * PAGE) as i64)
    }
}

/// One open end (`struct file` of a pipe or FIFO): its mode decides what it counts as and what poll shows.
pub struct PipeEnd {
    pipe: Arc<Pipe>,
    read: bool,
    write: bool,
    /// `f_version`: a FIFO opened O_RDONLY|O_NONBLOCK without a writer shows no POLLHUP until a writer came.
    version: u32,
}

impl PipeEnd {
    fn new(pipe: Arc<Pipe>, read: bool, write: bool) -> PipeEnd {
        // Counted before anything can see the end, released by Drop (`pipe_release`).
        if read {
            pipe.readers.fetch_add(1, Ordering::SeqCst);
        }
        if write {
            pipe.writers.fetch_add(1, Ordering::SeqCst);
        }
        PipeEnd { pipe, read, write, version: 0 }
    }
    pub fn pipe(&self) -> &Arc<Pipe> {
        &self.pipe
    }
    fn status_nonblock(status: u64) -> bool {
        status & abi::O_NONBLOCK != 0
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        let p = &self.pipe;
        let r = if self.read { p.readers.fetch_sub(1, Ordering::SeqCst) - 1 } else { p.readers.load(Ordering::SeqCst) };
        let w = if self.write { p.writers.fetch_sub(1, Ordering::SeqCst) - 1 } else { p.writers.load(Ordering::SeqCst) };
        // The last reader or writer, but not the other side: blocked readers see EOF, writers EPIPE, poll HUP/ERR.
        if (r == 0) != (w == 0) || (r == 0 && w == 0) {
            p.rd.wake_all();
            p.wr.wake_all();
        }
    }
}

impl File for PipeEnd {
    /// Kernel reads without a position: never wait.
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, true)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write_stream(buf, true)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        if !self.read {
            return Err(EBADF);
        }
        self.pipe.read(&mut BufSink::new(buf), nonblock)
    }
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        if !self.write {
            return Err(EBADF);
        }
        self.pipe.write(&mut BufSource::new(buf), nonblock, false)
    }
    fn read_iter(&self, to: &mut dyn IoSink, status: u64) -> Option<Result<usize, Errno>> {
        Some(if self.read { self.pipe.read(to, Self::status_nonblock(status)) } else { Err(EBADF) })
    }
    fn write_iter(&self, from: &mut dyn IoSource, status: u64) -> Option<Result<usize, Errno>> {
        if !self.write {
            return Some(Err(EBADF));
        }
        Some(self.pipe.write(from, Self::status_nonblock(status), status & abi::O_DIRECT != 0))
    }
    fn mode(&self) -> u32 {
        uapi::S_IFIFO | 0o600
    }
    /// `pipe_poll`.
    fn events(&self) -> Events {
        let p = &self.pipe;
        let mut m = 0;
        if self.read {
            if !p.is_empty() {
                m |= Events::IN;
            }
            if p.writers.load(Ordering::SeqCst) == 0 && self.version != p.w_counter.load(Ordering::SeqCst) {
                m |= Events::HUP;
            }
        }
        if self.write {
            if !p.is_full() {
                m |= Events::OUT;
            }
            // Linux reports POLLERR for FIFOs as for pipes when no reader is left.
            if p.readers.load(Ordering::SeqCst) == 0 {
                m |= Events::ERR;
            }
        }
        Events(m)
    }
    /// `pipe_ioctl`: FIONREAD (the bytes of every buffer); anything else ENOTTY.
    fn ioctl(&self, cmd: u64, _arg: u64) -> Result<Vec<u8>, Errno> {
        match cmd {
            uapi::ioctl::FIONREAD => Ok((self.pipe.bytes() as i32).to_le_bytes().to_vec()),
            _ => Err(ENOTTY),
        }
    }
    /// `pipe_fcntl`.
    fn fcntl(&self, cmd: u64, arg: u64, cred: Cred) -> Option<Result<i64, Errno>> {
        match cmd {
            F_SETPIPE_SZ => Some(self.pipe.set_size(arg as u32, cred.uid == 0)),
            F_GETPIPE_SZ => Some(Ok(self.pipe.capacity() as i64)),
            _ => None,
        }
    }
    fn attr(&self) -> Option<Attr> {
        self.pipe.meta.as_ref().map(|m| m.attr(pipefs_dev()))
    }
    fn meta(&self) -> Option<&crate::inode::Meta> {
        self.pipe.meta.as_ref()
    }
    /// The queue of this end's direction (a read end: `rd_wait`).
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(if self.read { &self.pipe.rd } else { &self.pipe.wr })
    }
    /// `pipe_poll`: `poll_wait` on `rd_wait` for an end that reads, on `wr_wait` for one that writes.
    fn poll_wait(&self, add: &mut dyn FnMut(&PollQueue)) {
        if self.read {
            add(&self.pipe.rd);
        }
        if self.write {
            add(&self.pipe.wr);
        }
    }
    fn link_name(&self) -> Option<String> {
        self.pipe.meta.as_ref().map(|m| format!("pipe:[{}]", m.ino))
    }
    /// `fifo_open` on this pipe's inode (AB-7a CR8): a new end with the access asked for, waiting for a partner
    /// as a FIFO open does. The pipe is live while this end exists, so the open never makes a new one.
    fn reopen(&self, flags: u64, privileged: bool) -> Option<Result<Arc<dyn File>, Errno>> {
        let node = Fifo { pipe: SpinLock::named("vfs.fifo", Arc::downgrade(&self.pipe)), inode: SpinLock::named("vfs.fifo.inode", Weak::new()) };
        Some(node.open(flags, privileged))
    }
}

impl Pollable for PipeEnd {
    fn events(&self) -> Events {
        File::events(self)
    }
    fn wait_queue(&self) -> &WaitQueue {
        if self.read { &self.pipe.rd } else { &self.pipe.wr }
    }
}

/// `pipe2`'s two open files: (read end, write end). `flags` are pipe2's (checked by the caller); the read
/// end gets O_NONBLOCK, the write end O_NONBLOCK and O_DIRECT (`create_pipe_files`). Neither has
/// O_LARGEFILE (pseudo files never get it).
pub fn new_pipe(cred: Cred, flags: u64) -> Result<(Arc<crate::fd::OpenFile>, Arc<crate::fd::OpenFile>), Errno> {
    if flags & O_NOTIFICATION_PIPE != 0 {
        return Err(ENOPKG);
    }
    let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
    let pipe = Pipe::new(Some(Meta::new(ino, uapi::S_IFIFO | 0o600, cred)), None, cred.uid == 0);
    let r = PipeEnd::new(pipe.clone(), true, false);
    let w = PipeEnd::new(pipe, false, true);
    let rf = crate::fd::OpenFile::anon(Arc::new(r), abi::O_RDONLY | (flags & abi::O_NONBLOCK));
    let wf = crate::fd::OpenFile::anon(Arc::new(w), abi::O_WRONLY | (flags & (abi::O_NONBLOCK | abi::O_DIRECT)));
    Ok((rf, wf))
}

/// Two kernel-held ends of a new pipe (ktests, kernel users): (read end, write end) as plain files.
pub fn kernel_pipe() -> (Arc<PipeEnd>, Arc<PipeEnd>) {
    let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
    let pipe = Pipe::new(Some(Meta::new(ino, uapi::S_IFIFO | 0o600, Cred::ROOT)), None, true);
    (Arc::new(PipeEnd::new(pipe.clone(), true, false)), Arc::new(PipeEnd::new(pipe, false, true)))
}

/// A FIFO node (mknod S_IFIFO, mkfifo): its pipe exists while an end is open (`inode->i_pipe`).
pub struct Fifo {
    pipe: SpinLock<Weak<Pipe>>,
    inode: SpinLock<Weak<Inode>>,
}

impl Fifo {
    pub fn new() -> Self {
        Fifo { pipe: SpinLock::named("vfs.fifo", Weak::new()), inode: SpinLock::named("vfs.fifo.inode", Weak::new()) }
    }
    /// The node's inode (times of writes go there).
    pub fn attach(&self, inode: &Arc<Inode>) {
        *self.inode.lock() = Arc::downgrade(inode);
    }
    /// The pipe while any end is open.
    pub fn current(&self) -> Option<Arc<Pipe>> {
        self.pipe.lock().upgrade().filter(|p| p.live())
    }

    /// One more end on the node's pipe (`fifo_open` up to the counting). Ends are counted only here, under the
    /// slot lock, so a pipe found with no end left is dead even while its last end is still being dropped (or a
    /// kernel user holds it): its data went with that close (`put_pipe_info`) and this opener gets a fresh pipe.
    /// `refuse_without_reader` (O_WRONLY|O_NONBLOCK): ENXIO, decided before anything is counted.
    fn open_end(&self, read: bool, write: bool, refuse_without_reader: bool, privileged: bool) -> Result<PipeEnd, Errno> {
        // The inode is read before the pipe slot is locked: the two locks are never nested.
        let node = self.inode.lock().clone();
        let mut g = self.pipe.lock();
        let (pipe, dead) = match g.upgrade() {
            Some(p) if p.live() => (p, None),
            old => {
                let p = Pipe::new(None, Some(node), privileged);
                *g = Arc::downgrade(&p);
                (p, old)
            }
        };
        let end = if refuse_without_reader && pipe.readers.load(Ordering::SeqCst) == 0 {
            Err(ENXIO)
        } else {
            Ok(PipeEnd::new(pipe.clone(), read, write))
        };
        drop(g);
        // A dead pipe's buffers (if this held its last reference) and an unused new pipe are freed unlocked.
        drop((dead, pipe));
        end
    }

    /// `fifo_open`. `privileged`: the opener may exceed pipe-max-size (a new pipe's size).
    pub fn open(&self, flags: u64, privileged: bool) -> Result<Arc<dyn File>, Errno> {
        let (read, write) = match flags & abi::O_ACCMODE {
            abi::O_RDONLY => (true, false),
            abi::O_WRONLY => (false, true),
            abi::O_RDWR => (true, true),
            _ => return Err(EINVAL),
        };
        let nonblock = flags & abi::O_NONBLOCK != 0;
        let mut end = self.open_end(read, write, write && !read && nonblock, privileged)?;
        let pipe = end.pipe.clone();
        // Order (Linux does all of this under the pipe mutex): this side is counted before its counter moves,
        // and a waiter reads the other side's counter before it looks at the other side's count. So a partner
        // that opens meanwhile is either seen as present or moves the counter the waiter watches.
        if read {
            pipe.r_counter.fetch_add(1, Ordering::SeqCst);
        }
        if write {
            pipe.w_counter.fetch_add(1, Ordering::SeqCst);
        }
        // wake_up_partner: an opener waiting for this side goes on (openers of both sides wait on rd_wait).
        pipe.rd.wake_all();
        if read && !write {
            let seen = pipe.w_counter.load(Ordering::SeqCst);
            if pipe.writers.load(Ordering::SeqCst) == 0 {
                if nonblock {
                    // Suppress POLLHUP until a writer has come.
                    end.version = seen;
                } else if let Err(e) = Pipe::wait(&pipe.rd, || pipe.w_counter.load(Ordering::SeqCst) != seen) {
                    klog::event!(Debug, "vfs", "fifo.open_interrupted", side = "read");
                    return Err(e);
                }
            }
        }
        if write && !read {
            let seen = pipe.r_counter.load(Ordering::SeqCst);
            if pipe.readers.load(Ordering::SeqCst) == 0
                && let Err(e) = Pipe::wait(&pipe.rd, || pipe.r_counter.load(Ordering::SeqCst) != seen)
            {
                klog::event!(Debug, "vfs", "fifo.open_interrupted", side = "write");
                return Err(e);
            }
        }
        Ok(Arc::new(end))
    }
}

impl Default for Fifo {
    fn default() -> Self {
        Self::new()
    }
}

impl File for Fifo {
    /// The node itself is not an open file (kernel users that look at it without opening get nothing).
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFIFO
    }
    fn open_node(&self, flags: u64, who: Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        Some(self.open(flags, who.uid == 0))
    }
}
