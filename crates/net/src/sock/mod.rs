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
//! NW-11: every socket belongs to the network namespace it was made in (`Caller::net`, see
//! [`crate::netns`]): the initial one keeps the tables above unchanged; a namespace below it has its own lo,
//! ports, abstract names and netlink ids. Credentials are kept in kernel ids and read in the reader's pid and
//! user namespaces (`Caller::ids`).
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

/// Who makes a socket call (credentials for SO_PEERCRED / SCM_CREDENTIALS, the B3 subject). `pid`, `uid` and
/// `gid` are kernel ids (the initial namespaces' numbers); NW-11: `net` is the caller's network namespace and
/// `ids` how it names processes and ids (both `None`: the initial namespaces).
#[derive(Clone)]
pub struct Caller {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    pub subject: policy::Subject,
    pub net: crate::netns::Net,
    pub ids: Option<Arc<dyn Ids>>,
    /// The real, effective and saved uid and gid (kernel ids) that `scm_check_creds` accepts as the sender's
    /// own; `Caller::new` gives all three as `uid`/`gid`, the namespace hook the process's own.
    pub cred: CredIds,
}

/// Real, effective and saved ids, in this order (kernel numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CredIds {
    pub uid: [u32; 3],
    pub gid: [u32; 3],
}

/// NW-11: a caller's pid and user namespaces, as the syscall glue knows them (proc's objects stay above net).
/// Translations follow Linux: `pid_vnr`/`find_vpid`, `make_kuid`/`from_kuid_munged`.
pub trait Ids: Send + Sync {
    /// Linux capability number `cap` where it counts for this caller: CAP_NET_ADMIN, CAP_NET_BIND_SERVICE and
    /// CAP_NET_RAW over the user namespace that owns its network namespace, CAP_SYS_ADMIN over the one that
    /// owns its pid namespace, CAP_SETUID and CAP_SETGID in its own.
    fn capable(&self, cap: u32) -> bool;
    /// Linux capability number `cap` over the initial user namespace (`capable`, e.g. SIOCSIFTXQLEN). The
    /// default answers no, the safe side for a hook that does not know (NW-12 CR: the kernel's hook answers it
    /// from the credentials).
    fn capable_initial(&self, _cap: u32) -> bool {
        false
    }
    /// A pid as the caller names it -> the kernel pid (None: no such process in its pid namespace).
    fn pid_to_kernel(&self, pid: u32) -> Option<u32>;
    /// A kernel pid as the caller sees it (0: not visible from its pid namespace).
    fn pid_from_kernel(&self, kpid: u32) -> u32;
    /// A uid/gid as the caller names it -> the kernel id (None: not mapped).
    fn uid_to_kernel(&self, uid: u32) -> Option<u32>;
    fn gid_to_kernel(&self, gid: u32) -> Option<u32>;
    /// A kernel uid/gid as the caller sees it (the overflow id 65534 when not mapped).
    fn uid_from_kernel(&self, kuid: u32) -> u32;
    fn gid_from_kernel(&self, kgid: u32) -> u32;
}

/// Linux capability numbers the socket layer asks for.
pub mod cap {
    pub const SETGID: u32 = 6;
    pub const SETUID: u32 = 7;
    pub const NET_BIND_SERVICE: u32 = 10;
    pub const NET_ADMIN: u32 = 12;
    pub const NET_RAW: u32 = 13;
    pub const SYS_ADMIN: u32 = 21;
}

impl Caller {
    /// A caller in the initial namespaces (root is uid 0).
    pub const fn new(pid: u32, uid: u32, gid: u32, subject: policy::Subject) -> Caller {
        Caller { pid, uid, gid, subject, net: None, ids: None, cred: CredIds { uid: [uid; 3], gid: [gid; 3] } }
    }

    /// The kernel itself (pid 0, root, the initial namespaces).
    pub const KERNEL: Caller = Caller::new(0, 0, 0, policy::Subject::KERNEL);

    pub fn ucred(&self) -> Ucred {
        Ucred { pid: self.pid, uid: self.uid, gid: self.gid }
    }

    /// Whether the caller holds capability `cap` (`cap::*`) where the socket layer asks for it.
    pub fn can(&self, cap: u32) -> bool {
        match &self.ids {
            None => self.uid == 0,
            Some(i) => i.capable(cap),
        }
    }

    /// `capable(cap)`: over the initial user namespace, whatever namespace the caller's network namespace has.
    pub fn can_initial(&self, cap: u32) -> bool {
        match &self.ids {
            None => self.uid == 0,
            Some(i) => i.capable_initial(cap),
        }
    }

    /// The caller's own pid as it names it (`task_tgid_vnr(current)`).
    pub fn pid_view(&self) -> u32 {
        match &self.ids {
            None => self.pid,
            Some(i) => i.pid_from_kernel(self.pid),
        }
    }

    /// Kernel credentials as this caller reads them (`scm_set_cred`'s `pid_vnr` and `cred_to_ucred`'s
    /// `from_kuid_munged`): SCM_CREDENTIALS received and SO_PEERCRED. -1 (no credentials) stays -1.
    pub fn view_of(&self, k: Ucred) -> Ucred {
        match &self.ids {
            None => k,
            Some(i) => Ucred {
                pid: if k.pid == 0 { 0 } else { i.pid_from_kernel(k.pid) },
                uid: if k.uid == u32::MAX { k.uid } else { i.uid_from_kernel(k.uid) },
                gid: if k.gid == u32::MAX { k.gid } else { i.gid_from_kernel(k.gid) },
            },
        }
    }
}

impl PartialEq for Caller {
    fn eq(&self, o: &Caller) -> bool {
        let ids = match (&self.ids, &o.ids) {
            (None, None) => true,
            (Some(a), Some(b)) => core::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
            _ => false,
        };
        self.pid == o.pid && self.uid == o.uid && self.gid == o.gid && self.subject == o.subject && self.cred == o.cred && crate::netns::same(&self.net, &o.net) && ids
    }
}

impl Eq for Caller {}

impl core::fmt::Debug for Caller {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Caller")
            .field("pid", &self.pid)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("subject", &self.subject)
            .field("net", &self.net)
            .field("ids", &self.ids.is_some())
            .field("cred", &self.cred)
            .finish()
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
    match HOOKS.get().and_then(|h| (h.current)()) {
        Some(c) if c.net.is_none() && c.ids.is_none() => with_ns(c),
        Some(c) => c,
        None => Caller::KERNEL,
    }
}

/// NW-11: where the calling program's namespaces come from (installed by the syscall glue, `set_ns_hooks`).
pub struct NsHooks {
    /// The calling program's network namespace view and its ids (`(None, None)`: the initial namespaces, the
    /// fast answer for every program outside a sandbox).
    pub current: fn() -> NsNow,
}

/// What the namespace hook knows of the calling program.
#[derive(Default)]
pub struct NsNow {
    pub net: crate::netns::Net,
    pub ids: Option<Arc<dyn Ids>>,
    /// The process's real, effective and saved ids (None: no process, the caller's `uid`/`gid` stay).
    pub cred: Option<CredIds>,
}

static NS_HOOKS: ksync::OnceRef<NsHooks> = ksync::OnceRef::new();

pub fn set_ns_hooks(h: &'static NsHooks) {
    NS_HOOKS.set(h);
}

/// NW-12: the calling program's namespaces as the hook gives them (the initial ones without it).
pub fn ns_now() -> NsNow {
    NS_HOOKS.get().map(|h| (h.current)()).unwrap_or_default()
}

/// `c` with the calling program's namespaces (unchanged without the hook: host tests, the kernel's own calls).
pub(crate) fn with_ns(mut c: Caller) -> Caller {
    if let Some(h) = NS_HOOKS.get() {
        let n = (h.current)();
        c.net = n.net;
        c.ids = n.ids;
        if let Some(k) = n.cred {
            c.cred = k;
        }
    }
    c
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
    /// TCP_QUICKACK 0: Linux's pingpong mode (getsockopt says !pingpong, so a fresh socket says 1).
    pub const PINGPONG: u32 = 2048;
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
    /// `SOCK_INODE(sock)->i_ino`: programs find a connection's owner by it, from /proc/net/tcp to /proc/<pid>/fd.
    ino: u64,
}

static NEXT_INO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(10000);

/// Every socket by address, with its network namespace's key (NW-12: /proc/net picks a namespace's sockets
/// without touching the others').
static TABLE: RwSpinLock<BTreeMap<usize, (Weak<SockFile>, usize)>> = RwSpinLock::named("net.sock.table", BTreeMap::new());

/// A namespace as a table key: its view's address, 0 for the initial one.
fn net_key(n: &crate::netns::Net) -> usize {
    n.as_ref().map_or(0, |v| Arc::as_ptr(v) as usize)
}

klog::counter!(SOCKETS_MADE, "net.sock.made");
klog::counter!(SOCKETS_FREED, "net.sock.freed");

fn key_of<T: ?Sized>(a: &Arc<T>) -> usize {
    Arc::as_ptr(a) as *const () as usize
}

impl SockFile {
    pub(crate) fn new(kind: Kind) -> Result<Arc<SockFile>, Errno> {
        let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
        let f = Arc::new(SockFile { kind, released: AtomicBool::new(false), ino });
        let key = key_of(&f);
        // The socket's own small allocations (this `Arc`, the table node) are infallible like every other small
        // kernel object; R2's ENOMEM applies to what a program sizes (queues and buffers, `buffer`). The table
        // goes when FS gives `File` a downcast (change request), and with it the only reason to return Result.
        let nk = net_key(f.net());
        TABLE.write().insert(key, (Arc::downgrade(&f), nk));
        SOCKETS_MADE.inc();
        Ok(f)
    }

    pub(crate) fn ino(&self) -> u64 {
        self.ino
    }

    /// NW-12: the open sockets of network namespace `net` (for /proc/net), in table order.
    pub(crate) fn all_in(net: &crate::netns::Net) -> Vec<Arc<SockFile>> {
        let nk = net_key(net);
        // The list's memory is made with the table's lock dropped (`unix::fill`'s reason); a table that grew
        // meanwhile asks again.
        let mut all: Vec<Arc<SockFile>> = Vec::new();
        loop {
            let want = {
                let t = TABLE.read();
                let mine = || t.values().filter(|(_, k)| *k == nk);
                let n = mine().count();
                if n <= all.capacity() {
                    all.extend(mine().filter_map(|(w, _)| w.upgrade()));
                    break;
                }
                n
            };
            all.reserve_exact(want);
        }
        // Dropped outside the table's lock: a socket closed meanwhile is released here (`SockFile`'s drop).
        all.into_iter().filter(|f| !f.released.load(Ordering::Acquire) && crate::netns::same(f.net(), net)).collect()
    }

    /// The socket behind an open file (None: not a socket, ENOTSOCK).
    pub fn of(f: &Arc<dyn File>) -> Option<Arc<SockFile>> {
        if f.mode() & uapi::S_IFMT != vfs::abi::S_IFSOCK {
            return None;
        }
        TABLE.read().get(&key_of(f)).and_then(|(w, _)| w.upgrade())
    }

    /// NW-11: the socket's network namespace (`sock_net`).
    pub(crate) fn net(&self) -> &crate::netns::Net {
        match &self.kind {
            Kind::Unix(u) => u.net(),
            Kind::Inet(i) => i.net(),
            Kind::Netlink(n) => n.net(),
        }
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
    /// netlink sockets. AF_INET sockets name the shared `inet::READY` (NW-10), which every change of any of
    /// them wakes (the stack's rounds, loopback, accept queues).
    fn poll_wait(&self, add: &mut dyn FnMut(&vfs::poll::PollQueue)) {
        match &self.kind {
            Kind::Unix(u) if u.ty != unix::UType::Dgram => add(&u.wq),
            Kind::Inet(_) => add(&inet::READY),
            _ => {}
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
    fn attr(&self) -> Option<vfs::abi::Attr> {
        Some(vfs::abi::Attr { ino: self.ino, mode: self.mode(), nlink: 1, blksize: 4096, uid: 1000, gid: 1000, ..Default::default() })
    }
    fn link_name(&self) -> Option<alloc::string::String> {
        Some(alloc::format!("socket:[{}]", self.ino))
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
#[cfg(test)]
mod tests_netns;
