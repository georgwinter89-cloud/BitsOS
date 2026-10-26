//! Sockets for programs (NW-5): the objects behind the socket syscalls, written against the VFS's `File` and
//! `Pollable` so read/write/poll/close work on them like on any file.
//!
//! - [`unix`]: AF_UNIX stream, datagram and seqpacket sockets with path and abstract names, socketpair,
//!   SCM_RIGHTS (open files in flight, closed with the socket that holds them, cycles collected by [`unix_gc`])
//!   and SCM_CREDENTIALS / SO_PEERCRED, with Linux 6.6's semantics (linux-6.6/net/unix/af_unix.c).
//! - [`inet`]: AF_INET TCP and UDP on the kernel's smoltcp stack and its 127.0.0.1 loopback, with
//!   `policy::check` at connect/bind/listen and a `ConnRecord` per connection (B3); AF_INET6 on the same
//!   machinery (its `inet6` part: ::1 and v4-mapped peers, no IPv6 on the wire).
//! - [`netlink`]: AF_NETLINK NETLINK_ROUTE (NW-6) with [`rtnl`]'s link and address dumps for `getifaddrs`;
//!   [`devioctl`]: the SIOCGIF* interface ioctls every socket answers. Both show [`crate::ifview`].
//! - [`sys`]: the syscalls on an abstract user memory (`vfs::sys::Sys`), host-tested; `crates/syscall`'s
//!   `socket.rs` registers them.
//!
//! An fd's `File` is a [`SockFile`]; dropping it (the last open file description going away) is Linux's
//! `sock_release`. The syscalls find the socket behind an `Arc<dyn File>` through a table keyed by the object's
//! address (the VFS `File` trait has no downcast; change request to FS).

pub mod abi;
pub mod addr;
pub mod cmsg;
pub mod devioctl;
pub mod inet;
pub mod netlink;
pub mod nlmsg;
pub mod rtnl;
pub mod sys;
pub mod unix;
pub mod unix_gc;

#[cfg(test)]
mod tests;

use abi::*;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use ksync::{RwSpinLock, WaitQueue};
use uapi::errno::*;
use vfs::{Errno, Events, File, Pollable};

/// Who makes a socket call (credentials for SO_PEERCRED / SCM_CREDENTIALS, the B3 subject).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    pub subject: policy::Subject,
}

impl Caller {
    pub fn ucred(&self) -> Ucred {
        Ucred { pid: self.pid, uid: self.uid, gid: self.gid }
    }
}

/// `struct ucred` (pid, uid, gid).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Ucred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Ucred {
    pub fn bytes(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&self.pid.to_le_bytes());
        b[4..8].copy_from_slice(&self.uid.to_le_bytes());
        b[8..12].copy_from_slice(&self.gid.to_le_bytes());
        b
    }
}

/// What the kernel provides the socket layer from above it (proc, signals): installed once by the syscall glue.
pub struct Hooks {
    /// The calling program was told to end (kill): blocking calls give up with EINTR.
    pub interrupted: fn() -> bool,
    /// SIGPIPE to the calling program (a stream write to a closed peer without MSG_NOSIGNAL).
    pub sigpipe: fn(),
    /// Whether a process with this pid exists (SCM_CREDENTIALS of another pid, sent by root).
    pub pid_exists: fn(u32) -> bool,
    /// The calling program (write(2) on a socket attaches its credentials when a side has SO_PASSCRED).
    pub current: fn() -> Option<Caller>,
}

static HOOKS: ksync::OnceRef<Hooks> = ksync::OnceRef::new();

pub fn set_hooks(h: &'static Hooks) {
    HOOKS.set(h);
}

pub(crate) fn interrupted() -> bool {
    HOOKS.get().is_some_and(|h| (h.interrupted)())
}

pub(crate) fn sigpipe() {
    if let Some(h) = HOOKS.get() {
        (h.sigpipe)();
    }
}

pub(crate) fn pid_exists(pid: u32) -> bool {
    HOOKS.get().map_or(true, |h| (h.pid_exists)(pid))
}

/// The calling program, or the kernel itself (pid 0, root).
pub(crate) fn current() -> Caller {
    HOOKS.get().and_then(|h| (h.current)()).unwrap_or(Caller { pid: 0, uid: 0, gid: 0, subject: policy::Subject::KERNEL })
}

/// Monotonic nanoseconds (the kernel clock; the process clock in host tests).
pub(crate) fn now() -> u64 {
    #[cfg(target_os = "none")]
    {
        time::now_ns()
    }
    #[cfg(not(target_os = "none"))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
    }
}

/// A blocking call looks at the kill flag at least this often.
const SLICE_NS: u64 = 50_000_000;

/// How long a blocking call may wait: `None` without end, `Some(0)` not at all.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline(Option<u64>);

impl Deadline {
    /// From a socket timeout (SO_RCVTIMEO/SO_SNDTIMEO, 0 = none) and the call's non-blocking flag.
    pub fn new(nonblock: bool, timeo_ns: u64) -> Deadline {
        if nonblock {
            Deadline(Some(0))
        } else if timeo_ns == 0 {
            Deadline(None)
        } else {
            Deadline(Some(now().saturating_add(timeo_ns)))
        }
    }
    pub fn nonblocking(&self) -> bool {
        self.0 == Some(0)
    }
    pub fn expired(&self) -> bool {
        self.0.is_some_and(|d| d == 0 || now() >= d)
    }
}

/// Waits on `wq` until `ready()`: Ok(true) ready, Ok(false) the deadline passed, Err(EINTR) the program was
/// killed. Never waits without a slice, so a kill is seen within `SLICE_NS` (B4).
pub(crate) fn wait(wq: &WaitQueue, dl: Deadline, mut ready: impl FnMut() -> bool) -> Result<bool, Errno> {
    loop {
        if ready() {
            return Ok(true);
        }
        if dl.expired() {
            return Ok(false);
        }
        if interrupted() {
            return Err(EINTR);
        }
        let slice = match dl.0 {
            Some(d) => d.saturating_sub(now()).clamp(1, SLICE_NS),
            None => SLICE_NS,
        };
        wq.wait_until_timeout(&mut ready, slice);
    }
}

/// NW-7 fixer: a waiter's memory of its connection's change counter (`Conn::change_seq`). Every AF_INET waiter
/// sleeps on the one `inet::READY` (Linux: on the socket's own wait queue, linux-6.6/net/core/sock.c:3315
/// `sock_def_readable`), so each wake meant for another socket would make every waiter take the card's lock to
/// look: the thundering herd NW-7 removed for kernel connections. While the counter stands still the look is
/// skipped, except once per `SLICE_NS` (B4: a change nobody reported is still seen).
pub(crate) struct Quiet {
    seen: core::cell::Cell<Option<u64>>,
    at: core::cell::Cell<u64>,
}

impl Quiet {
    pub(crate) fn new() -> Self {
        Quiet { seen: core::cell::Cell::new(None), at: core::cell::Cell::new(0) }
    }

    /// `look()` unless nothing can have changed since the last look that said "not ready"; `seq` is read before
    /// the look it guards. None (loopback, kernel-only objects): always look. A "ready" answer is never reused
    /// (the wait helpers ask again after a wait that ended ready).
    pub(crate) fn look(&self, seq: Option<u64>, look: impl FnOnce() -> bool) -> bool {
        if let Some(seq) = seq {
            let t = now();
            if self.seen.get() == Some(seq) && t.saturating_sub(self.at.get()) < SLICE_NS {
                return false;
            }
            self.seen.set(Some(seq));
            self.at.set(t);
        }
        let ready = look();
        if ready {
            self.seen.set(None);
        }
        ready
    }
}

// ---------------------------------------------------------------------------------------------------------
// SOL_SOCKET options every socket has

/// SOL_SOCKET options (`struct sock` fields of Linux). Values as getsockopt shows them.
pub struct Opts {
    pub sndbuf: AtomicU32,
    pub rcvbuf: AtomicU32,
    pub rcvlowat: AtomicU32,
    /// SO_RCVTIMEO / SO_SNDTIMEO in ns, 0 = none (Linux rounds to its tick: 1 ms on CachyOS, HZ=1000).
    pub rcvtimeo: AtomicU64,
    pub sndtimeo: AtomicU64,
    pub priority: AtomicU32,
    /// SO_LINGER: bit 63 = on, low bits = seconds.
    pub linger: AtomicU64,
    /// SO_BUSY_POLL in µs (stored; V2 does not busy-poll).
    pub busy_poll: AtomicU32,
    /// SO_INCOMING_CPU (`sk_incoming_cpu`): -1 until a program sets it (V2 does not record the receiving CPU).
    pub incoming_cpu: AtomicI32,
    /// SO_BINDTODEVICE (`sk_bound_dev_if`): the interface index, 0 = none. TCP connects and UDP sends route only
    /// over that card (WL-4 CR 2: `route::pick_on`).
    pub bound_dev: AtomicU32,
    flags: AtomicU32,
}

/// Bits of `Opts::flags`.
pub mod flag {
    pub const REUSEADDR: u32 = 1;
    pub const REUSEPORT: u32 = 2;
    pub const KEEPALIVE: u32 = 4;
    pub const BROADCAST: u32 = 8;
    pub const PASSCRED: u32 = 16;
    pub const DONTROUTE: u32 = 32;
    pub const OOBINLINE: u32 = 64;
    pub const NO_CHECK: u32 = 128;
    pub const DEBUG: u32 = 256;
    pub const NODELAY: u32 = 512;
    pub const CORK: u32 = 1024;
    pub const QUICKACK: u32 = 2048;
    /// SO_TIMESTAMP* (`SOCK_RCVTSTAMP`, `SOCK_RCVTSTAMPNS`, `SOCK_TSTAMP_NEW`): stored and shown by getsockopt;
    /// V2 attaches no SCM_TIMESTAMP control message yet.
    pub const RCVTSTAMP: u32 = 4096;
    pub const RCVTSTAMPNS: u32 = 8192;
    pub const TSTAMP_NEW: u32 = 16384;
}

impl Opts {
    pub fn new(sndbuf: u32, rcvbuf: u32) -> Self {
        Opts {
            sndbuf: AtomicU32::new(sndbuf),
            rcvbuf: AtomicU32::new(rcvbuf),
            rcvlowat: AtomicU32::new(1),
            rcvtimeo: AtomicU64::new(0),
            sndtimeo: AtomicU64::new(0),
            priority: AtomicU32::new(0),
            linger: AtomicU64::new(0),
            busy_poll: AtomicU32::new(0),
            incoming_cpu: AtomicI32::new(-1),
            bound_dev: AtomicU32::new(0),
            flags: AtomicU32::new(0),
        }
    }
    pub fn has(&self, f: u32) -> bool {
        self.flags.load(Ordering::Relaxed) & f != 0
    }
    pub fn set(&self, f: u32, on: bool) {
        if on {
            self.flags.fetch_or(f, Ordering::Relaxed);
        } else {
            self.flags.fetch_and(!f, Ordering::Relaxed);
        }
    }
    pub fn sndbuf(&self) -> usize {
        self.sndbuf.load(Ordering::Relaxed) as usize
    }
    pub fn rcvbuf(&self) -> usize {
        self.rcvbuf.load(Ordering::Relaxed) as usize
    }
    /// What `accept` copies from the listener (Linux clones the listening sock for TCP; AF_UNIX inherits only
    /// SO_PASSCRED, `unix_sock_inherit_flags`).
    pub fn copy_from(&self, o: &Opts) {
        for (a, b) in [(&self.sndbuf, &o.sndbuf), (&self.rcvbuf, &o.rcvbuf), (&self.rcvlowat, &o.rcvlowat), (&self.priority, &o.priority), (&self.busy_poll, &o.busy_poll), (&self.bound_dev, &o.bound_dev)] {
            a.store(b.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        for (a, b) in [(&self.rcvtimeo, &o.rcvtimeo), (&self.sndtimeo, &o.sndtimeo), (&self.linger, &o.linger)] {
            a.store(b.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.flags.store(o.flags.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------------------
// The file behind a socket fd

pub(crate) enum Kind {
    Unix(Arc<unix::UnixSock>),
    Inet(Arc<inet::InetSock>),
    Netlink(Arc<netlink::NetlinkSock>),
}

/// The `File` of a socket fd. Dropping it releases the socket (Linux `sock_release` on the last `fput`).
pub struct SockFile {
    pub(crate) kind: Kind,
    released: AtomicBool,
}

static TABLE: RwSpinLock<BTreeMap<usize, Weak<SockFile>>> = RwSpinLock::named("net.sock.table", BTreeMap::new());

klog::counter!(SOCKETS_MADE, "net.sock.made");
klog::counter!(SOCKETS_FREED, "net.sock.freed");

fn key_of<T: ?Sized>(a: &Arc<T>) -> usize {
    Arc::as_ptr(a) as *const () as usize
}

impl SockFile {
    pub(crate) fn new(kind: Kind) -> Result<Arc<SockFile>, Errno> {
        let f = Arc::new(SockFile { kind, released: AtomicBool::new(false) });
        let key = key_of(&f);
        // The socket's own small allocations (this `Arc`, the table node) are infallible like every other small
        // kernel object; R2's ENOMEM applies to what a program sizes (queues and buffers, `buffer`). The table
        // goes when FS gives `File` a downcast (change request), and with it the only reason to return Result.
        TABLE.write().insert(key, Arc::downgrade(&f));
        SOCKETS_MADE.inc();
        Ok(f)
    }

    /// The socket behind an open file (None: not a socket, ENOTSOCK).
    pub fn of(f: &Arc<dyn File>) -> Option<Arc<SockFile>> {
        if f.mode() & uapi::S_IFMT != vfs::abi::S_IFSOCK {
            return None;
        }
        TABLE.read().get(&key_of(f)).and_then(|w| w.upgrade())
    }

    pub fn unix(&self) -> Option<&Arc<unix::UnixSock>> {
        match &self.kind {
            Kind::Unix(u) => Some(u),
            _ => None,
        }
    }

    pub fn inet(&self) -> Option<&Arc<inet::InetSock>> {
        match &self.kind {
            Kind::Inet(i) => Some(i),
            _ => None,
        }
    }

    pub fn netlink(&self) -> Option<&Arc<netlink::NetlinkSock>> {
        match &self.kind {
            Kind::Netlink(n) => Some(n),
            _ => None,
        }
    }

    pub fn opts(&self) -> &Opts {
        match &self.kind {
            Kind::Unix(u) => &u.opts,
            Kind::Inet(i) => &i.opts,
            Kind::Netlink(n) => &n.opts,
        }
    }

    /// (family, type, protocol) as SO_DOMAIN/SO_TYPE/SO_PROTOCOL show them.
    pub fn triple(&self) -> (u16, u32, u32) {
        match &self.kind {
            Kind::Unix(u) => (AF_UNIX, u.ty.linux(), 0),
            Kind::Inet(i) => (i.family(), i.linux_type(), i.protocol()),
            Kind::Netlink(n) => (AF_NETLINK, n.linux_type(), netlink::NETLINK_ROUTE as u32),
        }
    }

    fn events_now(&self) -> u32 {
        match &self.kind {
            Kind::Unix(u) => u.poll(),
            Kind::Inet(i) => i.poll(),
            Kind::Netlink(n) => n.poll(),
        }
    }

    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        match &self.kind {
            Kind::Unix(u) => u.release(false),
            Kind::Inet(i) => i.release(),
            Kind::Netlink(n) => n.release(),
        }
    }
}

impl Drop for SockFile {
    fn drop(&mut self) {
        TABLE.write().remove(&(self as *const SockFile as usize));
        self.release();
        SOCKETS_FREED.inc();
    }
}

impl File for SockFile {
    /// AF_UNIX stream and seqpacket sockets name their queue (woken on data, hang-up, a connection and the own
    /// charge dropping: every change of IN/OUT/HUP). Datagram sockets also become writable when a peer's queue
    /// drains (Linux's peer_wake relay), which wakes no queue of theirs: they stay looked at every 10 ms, as
    /// inet and netlink sockets.
    fn poll_wait(&self, add: &mut dyn FnMut(&vfs::poll::PollQueue)) {
        if let Kind::Unix(u) = &self.kind
            && u.ty != unix::UType::Dgram
        {
            add(&u.wq);
        }
    }
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, false)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write_stream(buf, false)
    }
    fn mode(&self) -> u32 {
        vfs::abi::S_IFSOCK | 0o777
    }
    fn events(&self) -> Events {
        Events(self.events_now())
    }
    /// read(2): recvmsg without flags, address or control data.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        let size = buf.len();
        let mut at = 0usize;
        let mut sink = |d: &[u8]| -> Result<(), Errno> {
            buf[at..at + d.len()].copy_from_slice(d);
            at += d.len();
            Ok(())
        };
        let r = match &self.kind {
            Kind::Unix(u) => u.recv(size, 0, nonblock, &mut sink, false)?,
            Kind::Inet(i) => i.recv(size, 0, nonblock, &mut sink)?,
            Kind::Netlink(n) => n.recv(size, 0, nonblock, &mut sink)?,
        };
        Ok(r.n)
    }
    /// write(2): sendmsg without flags, address or control data (SIGPIPE on a closed stream).
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        let mut src = SliceSource { data: buf, at: 0 };
        match &self.kind {
            Kind::Unix(u) => u.send(&mut src, buf.len(), unix::SendArgs::plain(nonblock), None),
            Kind::Inet(i) => i.send(&mut src, buf.len(), 0, nonblock, None),
            Kind::Netlink(n) => n.send(&mut src, buf.len(), 0, nonblock, None, &current(), None),
        }
    }
    /// `sock_ioctl` and the protocols' ioctls (linux-6.6/net/socket.c:1247, net/unix/af_unix.c:3017,
    /// net/ipv4/tcp.c:604, net/ipv4/udp.c:1666). FIONBIO, FIOCLEX and FIOASYNC are the fd layer's (AB). The
    /// interface ioctls (SIOCGIF*, SIOCGIFCONF, the 0x89xx device range) read and write the program's `ifreq`
    /// and are `sys::dev_ioctl`'s (NW-6; the ioctl syscall asks it first). SIOCSPGRP/FIOSETOWN need the int the
    /// program passes and the fd's owner (AB change request), so they stay ENOTTY here.
    fn ioctl(&self, cmd: u64, _arg: u64) -> Result<Vec<u8>, Errno> {
        let tcp = self.inet().is_some_and(|i| i.linux_type() == SOCK_STREAM);
        let n: i64 = match (&self.kind, cmd) {
            (Kind::Unix(u), SIOCINQ) => u.inq()?,
            (Kind::Unix(u), SIOCOUTQ) => u.outq() as i64,
            (Kind::Inet(i), SIOCINQ) => i.inq()?,
            (Kind::Inet(i), SIOCOUTQ) => i.outq()?,
            // No owner can be set yet (F_SETOWN/SIOCSPGRP), so `f_getown` is 0.
            (_, SIOCGPGRP | FIOGETOWN) => 0,
            // At the mark only with urgent (TCP) or out-of-band (AF_UNIX, CONFIG_AF_UNIX_OOB) data, which V2 does
            // not take: 0. UDP has no SIOCATMARK (ENOTTY through dev_ioctl).
            (Kind::Unix(u), SIOCATMARK) if u.ty == unix::UType::Stream => 0,
            // CachyOS answers EOPNOTSUPP for the other AF_UNIX types (OOB exists on streams only).
            (Kind::Unix(_), SIOCATMARK) => return Err(EOPNOTSUPP),
            (Kind::Inet(_), SIOCATMARK) if tcp => 0,
            // `tcp_ioctl` SIOCOUTQNSD: bytes not yet sent (V2 does not tell sent-unacknowledged from unsent).
            (Kind::Inet(i), SIOCOUTQNSD) if tcp => {
                if i.is_listening() {
                    return Err(EINVAL);
                }
                i.outq()?
            }
            // `sock_gettstamp`: no packet was stamped (V2 keeps no receive stamps): ENOENT. AF_UNIX has no
            // `gettstamp` (ENOTTY).
            (Kind::Inet(_), SIOCGSTAMP_OLD | SIOCGSTAMPNS_OLD | SIOCGSTAMP_NEW | SIOCGSTAMPNS_NEW) => return Err(ENOENT),
            _ => return Err(ENOTTY),
        };
        Ok((n as i32).to_le_bytes().to_vec())
    }
}

impl Pollable for SockFile {
    fn events(&self) -> Events {
        Events(self.events_now())
    }
    fn wait_queue(&self) -> &WaitQueue {
        match &self.kind {
            Kind::Unix(u) => &u.wq,
            Kind::Inet(_) => &inet::READY,
            Kind::Netlink(n) => &n.wq,
        }
    }
}

/// Where the bytes of a send come from: a program's memory (`sys`) or a kernel slice (write(2), tests).
pub trait Source {
    /// Fills `buf` with the next bytes; Err(EFAULT) when the memory is not there.
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), Errno>;
}

pub struct SliceSource<'a> {
    pub data: &'a [u8],
    pub at: usize,
}

impl Source for SliceSource<'_> {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), Errno> {
        let end = self.at + buf.len();
        if end > self.data.len() {
            return Err(EFAULT);
        }
        buf.copy_from_slice(&self.data[self.at..end]);
        self.at = end;
        Ok(())
    }
}

/// What a receive found besides the bytes (`n`).
#[derive(Default)]
pub struct Received {
    /// Bytes given to the program (with MSG_TRUNC on a datagram: the datagram's full length).
    pub n: usize,
    /// MSG_TRUNC / MSG_CTRUNC bits for msg_flags.
    pub flags: u32,
    /// The sender's address (`msg_name`), when there is one.
    pub from: Option<Vec<u8>>,
    /// SCM_CREDENTIALS for a receiver with SO_PASSCRED.
    pub creds: Option<Ucred>,
    /// SCM_RIGHTS: open files to install.
    pub fds: Vec<Arc<vfs::OpenFile>>,
    /// NETLINK_PKTINFO: the group a netlink datagram was sent to (0 for unicast).
    pub pktinfo: Option<u32>,
    /// AF_INET / AF_INET6 ancillary data asked for with IP_PKTINFO, IPV6_RECVPKTINFO and the like: (level,
    /// type, data), in Linux's order.
    pub ctl: Vec<(u32, u32, Vec<u8>)>,
}

/// A kernel byte buffer that answers ENOMEM instead of aborting when the heap is short (R2).
pub(crate) fn buffer(n: usize) -> Result<Vec<u8>, Errno> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
    v.resize(n, 0);
    Ok(v)
}

/// Sockets alive now (made minus freed): for tests and `net_info`.
pub fn alive() -> usize {
    TABLE.read().len()
}

/// What the kernel's log and `net_info` show about program sockets.
pub fn summary() -> alloc::string::String {
    alloc::format!(
        "sockets alive={} made={} netlink_bound={} unix_inflight={} unix_gc_runs={} unix_gc_freed={}",
        alive(),
        SOCKETS_MADE.get(),
        netlink::bound(),
        unix_gc::inflight_total(),
        unix_gc::runs(),
        unix_gc::freed()
    )
}

#[cfg(test)]
mod tests_inet;
#[cfg(test)]
mod tests_inet6;
#[cfg(test)]
mod tests_netlink;
