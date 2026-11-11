//! AF_INET sockets for programs (linux-6.6/net/ipv4/af_inet.c, tcp.c, udp.c for the semantics) on the kernel's
//! smoltcp stack and its 127.0.0.0/8 loopback: TCP streams (non-blocking connect with EINPROGRESS and SO_ERROR,
//! listen/accept on the stack's private-only listeners, half-close, MSG_PEEK/WAITALL) and UDP datagrams
//! (bound/connected, the loopback delivered here directly). B3: `policy::check` at bind, listen and connect (and
//! on an unconnected UDP send to a new peer); TCP connections over a card are recorded by the stack with this
//! program as owner, UDP peers by `SmolUdp`.
//!
//! Locks: a socket's state is a short spin section (never held across a wait, a stack call that allocates, or
//! the drop of a connection: those happen outside). All AF_INET readiness waits on one queue, `READY`, which the
//! stack's poll rounds, the loopback pipes, accept queues and loopback datagrams wake (the stack has no
//! per-socket wake-ups yet; NW-7 may add them).

#[path = "inet6.rs"]
mod inet6;
#[path = "inet_ctl.rs"]
mod ctl;

use super::abi::*;
use super::addr::{encode_in, family, parse_in};
use inet6::{Fam, LO6};
pub use inet6::{IPV6_V6ONLY, SOL_IPV6, opt6_len};
use super::{Caller, Deadline, Opts, Quiet, Received, Source, flag, wait};
use crate::conn::Conn;
use crate::stack::sockets::ProgListener;
use crate::stack::udp::{PortGuard, SmolUdp};
use crate::loopback::Side;
use crate::netns::NetView;
use crate::stack::Stack;
use crate::{Ipv4, NetError, SockAddr};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ksync::SpinLock;
use uapi::errno::*;
use vfs::Errno;

klog::counter!(CONNECT_FAILED, "net.sock.connect_failed");

/// Every AF_INET socket's readiness changes are announced here. A `PollQueue` (NW-10): poll, select and epoll
/// subscribe to it, so a program sleeping in them wakes with the data; as a plain `WaitQueue` they looked again
/// only every 10 ms (`vfs::poll::LEGACY_SLICE_NS`), which stalled BusyBox wget for ~9 ms each time its receive
/// buffer ran dry (the sender then idles with nothing in flight). Readers blocked in recv wait on it as before.
pub static READY: vfs::poll::PollQueue = vfs::poll::PollQueue::new();

/// Wakes AF_INET waiters (cheap without any).
pub fn notify() {
    if READY.waiter_count() > 0 {
        READY.wake_all();
    }
}

static STACK: ksync::OnceRef<Stack> = ksync::OnceRef::new();

/// The stack program sockets use: the kernel's, or one a host test installed.
pub fn set_stack(s: &'static Stack) {
    STACK.set(s);
}

/// The stack program sockets use (a host test's, else the kernel's), if there is one.
pub(crate) fn stack_opt() -> Option<&'static Stack> {
    STACK.get().or_else(crate::stack::kernel)
}

fn stack() -> Result<&'static Stack, Errno> {
    stack_opt().ok_or(ENETDOWN)
}

fn errno(e: NetError) -> Errno {
    match e {
        NetError::WouldBlock => EAGAIN,
        NetError::TimedOut => ETIMEDOUT,
        NetError::ConnectionRefused => ECONNREFUSED,
        NetError::ConnectionReset => ECONNRESET,
        NetError::AddrInUse => EADDRINUSE,
        NetError::NotConnected => ENOTCONN,
        NetError::Refused => EACCES,
        NetError::NoMemory => ENOMEM,
        NetError::Unreachable => ENETUNREACH,
        NetError::Closed => EPIPE,
        NetError::Other => EIO,
    }
}

/// Connection buffers of program TCP sockets: SO_RCVBUF/SO_SNDBUF decide them at connect/listen (smoltcp's
/// buffers do not grow later), between these bounds.
const TCP_BUF_MIN: usize = 16 * 1024;
const TCP_BUF_DEFAULT_RX: usize = 256 * 1024;
const TCP_BUF_DEFAULT_TX: usize = 128 * 1024;
const TCP_BUF_MAX: usize = 4 * 1024 * 1024;
/// A recv copies at most this much per step (the kernel never allocates what a program asks for).
const CHUNK: usize = 64 * 1024;
/// UDP's largest payload (65535 - IP and UDP headers).
const UDP_MAX: usize = 65507;
/// A full UDP send buffer on a card is looked at again this often.
const UDP_RETRY_NS: u64 = 10_000_000;

// ---------------------------------------------------------------------------------------------------------
// TCP ports held by program sockets (Linux's bind hash with its SO_REUSEADDR rule)

#[derive(Clone, Copy, Debug)]
struct PortUse {
    id: usize,
    ip: Ipv4,
    fam: Fam,
    reuse: bool,
    listening: bool,
}

/// NW-11: the TCP ports, loopback UDP receivers and (below the initial namespace) UDP ports of one network
/// namespace. The initial namespace's are `INIT`; its UDP ports are the stack's table and `inet6::UDP6_PORTS`.
pub(crate) struct Tables {
    tcp_ports: SpinLock<BTreeMap<u16, Vec<PortUse>>>,
    /// Loopback UDP receivers per port and side (false: IPv4 senders, true: AF_INET6 senders to ::1); a
    /// dual-stack `::` holds both slots, so an IPv6-only socket and an AF_INET one share a port as on Linux.
    lo_udp: SpinLock<BTreeMap<(u16, bool), Weak<LoUdp>>>,
    /// UDP ports by side of a namespace below the initial one (`NsUdp`).
    udp: SpinLock<BTreeSet<(u16, bool)>>,
}

impl Tables {
    pub(crate) const fn new() -> Tables {
        Tables {
            tcp_ports: SpinLock::named("net.inet.ports", BTreeMap::new()),
            lo_udp: SpinLock::named("net.inet.lo_udp", BTreeMap::new()),
            udp: SpinLock::named("net.inet.ns_udp", BTreeSet::new()),
        }
    }

    fn tcp_bound(&self, port: u16) -> bool {
        self.tcp_ports.lock().get(&port).is_some_and(|v| !v.is_empty())
    }

    fn port_add(&self, port: u16, me: PortUse) {
        self.tcp_ports.lock().entry(port).or_default().push(me);
    }

    fn port_update(&self, port: u16, id: usize, f: impl FnOnce(&mut PortUse)) {
        if let Some(u) = self.tcp_ports.lock().get_mut(&port).and_then(|v| v.iter_mut().find(|u| u.id == id)) {
            f(u);
        }
    }

    fn port_release(&self, port: u16, id: usize) {
        let mut t = self.tcp_ports.lock();
        if let Some(v) = t.get_mut(&port) {
            v.retain(|u| u.id != id);
            if v.is_empty() {
                t.remove(&port);
            }
        }
    }

    /// Takes `r` off the slots of `port` it still holds.
    fn lo_unhook(&self, port: u16, r: &Arc<LoUdp>, slots: &[bool]) {
        let mut t = self.lo_udp.lock();
        for v6 in slots {
            if t.get(&(port, *v6)).is_some_and(|w| core::ptr::eq(w.as_ptr(), Arc::as_ptr(r))) {
                t.remove(&(port, *v6));
            }
        }
    }

    /// Delivers a datagram to whoever holds `to.port` on the loopback side `v6`; false when nobody does.
    fn lo_deliver(&self, data: &[u8], from: SockAddr, to: SockAddr, v6: bool) -> bool {
        let Some(r) = self.lo_udp.lock().get(&(to.port, v6)).and_then(|w| w.upgrade()) else { return false };
        // A full receive buffer drops the datagram (Linux counts it as a receive-buffer error).
        if r.bytes.load(Ordering::Acquire) + data.len() > r.rcvbuf.load(Ordering::Relaxed) {
            return true;
        }
        let mut v = Vec::new();
        if v.try_reserve_exact(data.len()).is_err() {
            return true;
        }
        v.extend_from_slice(data);
        // The queue grows with its lock dropped (`ksync::spare`).
        let mut spare = ksync::Spare::new();
        loop {
            let mut q = r.q.lock();
            if !spare.fits(&mut *q, 1) {
                drop(q);
                if spare.grow().is_err() {
                    return true;
                }
                continue;
            }
            r.bytes.fetch_add(data.len(), Ordering::AcqRel);
            q.push_back(LoDgram { data: v, from, at: ctl::Arrival { to: to.ip, v6, ifindex: 1, spec: to.ip } });
            break;
        }
        notify();
        true
    }
}

static INIT: Tables = Tables::new();

/// Whether a program socket of the initial namespace holds TCP `port` (the stack's ephemeral picker skips it).
pub(crate) fn tcp_port_bound(port: u16) -> bool {
    INIT.tcp_bound(port)
}

/// `sk_state` numbers /proc/net shows (include/net/tcp_states.h).
pub(crate) const TCP_ESTABLISHED: u8 = 1;
pub(crate) const TCP_SYN_SENT: u8 = 2;
pub(crate) const TCP_CLOSE: u8 = 7;
pub(crate) const TCP_LISTEN: u8 = 10;

/// NW-12: one line of /proc/net/tcp or /proc/net/udp.
pub(crate) struct ProcRow {
    pub(crate) tcp: bool,
    pub(crate) local: SockAddr,
    pub(crate) peer: SockAddr,
    pub(crate) state: u8,
    /// The owner's kernel uid.
    pub(crate) uid: u32,
}

/// NW-11: where a socket's traffic goes: the kernel's stack (the initial namespace) or a namespace's own
/// loopback (nothing else exists there).
#[derive(Clone, Copy)]
enum Place<'a> {
    Stack(&'static Stack),
    Ns(&'a Arc<NetView>),
}

impl<'a> Place<'a> {
    fn tables(self) -> &'a Tables {
        match self {
            Place::Stack(_) => &INIT,
            Place::Ns(v) => &v.inet,
        }
    }

    /// `inet_addr_valid_or_nonlocal` (`v6_lo`: an AF_INET6 socket's ::1). In a namespace, as CachyOS answers
    /// (`__inet_dev_addr_type`, linux-6.6/net/ipv4/fib_frontend.c:206): before lo's first address (its first UP
    /// or RTM_NEWADDR/SIOCSIFADDR) there is no local table, so every IPv4 address passes; after it 0.0.0.0,
    /// broadcast, multicast and the subnets of lo's addresses (a loopback's prefix route is local; they stay
    /// while lo is down again). ::1 is lo's only while it is UP (`ipv6_chk_addr`).
    fn local_ok(self, ip: Ipv4, v6_lo: bool) -> bool {
        match self {
            Place::Stack(st) => InetSock::local_ok(st, ip),
            Place::Ns(v) => {
                v.with_dev(|d| {
                    if v6_lo {
                        d.has6()
                    } else {
                        !d.table || ip.is_unspecified() || ip == Ipv4::BROADCAST || (224..240).contains(&ip.0[0]) || d.covers(ip.0)
                    }
                })
            }
        }
    }

    /// The loopback a TCP connection to 127/8 or ::1 (`v6_lo`) takes. In a namespace whose lo is down or has no
    /// address for `to`: no route (ENETUNREACH), for ::1 no source address (EADDRNOTAVAIL,
    /// `ip6_dst_lookup_tail`). (A connection to a lo that was up and is down again hangs on Linux until it times
    /// out; V2 answers ENETUNREACH at once.)
    fn lo(self, to: Ipv4, v6_lo: bool) -> Result<&'a crate::loopback::Loopback, Errno> {
        match self {
            Place::Stack(st) => Ok(&st.lo),
            Place::Ns(v) => {
                let ok = v.with_dev(|d| if v6_lo { d.has6() } else { d.up && d.covers(to.0) });
                match (ok, v6_lo) {
                    (true, _) => Ok(&v.lo),
                    (false, true) => Err(EADDRNOTAVAIL),
                    (false, false) => Err(ENETUNREACH),
                }
            }
        }
    }

    /// A datagram in a namespace (`v6_lo`: to ::1): to one of lo's local routes (127/8 after an UP, the
    /// addresses a program gave it) it is delivered while lo is UP and dropped while it is down (Ok(false), the
    /// route is still there); without a route: ENETUNREACH; ::1 without lo UP: EADDRNOTAVAIL.
    fn ns_route(v: &NetView, to: Ipv4, v6_lo: bool) -> Result<bool, Errno> {
        v.with_dev(|d| {
            if v6_lo {
                return if d.has6() { Ok(true) } else { Err(EADDRNOTAVAIL) };
            }
            if !d.covers(to.0) {
                return Err(ENETUNREACH);
            }
            Ok(d.up)
        })
    }
}

/// NW-11: a UDP port of a namespace below the initial one, on the slots of its family; given back on drop.
struct NsUdp {
    view: Arc<NetView>,
    port: u16,
    slots: &'static [bool],
}

impl NsUdp {
    /// Takes `port` (0: an ephemeral one) on `slots`: EADDRINUSE when one of them is held.
    fn take(view: &Arc<NetView>, slots: &'static [bool], port: u16) -> Result<NsUdp, Errno> {
        let t = &view.inet.udp;
        let held = |p: u16| {
            let u = t.lock();
            slots.iter().any(|s| u.contains(&(p, *s)))
        };
        // Bounded (B4): one pass over the ephemeral range at most.
        for _ in crate::stack::EPHEMERAL {
            let p = if port != 0 { port } else { view.ephemeral(held).ok_or(EADDRINUSE)? };
            let mut u = t.lock();
            if slots.iter().any(|s| u.contains(&(p, *s))) {
                if port != 0 {
                    return Err(EADDRINUSE);
                }
                continue;
            }
            for s in slots {
                u.insert((p, *s));
            }
            return Ok(NsUdp { view: view.clone(), port: p, slots });
        }
        Err(EADDRINUSE)
    }
}

impl Drop for NsUdp {
    fn drop(&mut self) {
        let mut u = self.view.inet.udp.lock();
        for s in self.slots {
            u.remove(&(self.port, *s));
        }
    }
}

/// `inet_csk_bind_conflict`: two sockets on one port conflict when their addresses overlap, unless both have
/// SO_REUSEADDR and the other one does not listen (so a socket may listen beside a bound, idle one).
fn conflict(v: &[PortUse], me: &PortUse) -> bool {
    v.iter().any(|o| o.id != me.id && inet6::overlap((o.ip, o.fam), (me.ip, me.fam)) && !(me.reuse && o.reuse && !o.listening))
}

/// Takes `port` (0: an ephemeral one) for `me`. Lock order: the port counter (the stack's or the namespace's)
/// before `tcp_ports` (its picker asks `tcp_bound`), so a pick happens outside `tcp_ports` and is checked again
/// under it.
fn port_take(at: Place<'_>, me: PortUse, port: u16) -> Result<u16, Errno> {
    let tables = at.tables();
    // Kernel listeners are IPv4 and program ones are in `tcp_ports`: an IPv6-only socket asks only the latter.
    // A namespace has no kernel listeners.
    let kernel = |p: u16| match at {
        Place::Stack(st) => me.fam != Fam::Only6 && st.listening_on(me.ip, p),
        Place::Ns(_) => false,
    };
    if port != 0 {
        if kernel(port) {
            return Err(EADDRINUSE);
        }
        let mut t = tables.tcp_ports.lock();
        if t.get(&port).is_some_and(|v| conflict(v, &me)) {
            return Err(EADDRINUSE);
        }
        t.entry(port).or_default().push(me);
        return Ok(port);
    }
    // Bounded (B4): one pass over the ephemeral range at most, as the picker itself.
    for _ in crate::stack::EPHEMERAL {
        let taken = |p: u16| tables.tcp_bound(p) || kernel(p);
        let p = match at {
            Place::Stack(st) => st.ephemeral_port(taken),
            Place::Ns(v) => v.ephemeral(taken),
        }
        .ok_or(EADDRINUSE)?;
        let mut t = tables.tcp_ports.lock();
        if t.get(&p).is_some_and(|v| conflict(v, &me)) {
            continue;
        }
        t.entry(p).or_default().push(me);
        return Ok(p);
    }
    Err(EADDRINUSE)
}

// ---------------------------------------------------------------------------------------------------------
// UDP on 127.0.0.0/8 (the stack's loopback carries TCP only)

pub(crate) struct LoUdp {
    q: SpinLock<VecDeque<LoDgram>>,
    bytes: AtomicUsize,
    rcvbuf: AtomicUsize,
}

/// A loopback datagram: payload, sender, and how it arrived (for the receive ancillary data).
struct LoDgram {
    data: Vec<u8>,
    from: SockAddr,
    at: ctl::Arrival,
}

/// The first datagram a socket connected to `peer` may have (others are dropped, as a connected socket takes
/// datagrams from its peer only), cut to `size`: bytes, full length, sender, arrival. Nothing is copied or
/// freed under the queue's lock: a taken datagram hands over its own bytes, a peek copies into a buffer made
/// with the lock dropped (`unix::fill`).
fn lo_take(l: &LoUdp, peer: Option<SockAddr>, size: usize, peek: bool) -> Result<Option<(Vec<u8>, usize, SockAddr, ctl::Arrival)>, Errno> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let taken = {
            let mut q = l.q.lock();
            let Some(x) = q.front() else { return Ok(None) };
            if peek && peer.is_none_or(|p| p == x.from) {
                let n = x.data.len().min(size);
                if super::unix::fill(&mut buf, &x.data[..n]) {
                    return Ok(Some((buf, x.data.len(), x.from, x.at)));
                }
                Err(n)
            } else {
                let x = q.pop_front().ok_or(EAGAIN)?;
                l.bytes.fetch_sub(x.data.len(), Ordering::AcqRel);
                Ok(x)
            }
        };
        match taken {
            Err(n) => super::unix::grow(&mut buf, n)?,
            Ok(x) if peer.is_some_and(|p| p != x.from) => drop(x),
            Ok(LoDgram { mut data, from, at }) => {
                let len = data.len();
                data.truncate(size);
                return Ok(Some((data, len, from, at)));
            }
        }
    }
}

/// The loopback slots a socket of family `fam` holds.
fn lo_slots(fam: Fam) -> &'static [bool] {
    match fam {
        Fam::V4 => &[false],
        Fam::Only6 => &[true],
        Fam::Dual => &[false, true],
    }
}


// ---------------------------------------------------------------------------------------------------------
// The socket

enum Tcp {
    /// Fresh, or bound (`local` set), or after a failed connect.
    Idle,
    Listening(ProgListener),
    /// SYN sent (a card); `Connected` once the handshake is done.
    Connecting(Box<dyn Conn>),
    Connected(Box<dyn Conn>),
}

struct TcpSt {
    s: Tcp,
    local: Option<SockAddr>,
    peer: Option<SockAddr>,
    /// The port this socket holds in its namespace's `tcp_ports`.
    port: Option<u16>,
    err: Errno,
    shutdown: u8,
    /// Linux's SS_CONNECTING: a non-blocking connect is under way, or ended and the program has not called
    /// connect() again to see the result.
    in_progress: bool,
    /// The connection failed or was reset and that was turned into the pending error (`tcp_done`: both
    /// directions shut).
    dead: bool,
}

struct UdpSt {
    local: Option<SockAddr>,
    guard: Option<PortGuard>,
    smol: Option<Arc<SmolUdp>>,
    lo: Option<Arc<LoUdp>>,
    /// The port on the IPv6 side (`inet6::UDP6_PORTS`): IPv6-only and dual-stack sockets.
    guard6: Option<inet6::Port6Guard>,
    /// NW-11: the port of a socket in a namespace below the initial one (instead of `guard`/`guard6`).
    ns_port: Option<NsUdp>,
    /// Which loopback slots `lo` holds.
    fam: Fam,
    /// The multicast group an AF_INET6 socket was bound to (getsockname shows it).
    group6: Option<[u8; 16]>,
    peer: Option<SockAddr>,
    err: Errno,
    shutdown: u8,
    /// The last destination the policy allowed (an unconnected send asks again for a new one).
    allowed: Option<SockAddr>,
    /// The source address a connect chose (`inet_saddr`), shown by getsockname while bound to 0.0.0.0.
    src: Option<Ipv4>,
}

enum Proto {
    Tcp(SpinLock<TcpSt>),
    Udp(SpinLock<UdpSt>),
}

pub struct InetSock {
    proto: Proto,
    pub(crate) opts: Opts,
    owner: Caller,
    /// SOL_IP / SOL_TCP / SOL_UDP values set by the program (kept and given back; NODELAY and keep-alive reach
    /// the connection).
    ip_opts: SpinLock<BTreeMap<(u32, u32), i32>>,
    /// Serialises TCP readers between peeking bytes and discarding them (`tcp_recv`).
    rd: ksync::Mutex<()>,
    /// AF_INET6 (`inet6`); IPV6_ADDRFORM turns it into AF_INET.
    v6: AtomicBool,
    /// IPV6_ADD_MEMBERSHIP groups (group, ifindex).
    groups6: SpinLock<Vec<([u8; 16], i32)>>,
    /// Serialises bind, listen and the automatic UDP bind with IPV6_V6ONLY and IPV6_ADDRFORM (Linux's socket
    /// lock): the family a port is taken with is the one the program reads back.
    bind_mx: ksync::Mutex<()>,
}

/// SOL_IP options accepted with their defaults (`ip_setsockopt`/`ip_getsockopt`: IP_TOS, IP_TTL, IP_PKTINFO,
/// IP_MTU_DISCOVER, IP_RECVERR, IP_RECVTTL, IP_RECVTOS, IP_FREEBIND, IP_BIND_ADDRESS_NO_PORT, IP_MULTICAST_TTL,
/// IP_MULTICAST_LOOP); others are ENOPROTOOPT.
const IP_OPTS: &[(u32, i32)] = &[(1, 0), (2, 64), (8, 0), (10, 1), (11, 0), (12, 0), (13, 0), (15, 0), (24, 0), (33, 1), (34, 1)];

fn idle_tcp() -> TcpSt {
    TcpSt { s: Tcp::Idle, local: None, peer: None, port: None, err: 0, shutdown: 0, in_progress: false, dead: false }
}

impl InetSock {
    /// `inet_create`: SOCK_STREAM is TCP, SOCK_DGRAM UDP (protocol 0 or its own number). ICMP datagram sockets
    /// and raw sockets are not provided (EPROTONOSUPPORT; raw is EPERM first for non-root, as Linux).
    pub fn create(ty: u32, protocol: i32, c: &Caller) -> Result<Arc<InetSock>, Errno> {
        Self::create_in(false, ty, protocol, c)
    }

    /// `inet6_create`: the same protocols over AF_INET6 (`inet6`).
    pub fn create6(ty: u32, protocol: i32, c: &Caller) -> Result<Arc<InetSock>, Errno> {
        Self::create_in(true, ty, protocol, c)
    }

    fn create_in(v6: bool, ty: u32, protocol: i32, c: &Caller) -> Result<Arc<InetSock>, Errno> {
        if protocol < 0 || protocol as u32 >= IPPROTO_MAX {
            return Err(EINVAL);
        }
        let p = protocol as u32;
        let proto = match ty {
            SOCK_STREAM if p == 0 || p == IPPROTO_TCP => Proto::Tcp(SpinLock::named("net.inet.tcp", idle_tcp())),
            SOCK_DGRAM if p == 0 || p == IPPROTO_UDP => Proto::Udp(SpinLock::named(
                "net.inet.udp",
                UdpSt { local: None, guard: None, smol: None, lo: None, guard6: None, ns_port: None, fam: Fam::V4, group6: None, peer: None, err: 0, shutdown: 0, allowed: None, src: None },
            )),
            SOCK_STREAM | SOCK_DGRAM => return Err(EPROTONOSUPPORT),
            SOCK_RAW if !c.can(super::cap::NET_RAW) => return Err(EPERM),
            SOCK_RAW => return Err(EPROTONOSUPPORT),
            _ => return Err(ESOCKTNOSUPPORT),
        };
        let opts = match proto {
            Proto::Tcp(_) => Opts::new(TCP_WMEM_DEFAULT, TCP_RMEM_DEFAULT),
            Proto::Udp(_) => Opts::new(WMEM_DEFAULT, RMEM_DEFAULT),
        };
        Ok(Arc::new(InetSock {
            proto,
            opts,
            // Without `ids`: a socket must not keep its creator's cred and namespaces alive; capability checks
            // use the caller of each call (`sockopt_ns_capable`).
            owner: Caller { pid: c.pid, uid: c.uid, gid: c.gid, subject: c.subject, net: c.net.clone(), ids: None, cred: c.cred },
            ip_opts: SpinLock::named("net.inet.opts", BTreeMap::new()),
            rd: ksync::Mutex::named("net.inet.read", ()),
            v6: AtomicBool::new(v6),
            groups6: SpinLock::named("net.inet.groups6", Vec::new()),
            bind_mx: ksync::Mutex::named("net.inet.bind", ()),
        }))
    }

    fn id(&self) -> usize {
        self as *const InetSock as usize
    }

    /// NW-11: the socket's network namespace (its creator's; None: the initial one).
    pub(crate) fn net(&self) -> &crate::netns::Net {
        &self.owner.net
    }

    fn t(&self) -> &Tables {
        match &self.owner.net {
            None => &INIT,
            Some(v) => &v.inet,
        }
    }

    fn place(&self) -> Result<Place<'_>, Errno> {
        match &self.owner.net {
            None => Ok(Place::Stack(stack()?)),
            Some(v) => Ok(Place::Ns(v)),
        }
    }

    pub fn family(&self) -> u16 {
        if self.is_v6() { AF_INET6 } else { AF_INET }
    }

    pub fn linux_type(&self) -> u32 {
        match self.proto {
            Proto::Tcp(_) => SOCK_STREAM,
            Proto::Udp(_) => SOCK_DGRAM,
        }
    }

    pub fn protocol(&self) -> u32 {
        match self.proto {
            Proto::Tcp(_) => IPPROTO_TCP,
            Proto::Udp(_) => IPPROTO_UDP,
        }
    }

    pub fn is_listening(&self) -> bool {
        matches!(&self.proto, Proto::Tcp(t) if matches!(t.lock().s, Tcp::Listening(_)))
    }

    fn reuse(&self) -> bool {
        self.opts.has(flag::REUSEADDR)
    }

    /// TCP buffer sizes for a new connection: from SO_RCVBUF/SO_SNDBUF when the program set them.
    /// WL-4 CR 2: the card SO_BINDTODEVICE / SO_BINDTOIFINDEX bound this socket to, as the stack's position
    /// (ifview: lo is ifindex 1, the cards 2, 3, ...). Bound to lo: no card (a non-loopback peer is unreachable).
    fn bound_card(&self) -> Option<usize> {
        match self.opts.bound_dev.load(Ordering::Relaxed) {
            0 => None,
            1 => Some(usize::MAX),
            i => Some(i as usize - 2),
        }
    }

    fn bufs(&self) -> (usize, usize) {
        let pick = |set: usize, default_set: u32, dflt: usize| if set == default_set as usize { dflt } else { set.clamp(TCP_BUF_MIN, TCP_BUF_MAX) };
        (pick(self.opts.rcvbuf(), TCP_RMEM_DEFAULT, TCP_BUF_DEFAULT_RX), pick(self.opts.sndbuf(), TCP_WMEM_DEFAULT, TCP_BUF_DEFAULT_TX))
    }

    fn policy(&self, a: policy::Access<'_>) -> Result<(), Errno> {
        if self.owner.subject == policy::Subject::KERNEL {
            return Ok(());
        }
        policy::check(&self.owner.subject, &a).map_err(|d| {
            klog::event!(Info, "net", "policy.refused", pid = self.owner.pid, program = self.owner.subject.program.0, reason = d.reason.as_str());
            EACCES
        })
    }

    // -----------------------------------------------------------------------------------------------------
    // addresses

    /// A sockaddr_in for bind/connect: EINVAL below 16 bytes, EAFNOSUPPORT for another family (bind also takes
    /// AF_UNSPEC with INADDR_ANY, `__inet_bind`'s compatibility rule).
    fn sin(raw: &[u8], bind: bool) -> Result<SockAddr, Errno> {
        let a = parse_in(raw).ok_or(EINVAL)?;
        match family(raw) {
            Some(AF_INET) => Ok(a),
            Some(AF_UNSPEC) if bind && a.ip.is_unspecified() => Ok(a),
            _ => Err(EAFNOSUPPORT),
        }
    }

    /// `inet_addr_valid_or_nonlocal`: 0.0.0.0, 127/8, broadcast, multicast or a card's address.
    fn local_ok(st: &Stack, ip: Ipv4) -> bool {
        ip.is_unspecified() || ip.is_loopback() || ip == Ipv4::BROADCAST || (224..240).contains(&ip.0[0]) || st.interfaces().iter().any(|i| i.info.addr.is_some_and(|(a, _)| a == ip))
    }

    pub fn bind(&self, raw: &[u8], c: &Caller) -> Result<(), Errno> {
        let _b = self.bind_mx.lock();
        if !self.is_v6() {
            return self.bind_at(Self::sin(raw, true)?, None, c);
        }
        let b = self.bind6(raw, c)?;
        let saved = b.only6.then(|| self.force_v6only());
        let r = self.bind_at(b.at, b.group, c);
        if r.is_err()
            && let Some(old) = saved
        {
            self.restore_v6only(old);
        }
        r
    }

    /// `bind` once the address is known (`group`: an AF_INET6 UDP socket's multicast group).
    fn bind_at(&self, a: SockAddr, group: Option<[u8; 16]>, c: &Caller) -> Result<(), Errno> {
        let at = self.place()?;
        if !at.local_ok(a.ip, self.is_v6() && a.ip == LO6) {
            return Err(EADDRNOTAVAIL);
        }
        if a.port != 0 && a.port < UNPRIVILEGED_PORT_START && !c.can(super::cap::NET_BIND_SERVICE) {
            return Err(EACCES);
        }
        match &self.proto {
            Proto::Tcp(t) => {
                {
                    let g = t.lock();
                    if g.port.is_some() || !matches!(g.s, Tcp::Idle) {
                        return Err(EINVAL);
                    }
                }
                self.policy(policy::Access::NetBind { proto: policy::NetProto::Tcp, port: a.port })?;
                let me = PortUse { id: self.id(), ip: a.ip, fam: self.fam_of(a.ip), reuse: self.reuse(), listening: false };
                let port = port_take(at, me, a.port)?;
                let mut g = t.lock();
                if g.port.is_some() {
                    drop(g);
                    self.t().port_release(port, self.id());
                    return Err(EINVAL);
                }
                g.port = Some(port);
                g.local = Some(SockAddr { ip: a.ip, port });
                Ok(())
            }
            Proto::Udp(u) => {
                if u.lock().local.is_some() {
                    return Err(EINVAL);
                }
                self.policy(policy::Access::NetBind { proto: policy::NetProto::Udp, port: a.port })?;
                self.udp_bind(at, a, group)
            }
        }
    }

    /// The automatic bind of an unbound UDP socket's first send or connect (`inet_autobind`).
    fn udp_autobind(&self, u: &SpinLock<UdpSt>, at: Place<'_>) -> Result<(), Errno> {
        let _b = self.bind_mx.lock();
        if u.lock().local.is_some() {
            return Ok(());
        }
        self.udp_bind(at, SockAddr { ip: Ipv4::ANY, port: 0 }, None)
    }

    /// The UDP ports a socket of family `fam` holds: the stack's table (IPv4), `UDP6_PORTS` (IPv6), a dual-stack
    /// one both with the same number.
    fn udp_ports(st: &'static Stack, fam: Fam, port: u16) -> Result<(Option<PortGuard>, Option<inet6::Port6Guard>), Errno> {
        match fam {
            Fam::V4 => Ok((Some(crate::stack::udp::reserve_port(st, port).map_err(errno)?), None)),
            Fam::Only6 => Ok((None, Some(inet6::udp6_take(st, port).ok_or(EADDRINUSE)?))),
            Fam::Dual => {
                // Bounded (B4): one pass over the ephemeral range at most.
                for _ in crate::stack::EPHEMERAL {
                    let g = crate::stack::udp::reserve_port(st, port).map_err(errno)?;
                    if let Some(g6) = inet6::udp6_take(st, g.0) {
                        return Ok((Some(g), Some(g6)));
                    }
                    if port != 0 {
                        return Err(EADDRINUSE);
                    }
                }
                Err(EADDRINUSE)
            }
        }
    }

    /// Called with `bind_mx` held.
    fn udp_bind(&self, at: Place<'_>, a: SockAddr, group: Option<[u8; 16]>) -> Result<(), Errno> {
        let Proto::Udp(u) = &self.proto else { return Err(EINVAL) };
        let fam = self.fam_of(a.ip);
        let (guard, guard6, ns_port) = match at {
            Place::Stack(st) => {
                let (g, g6) = Self::udp_ports(st, fam, a.port)?;
                (g, g6, None)
            }
            Place::Ns(v) => (None, None, Some(NsUdp::take(v, lo_slots(fam), a.port)?)),
        };
        let Some(port) = guard.as_ref().map(|g| g.0).or(guard6.as_ref().map(|g| g.0)).or(ns_port.as_ref().map(|g| g.port)) else { return Err(EADDRINUSE) };
        // An IPv6-only socket hears no IPv4 host: it stays off the cards (a namespace has none).
        let smol = match (&guard, at) {
            (Some(g), Place::Stack(st)) if !a.ip.is_loopback() && fam != Fam::Only6 => Some(Arc::new(SmolUdp::bind_prog(st, a.ip, g, self.owner.subject).map_err(errno)?)),
            _ => None,
        };
        // A group-bound socket takes the group's datagrams only: none reach it over the loopback.
        let lo = if group.is_none() && (a.ip.is_loopback() || a.ip.is_unspecified()) {
            let r = Arc::new(LoUdp { q: SpinLock::named("net.inet.lo_udp_q", VecDeque::new()), bytes: AtomicUsize::new(0), rcvbuf: AtomicUsize::new(self.opts.rcvbuf()) });
            let mut t = at.tables().lo_udp.lock();
            for v6 in lo_slots(fam) {
                t.insert((port, *v6), Arc::downgrade(&r));
            }
            Some(r)
        } else {
            None
        };
        let mut g = u.lock();
        if g.local.is_some() {
            drop(g);
            if let Some(r) = &lo {
                at.tables().lo_unhook(port, r, lo_slots(fam));
            }
            return Err(EINVAL);
        }
        g.local = Some(SockAddr { ip: a.ip, port });
        g.guard = guard;
        g.guard6 = guard6;
        g.ns_port = ns_port;
        g.fam = fam;
        g.group6 = group;
        g.smol = smol;
        g.lo = lo;
        Ok(())
    }

    /// getsockname / getpeername (`inet_getname`).
    /// NW-12: what /proc/net/tcp and /proc/net/udp show of this socket (`get_tcp4_sock`, `udp4_format_sock`).
    /// None for what they leave out: AF_INET6 sockets (tcp6/udp6), TCP sockets that neither listen nor connect
    /// (only bound ones are not in Linux's listening or established tables), UDP sockets without a port.
    pub(crate) fn proc_row(&self) -> Option<ProcRow> {
        if self.is_v6() {
            return None;
        }
        let none = SockAddr { ip: Ipv4::ANY, port: 0 };
        let (tcp, local, peer, state) = match &self.proto {
            Proto::Tcp(t) => {
                // Read-only (no `sync`): a reader must not move another program's socket on. A handshake that
                // finished shows as the owner's next call will see it; one that failed is closed (`tcp_done`
                // unhashes it) and not listed.
                let g = t.lock();
                let (state, local) = match &g.s {
                    Tcp::Idle => return None,
                    Tcp::Listening(_) => (TCP_LISTEN, g.local),
                    Tcp::Connecting(c) => {
                        let st = c.state();
                        if st.error.is_some() {
                            return None;
                        }
                        if st.connecting { (TCP_SYN_SENT, g.local) } else { (TCP_ESTABLISHED, Some(c.local())) }
                    }
                    Tcp::Connected(_) => (TCP_ESTABLISHED, g.local),
                };
                (true, local.unwrap_or(none), if state == TCP_LISTEN { none } else { g.peer.unwrap_or(none) }, state)
            }
            Proto::Udp(u) => {
                let g = u.lock();
                let l = g.local.filter(|l| l.port != 0)?;
                // `inet_rcv_saddr`: a connect of a socket bound to 0.0.0.0 fills in the source it chose.
                let ip = match g.src {
                    Some(ip) if l.ip.is_unspecified() => ip,
                    _ => l.ip,
                };
                let state = if g.peer.is_some() { TCP_ESTABLISHED } else { TCP_CLOSE };
                (false, SockAddr { ip, port: l.port }, g.peer.unwrap_or(none), state)
            }
        };
        Some(ProcRow { tcp, local, peer, state, uid: self.owner.uid })
    }

    pub fn getname(&self, peer: bool) -> Result<Vec<u8>, Errno> {
        let a = match &self.proto {
            Proto::Tcp(t) => {
                self.sync(t);
                let g = t.lock();
                if peer {
                    match &g.s {
                        Tcp::Connected(_) => g.peer.ok_or(ENOTCONN)?,
                        _ => return Err(ENOTCONN),
                    }
                } else {
                    g.local.unwrap_or(SockAddr { ip: Ipv4::ANY, port: 0 })
                }
            }
            Proto::Udp(u) => {
                let g = u.lock();
                if peer {
                    g.peer.ok_or(ENOTCONN)?
                } else {
                    let l = g.local.unwrap_or(SockAddr { ip: Ipv4::ANY, port: 0 });
                    if let Some(gr) = g.group6 {
                        return Ok(self.group_name(gr, l.port));
                    }
                    match g.src {
                        Some(ip) if l.ip.is_unspecified() => SockAddr { ip, port: l.port },
                        _ => l,
                    }
                }
            }
        };
        Ok(self.name_of(a))
    }

    // -----------------------------------------------------------------------------------------------------
    // TCP connections

    /// Turns a connection's news into socket state: a finished handshake, a failure (the pending error, the
    /// socket back to idle, its port free), a reset (the pending error, once).
    fn sync(&self, t: &SpinLock<TcpSt>) {
        let mut gone: Option<Box<dyn Conn>> = None;
        let mut free: Option<u16> = None;
        {
            let mut g = t.lock();
            let state = match &g.s {
                Tcp::Connecting(c) | Tcp::Connected(c) => c.state(),
                _ => return,
            };
            match core::mem::replace(&mut g.s, Tcp::Idle) {
                Tcp::Connecting(c) => {
                    if let Some(e) = state.error {
                        g.err = errno(e);
                        CONNECT_FAILED.inc();
                        klog::event!(Debug, "net", "sock.connect_failed", pid = self.owner.pid, peer = alloc::format!("{:?}", g.peer.map(|p| (p.ip.0, p.port))), error = g.err);
                        g.peer = None;
                        // `tcp_done`: both directions shut until a connect() call or a disconnect clears them.
                        g.shutdown = SHUTDOWN_MASK;
                        free = g.port.take();
                        gone = Some(c);
                    } else if !state.connecting {
                        g.local = Some(c.local());
                        c.on_established();
                        g.s = Tcp::Connected(c);
                    } else {
                        g.s = Tcp::Connecting(c);
                    }
                }
                Tcp::Connected(c) => {
                    if let Some(e) = state.error
                        && !g.dead
                    {
                        // A reset: reported once (SO_ERROR or the next call), then both directions are shut.
                        // After the peer's FIN Linux reports EPIPE for it (tcp_reset in CLOSE_WAIT).
                        g.dead = true;
                        g.err = if state.eof { EPIPE } else { errno(e) };
                        klog::event!(Debug, "net", "sock.reset", pid = self.owner.pid, error = g.err);
                        g.shutdown = SHUTDOWN_MASK;
                    }
                    g.s = Tcp::Connected(c);
                }
                other => g.s = other,
            }
        }
        if let Some(p) = free {
            self.t().port_release(p, self.id());
        }
        drop(gone);
    }

    pub fn listen(&self, backlog: usize, _c: &Caller) -> Result<(), Errno> {
        let Proto::Tcp(t) = &self.proto else { return Err(EOPNOTSUPP) };
        let at = self.place()?;
        let _b = self.bind_mx.lock();
        let (local, has_port) = {
            let g = t.lock();
            match &g.s {
                // Linux only adjusts the backlog of a listening socket; the stack keeps the first one.
                Tcp::Listening(_) => return Ok(()),
                Tcp::Idle if !g.in_progress => {}
                _ => return Err(EINVAL),
            }
            (g.local.unwrap_or(SockAddr { ip: Ipv4::ANY, port: 0 }), g.port.is_some())
        };
        self.policy(policy::Access::NetListen { port: local.port })?;
        // Built before `tcp_ports` is locked (`fam_of` takes the option lock).
        let fam = self.fam_of(local.ip);
        let me = PortUse { id: self.id(), ip: local.ip, fam, reuse: self.reuse(), listening: true };
        let port = if has_port {
            // A listener conflicts with every other user of its port (SO_REUSEADDR does not help there).
            if self.t().tcp_ports.lock().get(&local.port).is_some_and(|v| conflict(v, &me)) {
                return Err(EADDRINUSE);
            }
            local.port
        } else {
            port_take(at, me, 0)?
        };
        let (rx, tx) = self.bufs();
        // An IPv6-only :: listens on the loopback only (no IPv6 reaches the cards).
        let (on, side) = match fam {
            Fam::V4 => (local.ip, Side::V4),
            Fam::Dual => (local.ip, Side::Both),
            Fam::Only6 => (LO6, Side::V6),
        };
        let made = match at {
            Place::Stack(st) => st.listen_prog(SockAddr { ip: on, port }, side, backlog, self.owner.subject, rx, tx),
            Place::Ns(v) => ProgListener::on_loopback(&v.lo, SockAddr { ip: on, port }, side),
        };
        let l = match made {
            Ok(l) => l,
            Err(e) => {
                if !has_port {
                    self.t().port_release(port, self.id());
                }
                return Err(errno(e));
            }
        };
        self.t().port_update(port, self.id(), |u| u.listening = true);
        // `inet_csk_listen_start` clears the ACK state, pingpong included (inet_connection_sock.c:1247).
        self.opts.set(flag::PINGPONG, false);
        let mut g = t.lock();
        g.port = Some(port);
        g.local = Some(SockAddr { ip: local.ip, port });
        g.s = Tcp::Listening(l);
        Ok(())
    }

    pub fn accept(&self, nonblock: bool, _c: &Caller) -> Result<(Arc<InetSock>, Vec<u8>), Errno> {
        let Proto::Tcp(t) = &self.proto else { return Err(EOPNOTSUPP) };
        let dl = Deadline::new(nonblock, self.opts.rcvtimeo.load(Ordering::Relaxed));
        loop {
            let got = {
                let g = t.lock();
                match &g.s {
                    Tcp::Listening(l) => l.try_accept(),
                    _ => return Err(EINVAL),
                }
            };
            if let Some(conn) = got {
                conn.set_owner(self.owner.subject);
                if self.opts.has(flag::NODELAY) {
                    conn.set_nodelay(true);
                }
                if self.opts.has(flag::KEEPALIVE) {
                    conn.set_keepalive(true);
                }
                // No PINGPONG here: listen() cleared the listener's (`inet_csk_delack_init`), and an accepted
                // socket starts with fresh options.
                let (local, peer) = (conn.local(), conn.peer());
                let n = InetSock::create_in(self.is_v6(), SOCK_STREAM, 0, &self.owner)?;
                n.opts.copy_from(&self.opts);
                let inherited = self.ip_opts.lock().clone();
                *n.ip_opts.lock() = inherited;
                if let Proto::Tcp(nt) = &n.proto {
                    let mut g = nt.lock();
                    g.local = Some(local);
                    g.peer = Some(peer);
                    g.s = Tcp::Connected(conn);
                    g.port = Some(local.port);
                }
                self.t().port_add(local.port, PortUse { id: n.id(), ip: local.ip, fam: n.fam_of(local.ip), reuse: self.reuse(), listening: false });
                return Ok((n, self.name_of(peer)));
            }
            let ready = || {
                let g = t.lock();
                match &g.s {
                    Tcp::Listening(l) => l.pending() > 0,
                    _ => true,
                }
            };
            if !wait(&READY, dl, ready)? {
                return Err(EAGAIN);
            }
        }
    }

    pub fn connect(&self, raw: &[u8], _c: &Caller, nonblock: bool) -> Result<(), Errno> {
        if raw.len() < 2 {
            return Err(EINVAL);
        }
        if family(raw) == Some(AF_UNSPEC) {
            return self.disconnect();
        }
        match &self.proto {
            Proto::Tcp(t) => self.tcp_connect(t, raw, nonblock),
            Proto::Udp(u) => {
                let mut to = if self.is_v6() { self.connect6(raw)? } else { Self::sin(raw, false)? };
                if to.ip.is_unspecified() {
                    to.ip = Ipv4::LOCALHOST;
                }
                let at = self.place()?;
                match at {
                    Place::Stack(st) => {
                        if !to.ip.is_loopback() && to.ip != Ipv4::BROADCAST && st.route_on(to.ip, self.bound_card()).is_none() {
                            return Err(ENETUNREACH);
                        }
                    }
                    // A namespace routes 127/8 and ::1 only (`ns_route`).
                    Place::Ns(v) => {
                        Place::ns_route(v, to.ip, self.is_v6() && to.ip == LO6)?;
                    }
                }
                self.policy(policy::Access::NetConnect { proto: policy::NetProto::Udp, addr: inet6::policy_ip(to.ip), port: to.port })?;
                self.udp_autobind(u, at)?;
                // `ip4_datagram_connect` / `ip6_datagram_connect` pick the source now.
                let src = if to.ip == LO6 {
                    Some(LO6)
                } else if to.ip.is_loopback() {
                    Some(Ipv4::LOCALHOST)
                } else if let Place::Stack(st) = at {
                    st.route_on(to.ip, self.bound_card()).and_then(|(i, _)| i.info().addr.map(|(a, _)| a))
                } else if let Place::Ns(v) = at {
                    Some(Ipv4(v.with_dev(|d| d.src_for(to.ip.0))))
                } else {
                    None
                };
                let mut g = u.lock();
                g.peer = Some(to);
                g.allowed = Some(to);
                g.src = src;
                Ok(())
            }
        }
    }

    fn disconnect(&self) -> Result<(), Errno> {
        match &self.proto {
            Proto::Tcp(t) => {
                let old = {
                    let mut g = t.lock();
                    g.in_progress = false;
                    g.peer = None;
                    core::mem::replace(&mut g.s, Tcp::Idle)
                };
                if let Tcp::Connecting(c) | Tcp::Connected(c) = &old {
                    c.shutdown();
                }
                drop(old);
                notify();
                Ok(())
            }
            Proto::Udp(u) => {
                let mut g = u.lock();
                g.peer = None;
                g.src = None;
                Ok(())
            }
        }
    }

    fn tcp_connect(&self, t: &SpinLock<TcpSt>, raw: &[u8], nonblock: bool) -> Result<(), Errno> {
        self.sync(t);
        let connecting = {
            let mut g = t.lock();
            match &g.s {
                Tcp::Connected(_) if g.in_progress => {
                    // The non-blocking connect finished: this call reports it (Linux's SS_CONNECTING -> CONNECTED).
                    g.in_progress = false;
                    return Ok(());
                }
                Tcp::Connected(_) | Tcp::Listening(_) => return Err(EISCONN),
                Tcp::Connecting(_) if nonblock => return Err(EALREADY),
                Tcp::Connecting(_) => true,
                Tcp::Idle if g.in_progress => {
                    // It failed meanwhile: the pending error is the answer (ECONNABORTED once it was read), and
                    // the socket is disconnected (`tcp_disconnect` clears the shutdown bits).
                    g.in_progress = false;
                    g.shutdown = 0;
                    let e = core::mem::take(&mut g.err);
                    return Err(if e != 0 { e } else { ECONNABORTED });
                }
                Tcp::Idle => false,
            }
        };
        if !connecting {
            let mut to = if self.is_v6() { self.connect6(raw)? } else { Self::sin(raw, false)? };
            if to.ip.is_unspecified() {
                to.ip = Ipv4::LOCALHOST;
            }
            let at = self.place()?;
            self.policy(policy::Access::NetConnect { proto: policy::NetProto::Tcp, addr: inet6::policy_ip(to.ip), port: to.port })?;
            let bound = t.lock().port;
            let conn: Result<Box<dyn Conn>, NetError> = if to.ip.is_loopback() {
                let v6_lo = self.is_v6() && to.ip == LO6;
                let lo = at.lo(to.ip, v6_lo)?;
                if v6_lo {
                    // ::1 talks to ::1 on the loopback's IPv6 side: both ends of the pair carry the stand-in.
                    lo.connect_side(to, LO6, true)
                } else {
                    // IPv4 (also an AF_INET program's own 127.0.6.1): the IPv4 side.
                    lo.connect_conn(to)
                }
            } else if let Place::Stack(st) = at {
                let (rx, tx) = self.bufs();
                crate::stack::tcp::connect_start(st, to, bound, rx, tx, self.owner.subject, self.bound_card()).map(|s| Box::new(s) as Box<dyn Conn>)
            } else if let Place::Ns(v) = at {
                // NW-12: lo's other addresses (outside 127/8) are the namespace's loopback too.
                let lo = at.lo(to.ip, false)?;
                lo.connect_from(to, Ipv4(v.with_dev(|d| d.src_for(to.ip.0))), false)
            } else {
                Err(NetError::Unreachable)
            };
            let conn = match conn {
                Ok(c) => c,
                Err(NetError::ConnectionRefused) if (to.ip.is_loopback() || matches!(at, Place::Ns(_))) && nonblock => {
                    // A loopback refusal is immediate on Linux too; a non-blocking caller still sees EINPROGRESS
                    // and finds ECONNREFUSED in SO_ERROR.
                    let mut g = t.lock();
                    g.err = ECONNREFUSED;
                    g.in_progress = true;
                    g.shutdown = SHUTDOWN_MASK;
                    drop(g);
                    notify();
                    return Err(EINPROGRESS);
                }
                Err(NetError::AddrInUse) => return Err(EADDRNOTAVAIL),
                Err(e) => return Err(errno(e)),
            };
            if self.opts.has(flag::NODELAY) {
                conn.set_nodelay(true);
            }
            if self.opts.has(flag::KEEPALIVE) {
                conn.set_keepalive(true);
            }
            // TCP_QUICKACK 0 before connect() stays (Linux keeps pingpong over the handshake, tcp_input.c:6348).
            if self.opts.has(flag::PINGPONG) {
                conn.set_quickack(false);
            }
            let local = conn.local();
            let had_port = t.lock().port.is_some();
            if !had_port {
                self.t().port_add(local.port, PortUse { id: self.id(), ip: local.ip, fam: self.fam_of(local.ip), reuse: self.reuse(), listening: false });
            }
            let mut g = t.lock();
            if !had_port {
                g.port = Some(local.port);
            }
            g.local = Some(local);
            g.peer = Some(to);
            g.s = Tcp::Connecting(conn);
            g.in_progress = true;
        }
        if nonblock {
            self.sync(t);
            return Err(EINPROGRESS);
        }
        let dl = Deadline::new(false, self.opts.sndtimeo.load(Ordering::Relaxed));
        let quiet = Quiet::new();
        let done = || match &t.lock().s {
            Tcp::Connecting(c) => {
                quiet.look(c.change_seq(), || {
                    let s = c.state();
                    s.error.is_some() || !s.connecting
                })
            }
            _ => true,
        };
        let ok = wait(&READY, dl, done)?;
        self.sync(t);
        if !ok {
            return Err(EINPROGRESS);
        }
        let mut g = t.lock();
        g.in_progress = false;
        match g.s {
            Tcp::Connected(_) => Ok(()),
            _ => {
                g.shutdown = 0;
                let e = core::mem::take(&mut g.err);
                Err(if e != 0 { e } else { ECONNABORTED })
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // data

    pub fn send(&self, src: &mut dyn Source, len: usize, flags: u32, nonblock: bool, to: Option<&[u8]>) -> Result<usize, Errno> {
        match &self.proto {
            Proto::Tcp(t) => self.tcp_send(t, src, len, flags, nonblock),
            Proto::Udp(u) => self.udp_send(u, src, len, flags, nonblock, to),
        }
    }

    fn tcp_send(&self, t: &SpinLock<TcpSt>, src: &mut dyn Source, len: usize, flags: u32, nonblock: bool) -> Result<usize, Errno> {
        // Urgent data is not supported; Linux checks the connection first (EPIPE on an unconnected socket).
        if flags & MSG_OOB != 0 && matches!(t.lock().s, Tcp::Connected(_)) {
            return Err(EOPNOTSUPP);
        }
        let dl = Deadline::new(nonblock || flags & MSG_DONTWAIT != 0, self.opts.sndtimeo.load(Ordering::Relaxed));
        let pipe = |sent: usize| -> Result<usize, Errno> {
            if sent > 0 {
                return Ok(sent);
            }
            if flags & MSG_NOSIGNAL == 0 {
                super::sigpipe();
            }
            Err(EPIPE)
        };
        enum Step {
            Fill,
            Sent(Result<usize, NetError>, bool),
            Wait,
        }
        let mut sent = 0usize;
        let mut chunk: Vec<u8> = Vec::new();
        let mut have = 0usize;
        loop {
            self.sync(t);
            let step = {
                let mut g = t.lock();
                if g.err != 0 {
                    let e = core::mem::take(&mut g.err);
                    return if sent > 0 { Ok(sent) } else { Err(e) };
                }
                if g.shutdown & SEND_SHUTDOWN != 0 {
                    drop(g);
                    return pipe(sent);
                }
                match &g.s {
                    Tcp::Connecting(_) => Step::Wait,
                    Tcp::Connected(c) => {
                        if sent >= len {
                            return Ok(sent);
                        }
                        if have == 0 {
                            Step::Fill
                        } else {
                            let r = c.try_send(&chunk[..have]);
                            let eof = r.is_err() && c.state().eof;
                            Step::Sent(r, eof)
                        }
                    }
                    _ => {
                        drop(g);
                        return pipe(sent);
                    }
                }
            };
            match step {
                Step::Fill => {
                    // The bytes come from the program outside the lock (a fault may sleep).
                    let n = (len - sent).min(CHUNK);
                    if chunk.len() < n {
                        chunk = super::buffer(n)?;
                    }
                    if let Err(e) = src.fill(&mut chunk[..n]) {
                        return if sent > 0 { Ok(sent) } else { Err(e) };
                    }
                    have = n;
                    continue;
                }
                Step::Sent(Ok(n), _) if n > 0 => {
                    sent += n;
                    chunk.copy_within(n..have, 0);
                    have -= n;
                    continue;
                }
                Step::Sent(Err(e), eof) => {
                    return match e {
                        NetError::Closed => pipe(sent),
                        // A reset after the peer's FIN is EPIPE on Linux (tcp_reset in CLOSE_WAIT).
                        NetError::ConnectionReset if eof => pipe(sent),
                        e if sent > 0 => {
                            let _ = e;
                            Ok(sent)
                        }
                        e => Err(errno(e)),
                    };
                }
                _ => {}
            }
            // No room (or still connecting): wait.
            if dl.nonblocking() {
                return if sent > 0 { Ok(sent) } else { Err(EAGAIN) };
            }
            // Another thread's shutdown(SHUT_WR) or a pending error ends the wait too (`sk_stream_wait_memory`).
            let quiet = Quiet::new();
            let ready = || {
                let g = t.lock();
                if g.shutdown & SEND_SHUTDOWN != 0 || g.err != 0 || g.dead {
                    return true;
                }
                match &g.s {
                    Tcp::Connected(c) => {
                        quiet.look(c.change_seq(), || {
                            let s = c.state();
                            s.writable || s.error.is_some() || s.closed
                        })
                    }
                    Tcp::Connecting(c) => !c.state().connecting,
                    _ => true,
                }
            };
            match wait(&READY, dl, ready) {
                Ok(true) => {}
                Ok(false) => return if sent > 0 { Ok(sent) } else { Err(EAGAIN) },
                Err(e) => return if sent > 0 { Ok(sent) } else { Err(e) },
            }
        }
    }

    pub fn recv(&self, size: usize, flags: u32, nonblock: bool, sink: &mut dyn FnMut(&[u8]) -> Result<(), Errno>) -> Result<Received, Errno> {
        match &self.proto {
            Proto::Tcp(t) => self.tcp_recv(t, size, flags, nonblock, sink),
            Proto::Udp(u) => self.udp_recv(u, size, flags, nonblock, sink),
        }
    }

    fn tcp_recv(&self, t: &SpinLock<TcpSt>, size: usize, flags: u32, nonblock: bool, sink: &mut dyn FnMut(&[u8]) -> Result<(), Errno>) -> Result<Received, Errno> {
        if flags & MSG_OOB != 0 {
            return Err(EINVAL);
        }
        let peek = flags & MSG_PEEK != 0;
        let target = if flags & MSG_WAITALL != 0 && !peek { size } else { (self.opts.rcvlowat.load(Ordering::Relaxed) as usize).min(size) }.max(1);
        let dl = Deadline::new(nonblock || flags & MSG_DONTWAIT != 0, self.opts.rcvtimeo.load(Ordering::Relaxed));
        let mut copied = 0usize;
        let mut buf = super::buffer(size.clamp(1, CHUNK))?;
        enum Got {
            Data(usize),
            Eof,
            Empty,
            Err(Errno),
        }
        loop {
            self.sync(t);
            // Readers one at a time from the peek to the discard (Linux `lock_sock`), never while waiting.
            let reader = (!peek).then(|| self.rd.lock());
            let got = {
                let mut g = t.lock();
                match &g.s {
                    Tcp::Listening(_) => return Err(ENOTCONN),
                    Tcp::Idle => {
                        if g.err != 0 {
                            Got::Err(core::mem::take(&mut g.err))
                        } else if g.shutdown & RCV_SHUTDOWN != 0 {
                            Got::Eof
                        } else if copied > 0 {
                            Got::Eof
                        } else {
                            return Err(ENOTCONN);
                        }
                    }
                    Tcp::Connecting(_) => Got::Empty,
                    Tcp::Connected(_) if g.dead => {
                        if g.err != 0 {
                            Got::Err(core::mem::take(&mut g.err))
                        } else {
                            Got::Eof
                        }
                    }
                    Tcp::Connected(c) => {
                        let want = (size - copied).min(buf.len());
                        // Always a peek: the bytes leave the queue only after the program has them.
                        match c.try_recv(&mut buf[..want], true) {
                            Ok(Some(0)) => Got::Eof,
                            Ok(Some(n)) => Got::Data(n),
                            Ok(None) if g.err != 0 => Got::Err(core::mem::take(&mut g.err)),
                            Ok(None) if g.shutdown & RCV_SHUTDOWN != 0 => Got::Eof,
                            Ok(None) => Got::Empty,
                            Err(e) => {
                                g.err = 0;
                                Got::Err(errno(e))
                            }
                        }
                    }
                }
            };
            match got {
                Got::Data(n) => {
                    if flags & MSG_TRUNC == 0
                        && let Err(e) = sink(&buf[..n])
                    {
                        // `tcp_recvmsg`: a failed copy consumes nothing.
                        return if copied > 0 { Ok(Received { n: copied, ..Default::default() }) } else { Err(e) };
                    }
                    if !peek {
                        let g = t.lock();
                        if let Tcp::Connected(c) = &g.s {
                            let _ = c.discard(n);
                        }
                    }
                    drop(reader);
                    copied += n;
                    if peek || copied >= size {
                        break;
                    }
                }
                Got::Eof => break,
                Got::Err(e) => {
                    if copied > 0 {
                        break;
                    }
                    return Err(e);
                }
                Got::Empty => {
                    drop(reader);
                    if copied >= target {
                        break;
                    }
                    if dl.nonblocking() {
                        if copied > 0 {
                            break;
                        }
                        return Err(EAGAIN);
                    }
                    // Another thread's shutdown(SHUT_RD) ends the wait with EOF (`tcp_recvmsg` looks at
                    // RCV_SHUTDOWN after every wake).
                    let quiet = Quiet::new();
                    let ready = || {
                        let g = t.lock();
                        if g.shutdown & RCV_SHUTDOWN != 0 || g.err != 0 || g.dead {
                            return true;
                        }
                        match &g.s {
                            Tcp::Connected(c) => {
                                quiet.look(c.change_seq(), || {
                                    let s = c.state();
                                    s.readable || s.eof || s.error.is_some()
                                })
                            }
                            Tcp::Connecting(c) => !c.state().connecting,
                            _ => true,
                        }
                    };
                    match wait(&READY, dl, ready) {
                        Ok(true) => {}
                        Ok(false) | Err(_) if copied > 0 => break,
                        Ok(false) => return Err(EAGAIN),
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(Received { n: copied, ..Default::default() })
    }

    fn udp_send(&self, u: &SpinLock<UdpSt>, src: &mut dyn Source, len: usize, flags: u32, nonblock: bool, to: Option<&[u8]>) -> Result<usize, Errno> {
        if flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        let given = match to.filter(|t| !t.is_empty()) {
            Some(raw) if self.is_v6() => self.sendto6(raw)?,
            other => other.map(|raw| -> Result<SockAddr, Errno> {
                let a = parse_in(raw).ok_or(EINVAL)?;
                if !matches!(family(raw), Some(AF_INET) | Some(AF_UNSPEC)) {
                    return Err(EAFNOSUPPORT);
                }
                if a.port == 0 {
                    return Err(EINVAL);
                }
                Ok(a)
            }).transpose()?,
        };
        let mut dest = match given {
            Some(a) => a,
            None => {
                let mut g = u.lock();
                if g.err != 0 {
                    return Err(core::mem::take(&mut g.err));
                }
                g.peer.ok_or(EDESTADDRREQ)?
            }
        };
        if len > if dest.ip == LO6 { inet6::UDP6_MAX } else { UDP_MAX } {
            return Err(EMSGSIZE);
        }
        if u.lock().shutdown & SEND_SHUTDOWN != 0 {
            if flags & MSG_NOSIGNAL == 0 {
                super::sigpipe();
            }
            return Err(EPIPE);
        }
        if dest.ip.is_unspecified() {
            dest.ip = Ipv4::LOCALHOST;
        }
        if dest.ip == Ipv4::BROADCAST && !self.opts.has(flag::BROADCAST) {
            return Err(EACCES);
        }
        if u.lock().allowed != Some(dest) {
            self.policy(policy::Access::NetConnect { proto: policy::NetProto::Udp, addr: inet6::policy_ip(dest.ip), port: dest.port })?;
            u.lock().allowed = Some(dest);
        }
        let at = self.place()?;
        // A namespace routes 127/8 and ::1 only; a datagram for a lo that is down is dropped (`ns_route`).
        let deliver = match at {
            Place::Ns(v) => Place::ns_route(v, dest.ip, self.is_v6() && dest.ip == LO6)?,
            Place::Stack(_) => true,
        };
        self.udp_autobind(u, at)?;
        let mut data = super::buffer(len)?;
        src.fill(&mut data)?;
        if !deliver {
            return Ok(len);
        }
        let (local, smol, connected) = {
            let g = u.lock();
            (g.local.unwrap_or(SockAddr { ip: Ipv4::ANY, port: 0 }), g.smol.clone(), g.peer == Some(dest))
        };
        if dest.ip.is_loopback() || matches!(at, Place::Ns(_)) {
            // An AF_INET6 socket to ::1 sends from ::1 on the IPv6 side; to 127/8 the source is the bound address
            // or 127.0.0.1 (an AF_INET program's own 127.0.6.1 included); NW-12: to lo's other addresses in a
            // namespace the route's source.
            let v6 = self.is_v6() && dest.ip == LO6;
            let ip = if v6 {
                LO6
            } else if local.ip.is_unspecified() || (self.is_v6() && local.ip == LO6) {
                match at {
                    Place::Ns(v) if !dest.ip.is_loopback() => Ipv4(v.with_dev(|d| d.src_for(dest.ip.0))),
                    _ => Ipv4::LOCALHOST,
                }
            } else {
                local.ip
            };
            let from = SockAddr { ip, port: local.port };
            if !at.tables().lo_deliver(&data, from, dest, v6) && connected {
                // Linux's ICMP port unreachable reaches a connected socket at once: its next call sees it.
                u.lock().err = ECONNREFUSED;
                notify();
            }
            return Ok(len);
        }
        let smol = smol.ok_or(ENETUNREACH)?;
        let dl = Deadline::new(nonblock || flags & MSG_DONTWAIT != 0, self.opts.sndtimeo.load(Ordering::Relaxed));
        loop {
            match smol.try_send_prog(&data, dest, self.bound_card()) {
                Ok(true) => return Ok(len),
                Ok(false) => {}
                Err(NetError::Other) => return Err(EMSGSIZE),
                Err(e) => return Err(errno(e)),
            }
            if dl.nonblocking() || dl.expired() {
                return Err(EAGAIN);
            }
            // The card's send queue frees without a socket event: look again shortly.
            let _ = wait(&READY, Deadline(Some(super::now().saturating_add(UDP_RETRY_NS))), || false)?;
        }
    }

    fn udp_recv(&self, u: &SpinLock<UdpSt>, size: usize, flags: u32, nonblock: bool, sink: &mut dyn FnMut(&[u8]) -> Result<(), Errno>) -> Result<Received, Errno> {
        if flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        let peek = flags & MSG_PEEK != 0;
        let dl = Deadline::new(nonblock || flags & MSG_DONTWAIT != 0, self.opts.rcvtimeo.load(Ordering::Relaxed));
        loop {
            let (lo, smol, peer) = {
                let mut g = u.lock();
                if g.err != 0 {
                    return Err(core::mem::take(&mut g.err));
                }
                (g.lo.clone(), g.smol.clone(), g.peer)
            };
            let mut got: Option<(Vec<u8>, usize, SockAddr, ctl::Arrival)> = None;
            if let Some(l) = &lo {
                got = lo_take(l, peer, size, peek)?;
            }
            if got.is_none()
                && let Some(s) = &smol
            {
                while let Some(g) = s.try_recv_prog(size, peek) {
                    if peer.is_some_and(|p| p != g.from) {
                        if peek {
                            let _ = s.try_recv_prog(0, false);
                        }
                        continue;
                    }
                    let unicast = !g.to.is_unspecified() && g.to != Ipv4::BROADCAST && !(224..240).contains(&g.to.0[0]);
                    let spec = if unicast { g.to } else { g.card.unwrap_or(g.to) };
                    got = Some((g.data, g.len, g.from, ctl::Arrival { to: g.to, v6: false, ifindex: g.ifindex, spec }));
                    break;
                }
            }
            if let Some((d, len, from, at)) = got {
                sink(&d)?;
                let name = if self.is_v6() { inet6::encode_in6_as(from, at.v6) } else { encode_in(from) };
                let mut r = Received { n: if flags & MSG_TRUNC != 0 { len } else { d.len() }, from: Some(name), ctl: self.recv_ctl(&at), ..Default::default() };
                if d.len() < len {
                    r.flags |= MSG_TRUNC;
                }
                return Ok(r);
            }
            if u.lock().shutdown & RCV_SHUTDOWN != 0 && !dl.nonblocking() {
                return Ok(Received::default());
            }
            if dl.nonblocking() {
                return Err(EAGAIN);
            }
            let quiet = Quiet::new();
            let ready = || {
                let g = u.lock();
                g.err != 0 || g.shutdown & RCV_SHUTDOWN != 0 || g.lo.as_ref().is_some_and(|l| !l.q.lock().is_empty()) || g.smol.as_ref().is_some_and(|s| quiet.look(s.change_seq(), || s.readable()))
            };
            if !wait(&READY, dl, ready)? {
                return Err(EAGAIN);
            }
        }
    }

    pub fn shutdown(&self, how: u64) -> Result<(), Errno> {
        if how > SHUT_RDWR {
            return Err(EINVAL);
        }
        let mode = (how + 1) as u8;
        match &self.proto {
            Proto::Tcp(t) => {
                self.sync(t);
                let mut stop: Option<Tcp> = None;
                let r = {
                    let mut g = t.lock();
                    g.shutdown |= mode;
                    match &g.s {
                        Tcp::Idle => Err(ENOTCONN),
                        Tcp::Listening(_) if mode & RCV_SHUTDOWN != 0 => {
                            stop = Some(core::mem::replace(&mut g.s, Tcp::Idle));
                            g.shutdown = 0;
                            Ok(())
                        }
                        Tcp::Listening(_) => Ok(()),
                        Tcp::Connecting(_) => {
                            stop = Some(core::mem::replace(&mut g.s, Tcp::Idle));
                            g.shutdown = 0;
                            g.in_progress = false;
                            Ok(())
                        }
                        Tcp::Connected(c) => {
                            if mode & SEND_SHUTDOWN != 0 {
                                c.finish();
                            }
                            Ok(())
                        }
                    }
                };
                if let Some(Tcp::Connecting(c)) = &stop {
                    c.shutdown();
                }
                drop(stop);
                notify();
                r
            }
            Proto::Udp(u) => {
                let mut g = u.lock();
                g.shutdown |= mode;
                let r = if g.peer.is_none() { Err(ENOTCONN) } else { Ok(()) };
                drop(g);
                notify();
                r
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // readiness, errors, options, release

    /// `tcp_poll` for an established connection.
    fn connected_mask(s: &crate::conn::ConnState, shutdown: u8) -> u32 {
        use vfs::Events as E;
        let mut m = 0;
        let rd = s.eof || shutdown & RCV_SHUTDOWN != 0;
        if s.readable || rd {
            m |= E::IN;
        }
        if rd {
            m |= E::RDHUP;
        }
        if (s.eof && s.closed) || shutdown == SHUTDOWN_MASK {
            m |= E::HUP;
        }
        if s.writable || shutdown & SEND_SHUTDOWN != 0 || s.closed {
            m |= E::OUT;
        }
        m
    }

    /// `tcp_poll` / `udp_poll` (`datagram_poll`).
    pub fn poll(&self) -> u32 {
        use vfs::Events as E;
        match &self.proto {
            Proto::Tcp(t) => {
                // No state change here (FS-4's epoll may ask from inside a wait condition): a connection's news is
                // read as `sync` would turn it.
                let g = t.lock();
                let mut m = 0;
                if g.err != 0 {
                    m |= E::ERR;
                }
                // A failed handshake or a reset not yet turned into the pending error: tcp_done's mask.
                let failed = E::ERR | E::HUP | E::OUT | E::IN | E::RDHUP;
                match &g.s {
                    Tcp::Listening(l) => {
                        if l.pending() > 0 {
                            m |= E::IN;
                        }
                    }
                    Tcp::Idle => {
                        // A fresh or failed socket: HUP and writable (tcp_poll's TCP_CLOSE rule).
                        m |= E::HUP | E::OUT;
                        if g.shutdown & RCV_SHUTDOWN != 0 {
                            m |= E::IN | E::RDHUP;
                        }
                    }
                    Tcp::Connecting(c) => {
                        let s = c.state();
                        if s.error.is_some() {
                            m |= failed;
                        } else if !s.connecting {
                            m |= Self::connected_mask(&s, g.shutdown);
                        }
                    }
                    Tcp::Connected(c) => {
                        let s = c.state();
                        if s.error.is_some() && !g.dead {
                            m |= failed;
                        } else {
                            m |= Self::connected_mask(&s, g.shutdown);
                        }
                    }
                }
                m
            }
            Proto::Udp(u) => {
                let g = u.lock();
                let mut m = E::OUT;
                if g.err != 0 {
                    m |= E::ERR;
                }
                if g.shutdown & RCV_SHUTDOWN != 0 {
                    m |= E::IN | E::RDHUP;
                }
                if g.shutdown == SHUTDOWN_MASK {
                    m |= E::HUP;
                }
                if g.lo.as_ref().is_some_and(|l| !l.q.lock().is_empty()) || g.smol.as_ref().is_some_and(|s| s.readable()) {
                    m |= E::IN;
                }
                m
            }
        }
    }

    pub fn take_error(&self) -> i64 {
        match &self.proto {
            Proto::Tcp(t) => {
                self.sync(t);
                core::mem::take(&mut t.lock().err)
            }
            Proto::Udp(u) => core::mem::take(&mut u.lock().err),
        }
    }

    pub fn set_error(&self, e: i64) {
        match &self.proto {
            Proto::Tcp(t) => t.lock().err = e,
            Proto::Udp(u) => u.lock().err = e,
        }
        notify();
    }

    /// SIOCINQ (EINVAL on a listener, as Linux).
    pub fn inq(&self) -> Result<i64, Errno> {
        Ok(match &self.proto {
            Proto::Tcp(t) => match &t.lock().s {
                Tcp::Listening(_) => return Err(EINVAL),
                Tcp::Connected(c) => c.recv_queue() as i64,
                _ => 0,
            },
            Proto::Udp(u) => {
                let g = u.lock();
                let lo = g.lo.as_ref().and_then(|l| l.q.lock().front().map(|x| x.data.len()));
                lo.or_else(|| g.smol.as_ref().map(|s| s.next_len())).unwrap_or(0) as i64
            }
        })
    }

    pub fn outq(&self) -> Result<i64, Errno> {
        Ok(match &self.proto {
            Proto::Tcp(t) => match &t.lock().s {
                Tcp::Listening(_) => return Err(EINVAL),
                Tcp::Connected(c) => c.send_queue() as i64,
                _ => 0,
            },
            Proto::Udp(_) => 0,
        })
    }

    /// SO_KEEPALIVE, TCP_NODELAY, TCP_QUICKACK and SO_RCVBUF reach what exists already.
    pub fn options_changed(&self) {
        match &self.proto {
            Proto::Tcp(t) => {
                if let Tcp::Connecting(c) | Tcp::Connected(c) = &t.lock().s {
                    c.set_keepalive(self.opts.has(flag::KEEPALIVE));
                    c.set_nodelay(self.opts.has(flag::NODELAY));
                    c.set_quickack(!self.opts.has(flag::PINGPONG));
                }
            }
            Proto::Udp(u) => {
                if let Some(l) = &u.lock().lo {
                    l.rcvbuf.store(self.opts.rcvbuf(), Ordering::Relaxed);
                }
            }
        }
    }

    /// Non-SOL_SOCKET options: SOL_TCP on TCP (`do_tcp_setsockopt`), SOL_UDP on UDP, SOL_IP on both
    /// (`ip_setsockopt`, common options kept and given back). `val`: the first bytes the program gave.
    pub fn setsockopt(&self, level: u32, name: u32, optlen: usize, val: Result<Vec<u8>, Errno>, c: &Caller) -> Result<(), Errno> {
        let tcp = matches!(self.proto, Proto::Tcp(_));
        let int = |val: Result<Vec<u8>, Errno>| -> Result<i32, Errno> {
            if optlen < 4 {
                return Err(EINVAL);
            }
            let v = val?;
            Ok(i32::from_le_bytes(v.get(..4).ok_or(EFAULT)?.try_into().unwrap_or([0; 4])))
        };
        match level {
            SOL_TCP if tcp => {
                let v = int(val)?;
                match name {
                    TCP_NODELAY => {
                        self.opts.set(flag::NODELAY, v != 0);
                        self.options_changed();
                    }
                    TCP_CORK => self.opts.set(flag::CORK, v != 0),
                    // `__tcp_sock_set_quickack` (linux-6.6/net/ipv4/tcp.c:3259): 0 enters pingpong mode, else leaves it.
                    TCP_QUICKACK => {
                        self.opts.set(flag::PINGPONG, v == 0);
                        self.options_changed();
                    }
                    TCP_MAXSEG if !(88..=65535).contains(&v) && v != 0 => return Err(EINVAL),
                    TCP_KEEPIDLE | TCP_KEEPINTVL if !(1..=32767).contains(&v) => return Err(EINVAL),
                    TCP_KEEPCNT if !(1..=127).contains(&v) => return Err(EINVAL),
                    TCP_MAXSEG | TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT => {
                        self.ip_opts.lock().insert((level, name), v);
                    }
                    _ => return Err(ENOPROTOOPT),
                }
                Ok(())
            }
            SOL_IP => {
                if !IP_OPTS.iter().any(|(n, _)| *n == name) {
                    return Err(ENOPROTOOPT);
                }
                // ip_setsockopt takes a byte for the small options and an int otherwise.
                let v = if optlen >= 4 {
                    int(val)?
                } else if optlen >= 1 {
                    val?.first().copied().unwrap_or(0) as i32
                } else {
                    return Err(EINVAL);
                };
                self.ip_opts.lock().insert((level, name), v);
                Ok(())
            }
            SOL_IPV6 if self.is_v6() => self.setsockopt6(name, optlen, val, c),
            SOL_UDP if !tcp => {
                let v = int(val)?;
                // UDP_CORK (1), UDP_SEGMENT (103), UDP_GRO (104): kept, not acted on.
                if ![1, 103, 104].contains(&name) {
                    return Err(ENOPROTOOPT);
                }
                self.ip_opts.lock().insert((level, name), v);
                Ok(())
            }
            _ => Err(ENOPROTOOPT),
        }
    }

    pub fn getsockopt(&self, level: u32, name: u32) -> Result<Vec<u8>, Errno> {
        let tcp = matches!(self.proto, Proto::Tcp(_));
        let stored = |d: i32| self.ip_opts.lock().get(&(level, name)).copied().unwrap_or(d);
        let v = match level {
            SOL_TCP if tcp => match name {
                TCP_NODELAY => self.opts.has(flag::NODELAY) as i32,
                TCP_CORK => self.opts.has(flag::CORK) as i32,
                TCP_QUICKACK => !self.opts.has(flag::PINGPONG) as i32,
                TCP_MAXSEG => stored(536),
                TCP_KEEPIDLE => stored(7200),
                TCP_KEEPINTVL => stored(75),
                TCP_KEEPCNT => stored(9),
                _ => return Err(ENOPROTOOPT),
            },
            SOL_IP => match IP_OPTS.iter().find(|(n, _)| *n == name) {
                Some((_, d)) => stored(*d),
                None => return Err(ENOPROTOOPT),
            },
            SOL_UDP if !tcp && [1, 103, 104].contains(&name) => stored(0),
            SOL_UDP if !tcp => return Err(ENOPROTOOPT),
            SOL_IPV6 if self.is_v6() => return self.getsockopt6(name),
            // A level the protocol does not know: `ip_getsockopt` says EOPNOTSUPP (its setsockopt ENOPROTOOPT),
            // `ipv6_getsockopt` ENOPROTOOPT.
            _ => return Err(if self.is_v6() { ENOPROTOOPT } else { EOPNOTSUPP }),
        };
        Ok(v.to_le_bytes().to_vec())
    }

    /// The last fd went: a connection closes gracefully (SO_LINGER with 0 s: reset), a listener resets what
    /// waits, ports go back.
    pub fn release(&self) {
        match &self.proto {
            Proto::Tcp(t) => {
                let (s, port) = {
                    let mut g = t.lock();
                    (core::mem::replace(&mut g.s, Tcp::Idle), g.port.take())
                };
                let linger0 = self.opts.linger.load(Ordering::Relaxed) == 1u64 << 63;
                if let Tcp::Connected(c) | Tcp::Connecting(c) = &s
                    && linger0
                {
                    c.shutdown();
                }
                drop(s);
                if let Some(p) = port {
                    self.t().port_release(p, self.id());
                }
            }
            Proto::Udp(u) => {
                let (lo, smol, guard, guard6, ns_port, local, fam) = {
                    let mut g = u.lock();
                    (g.lo.take(), g.smol.take(), g.guard.take(), g.guard6.take(), g.ns_port.take(), g.local, g.fam)
                };
                if let (Some(a), Some(l)) = (local, &lo) {
                    self.t().lo_unhook(a.port, l, lo_slots(fam));
                }
                drop((lo, smol, guard, guard6, ns_port));
            }
        }
        notify();
    }
}
