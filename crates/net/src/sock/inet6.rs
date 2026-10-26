//! AF_INET6 for programs (NW-v6) on the IPv4 machinery of `inet` (this is its child module). BitsOS has no IPv6
//! on the wire (smoltcp runs without proto-ipv6, so no SLAAC or NDP), and an AF_INET6 socket reaches two kinds
//! of peers: ::1 over the loopback, and IPv4 hosts through v4-mapped addresses (::ffff:a.b.c.d), the dual stack
//! of linux-6.6/net/ipv6/af_inet6.c, tcp_ipv6.c, udp.c and datagram.c. Inside the kernel ::1 is the IPv4
//! stand-in `LO6` (in 127.0.0.0/8, so the loopback carries it unchanged) and :: is 0.0.0.0; this file turns a
//! sockaddr_in6 into that view and back, holds Linux's bind-conflict rule across both families and the SOL_IPV6
//! options (linux-6.6/net/ipv6/ipv6_sockglue.c).

use super::{InetSock, Proto, Tcp};
use crate::sock::abi::*;
use crate::stack::Stack;
use crate::{Ipv4, SockAddr};
use alloc::collections::BTreeSet;
use alloc::vec;
use ksync::SpinLock;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use uapi::errno::*;
use vfs::Errno;

pub const SOL_IPV6: u32 = 41;
/// `SIN6_LEN_RFC2133`: a sockaddr_in6 without sin6_scope_id is still taken.
const SIN6_LEN_RFC2133: usize = 24;
const SOCKADDR_IN6: usize = 28;
const SOCKADDR_IN: usize = 16;
/// Not in `uapi::errno` (only this option answers it).
const EPROTO: Errno = 71;

/// ::1 inside the kernel. A program never sees it: an AF_INET6 socket shows it as ::1. The loopback keeps the
/// two sides apart (an AF_INET program's own 127.0.6.1 goes to the IPv4 side); the remaining traces are listed
/// in plan/notes/nw-NW-v6.md.
pub const LO6: Ipv4 = Ipv4([127, 0, 6, 1]);
pub const LO6_BYTES: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
/// The largest UDP payload over IPv6 (65535 payload bytes less the UDP header; IPv4's is 65507).
pub const UDP6_MAX: usize = 65527;

const IPV6_ADDRFORM: u32 = 1;
const IPV6_2292PKTOPTIONS: u32 = 6;
const IPV6_UNICAST_HOPS: u32 = 16;
const IPV6_MULTICAST_IF: u32 = 17;
const IPV6_MULTICAST_HOPS: u32 = 18;
const IPV6_MULTICAST_LOOP: u32 = 19;
const IPV6_ADD_MEMBERSHIP: u32 = 20;
const IPV6_DROP_MEMBERSHIP: u32 = 21;
const IPV6_ROUTER_ALERT: u32 = 22;
const IPV6_MTU_DISCOVER: u32 = 23;
const IPV6_MTU: u32 = 24;
pub const IPV6_V6ONLY: u32 = 26;
const IPV6_JOIN_ANYCAST: u32 = 27;
const IPV6_LEAVE_ANYCAST: u32 = 28;
const IPV6_MULTICAST_ALL: u32 = 29;
const IPV6_RECVERR_RFC4884: u32 = 31;
const IPV6_FLOWLABEL_MGR: u32 = 32;
const IPV6_IPSEC_POLICY: u32 = 34;
const IPV6_XFRM_POLICY: u32 = 35;
const IPV6_PKTINFO: u32 = 50;
const IPV6_HOPOPTS: u32 = 54;
const IPV6_RTHDRDSTOPTS: u32 = 55;
const IPV6_RTHDR: u32 = 57;
const IPV6_DSTOPTS: u32 = 59;
const IPV6_PATHMTU: u32 = 61;
const IPV6_DONTFRAG: u32 = 62;
const IPV6_TCLASS: u32 = 67;
const IPV6_AUTOFLOWLABEL: u32 = 70;
const IPV6_ADDR_PREFERENCES: u32 = 72;
const IPV6_MINHOPCOUNT: u32 = 73;
const IPV6_TRANSPARENT: u32 = 75;
const IPV6_UNICAST_IF: u32 = 76;
const IPV6_RECVFRAGSIZE: u32 = 77;
/// `sizeof(struct ipv6_mc_socklist)`: what one joined group costs of the socket's option memory.
const MC_SOCKLIST: usize = 56;
/// MRT6_BASE..=MRT6_MAX: multicast routing, raw ICMPv6 sockets only.
const MRT6: core::ops::RangeInclusive<u32> = 200..=212;

/// Plain flags (`valbool`, EINVAL below an int): the IPV6_2292* and IPV6_RECV* receive options, IPV6_FLOWINFO,
/// IPV6_RECVERR, IPV6_V6ONLY's neighbours IPV6_ROUTER_ALERT_ISOLATE and IPV6_FLOWINFO_SEND, IPV6_FREEBIND.
const FLAGS: &[u32] = &[2, 3, 4, 5, 8, 11, 25, 30, 33, 49, 51, 53, 56, 58, 60, 66, 74, 78];
/// Flags taken whatever the length (a short value reads as 0): IPV6_DONTFRAG, IPV6_AUTOFLOWLABEL,
/// IPV6_RECVFRAGSIZE.
const LOOSE: &[u32] = &[IPV6_DONTFRAG, IPV6_AUTOFLOWLABEL, IPV6_RECVFRAGSIZE];

/// `IPV6_PREFER_SRC_*` bits kept in `np->srcprefs`.
const PREFER_TMP: i32 = 1;
const PREFER_PUBLIC: i32 = 2;
const PREFER_COA: i32 = 4;
const PREFER_PUBTMP_DEFAULT: i32 = 0x100;
const PREFER_HOME: i32 = 0x400;
const PREFER_CGA: i32 = 8;
const PREFER_NONCGA: i32 = 0x800;

// ---------------------------------------------------------------------------------------------------------
// addresses

/// What `bind6` decided: the kernel address, whether the bind makes the socket IPv6-only, the multicast group.
pub struct Bind6 {
    pub at: SockAddr,
    pub only6: bool,
    pub group: Option<[u8; 16]>,
}

/// What an IPv6 address is on BitsOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V6 {
    /// `::`
    Any,
    /// `::1`
    Loopback,
    /// `::ffff:a.b.c.d`
    Mapped(Ipv4),
    /// ff00::/8
    Multicast,
    /// fe80::/10: needs an interface (sin6_scope_id).
    LinkLocal,
    /// Any other address: none is configured here.
    Other,
}

pub fn classify(a: &[u8; 16]) -> V6 {
    if a[..15] == [0; 15] && a[15] <= 1 {
        return if a[15] == 0 { V6::Any } else { V6::Loopback };
    }
    if a[..10] == [0; 10] && a[10..12] == [0xff, 0xff] {
        return V6::Mapped(Ipv4([a[12], a[13], a[14], a[15]]));
    }
    match a[0] {
        0xff => V6::Multicast,
        0xfe if a[1] & 0xc0 == 0x80 => V6::LinkLocal,
        _ => V6::Other,
    }
}

/// A sockaddr_in6: address, port, sin6_scope_id (0 when the caller gave the RFC 2133 length).
pub struct Sin6 {
    pub addr: V6,
    pub raw: [u8; 16],
    pub port: u16,
    pub scope: u32,
}

/// EINVAL below `SIN6_LEN_RFC2133`, EAFNOSUPPORT for another family (`inet6_bind_sk`, `tcp_v6_connect`).
pub fn parse_in6(raw: &[u8]) -> Result<Sin6, Errno> {
    if raw.len() < SIN6_LEN_RFC2133 {
        return Err(EINVAL);
    }
    if u16::from_le_bytes([raw[0], raw[1]]) != AF_INET6 {
        return Err(EAFNOSUPPORT);
    }
    let mut a = [0u8; 16];
    a.copy_from_slice(&raw[8..24]);
    let scope = if raw.len() >= SOCKADDR_IN6 { u32::from_le_bytes([raw[24], raw[25], raw[26], raw[27]]) } else { 0 };
    Ok(Sin6 { addr: classify(&a), raw: a, port: u16::from_be_bytes([raw[2], raw[3]]), scope })
}

/// The kernel's view of `a` as an AF_INET6 socket shows it: `LO6` is ::1, 0.0.0.0 is ::, anything else is
/// v4-mapped. (A socket bound to ::ffff:0.0.0.0 therefore shows ::, and one bound to a multicast group shows ::.)
pub fn encode_in6(a: SockAddr) -> Vec<u8> {
    let mut v = vec![0u8; SOCKADDR_IN6];
    v[..2].copy_from_slice(&AF_INET6.to_le_bytes());
    v[2..4].copy_from_slice(&a.port.to_be_bytes());
    if a.ip == LO6 {
        v[23] = 1;
    } else if !a.ip.is_unspecified() {
        v[18] = 0xff;
        v[19] = 0xff;
        v[20..24].copy_from_slice(&a.ip.0);
    }
    v
}

/// `encode_in6` for a datagram's address: an IPv4 sender (`v6` false) is always v4-mapped, also an AF_INET
/// program that used 127.0.6.1 itself.
pub fn encode_in6_as(a: SockAddr, v6: bool) -> Vec<u8> {
    let mut v = encode_in6(a);
    if !v6 && a.ip == LO6 {
        v[18] = 0xff;
        v[19] = 0xff;
        v[20..24].copy_from_slice(&a.ip.0);
    }
    v
}

/// Whether a multicast group's scope (node- or link-local) needs an interface, as fe80::/10 does
/// (`__ipv6_addr_needs_scope_id`, linux-6.6/include/net/ipv6.h:571).
fn mc_needs_scope(a: &[u8; 16]) -> bool {
    matches!(a[1] & 0x0f, 1 | 2)
}

fn ifindex_exists(i: u32) -> bool {
    crate::ifview::links().iter().any(|l| l.index == i)
}

/// How a bound address takes part in the port rules (the socket's family plus IPV6_V6ONLY).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fam {
    /// An AF_INET socket, or an AF_INET6 one on a v4-mapped address.
    V4,
    /// AF_INET6 on ::, IPV6_V6ONLY off: IPv4 and IPv6 peers.
    Dual,
    /// AF_INET6 on ::1, or on :: with IPV6_V6ONLY: IPv6 peers only.
    Only6,
}

/// UDP ports held on the IPv6 side (IPv6-only and dual-stack sockets); the IPv4 side is the stack's table
/// (`stack::udp::reserve_port`), which dual-stack sockets hold as well. So an IPv6-only socket and an AF_INET one
/// share a port, as in Linux's `udp_lib_lport_inuse` with `ipv6_rcv_saddr_equal`.
static UDP6_PORTS: SpinLock<BTreeSet<u16>> = SpinLock::named("net.inet.udp6_ports", BTreeSet::new());

/// A port of `UDP6_PORTS`, given back on drop.
pub struct Port6Guard(pub u16);

impl Drop for Port6Guard {
    fn drop(&mut self) {
        UDP6_PORTS.lock().remove(&self.0);
    }
}

/// Takes UDP `port` on the IPv6 side; 0 picks a free ephemeral one (the IPv4 table is not asked: an IPv6-only
/// socket does not meet IPv4 ones).
pub fn udp6_take(st: &Stack, port: u16) -> Option<Port6Guard> {
    let mut used = UDP6_PORTS.lock();
    let port = if port == 0 { st.ephemeral_port(|p| used.contains(&p))? } else { port };
    // Not `then_some`: a guard built for a taken port would give that port back on its drop.
    if used.insert(port) { Some(Port6Guard(port)) } else { None }
}

/// `inet_rcv_saddr_equal` / `ipv6_rcv_saddr_equal` with wildcards matching (the bind-conflict question): two
/// IPv4 addresses overlap when equal or one is 0.0.0.0; a dual-stack :: overlaps every IPv4 address; IPv6-only
/// addresses never meet IPv4 ones; on the IPv6 side only :: and ::1 exist, and they overlap each other.
pub fn overlap(a: (Ipv4, Fam), b: (Ipv4, Fam)) -> bool {
    match (a.1, b.1) {
        (Fam::V4, Fam::V4) => a.0 == b.0 || a.0.is_unspecified() || b.0.is_unspecified(),
        (Fam::V4, Fam::Dual) | (Fam::Dual, Fam::V4) => true,
        (Fam::V4, Fam::Only6) | (Fam::Only6, Fam::V4) => false,
        _ => true,
    }
}

/// The policy's name for a destination the kernel holds as `ip` (B3: ::1 is named as IPv6).
pub fn policy_ip(ip: Ipv4) -> policy::IpAddr {
    if ip == LO6 { policy::IpAddr::V6(LO6_BYTES) } else { policy::IpAddr::V4(ip.0) }
}

// ---------------------------------------------------------------------------------------------------------
// the socket side

impl InetSock {
    pub fn is_v6(&self) -> bool {
        self.v6.load(Ordering::Acquire)
    }

    pub(super) fn v6only(&self) -> bool {
        self.is_v6() && self.ip_opts.lock().get(&(SOL_IPV6, IPV6_V6ONLY)).is_some_and(|v| *v != 0)
    }

    /// The port-rule family of this socket on the kernel address `ip`.
    pub(super) fn fam_of(&self, ip: Ipv4) -> Fam {
        if !self.is_v6() {
            Fam::V4
        } else if ip == LO6 {
            Fam::Only6
        } else if ip.is_unspecified() {
            if self.v6only() { Fam::Only6 } else { Fam::Dual }
        } else {
            Fam::V4
        }
    }

    /// A kernel address as this socket's family shows it (getsockname, getpeername, accept, recvfrom).
    pub(super) fn name_of(&self, a: SockAddr) -> Vec<u8> {
        if self.is_v6() { encode_in6(a) } else { super::encode_in(a) }
    }

    /// The address bound before (TCP or UDP), if any.
    fn bound_ip(&self) -> Option<Ipv4> {
        match &self.proto {
            Proto::Tcp(t) => t.lock().local.map(|a| a.ip),
            Proto::Udp(u) => u.lock().local.map(|a| a.ip),
        }
    }

    fn is_bound(&self) -> bool {
        match &self.proto {
            Proto::Tcp(t) => {
                let g = t.lock();
                g.port.is_some() || !matches!(g.s, Tcp::Idle)
            }
            Proto::Udp(u) => u.lock().local.is_some(),
        }
    }

    /// `__inet6_bind` up to the port: what to bind. A specific IPv6 address (::1, a group) makes the socket
    /// IPv6-only, as Linux does; the caller sets that (`force_v6only`) only for the port's taking, and gives the
    /// old value back when the bind fails (`saved_ipv6only`, linux-6.6/net/ipv6/af_inet6.c:395-403).
    pub(super) fn bind6(&self, raw: &[u8], uid: u32) -> Result<Bind6, Errno> {
        let s = parse_in6(raw)?;
        let tcp = matches!(self.proto, Proto::Tcp(_));
        if s.addr == V6::Multicast && tcp {
            return Err(EINVAL);
        }
        if s.port != 0 && s.port < UNPRIVILEGED_PORT_START && uid != 0 {
            return Err(EACCES);
        }
        if self.is_bound() {
            return Err(EINVAL);
        }
        if matches!(s.addr, V6::Mapped(_)) && self.v6only() {
            return Err(EINVAL);
        }
        let scoped = s.addr == V6::LinkLocal || (s.addr == V6::Multicast && mc_needs_scope(&s.raw));
        if scoped && s.scope != 0 {
            // Linux binds the socket to the scope's interface before it checks anything (and keeps it so).
            self.opts.bound_dev.store(s.scope, Ordering::Relaxed);
        }
        let dev = self.opts.bound_dev.load(Ordering::Relaxed);
        if scoped && dev == 0 {
            return Err(EINVAL);
        }
        if s.addr != V6::Any && dev != 0 && !ifindex_exists(dev) {
            return Err(ENODEV);
        }
        let ip = match s.addr {
            V6::Mapped(ip) => ip,
            V6::Any => Ipv4::ANY,
            V6::Loopback => LO6,
            // No IPv6 multicast reaches this host; the socket binds as an IPv6-only :: and shows its group.
            V6::Multicast => Ipv4::ANY,
            V6::LinkLocal | V6::Other => return Err(EADDRNOTAVAIL),
        };
        Ok(Bind6 {
            at: SockAddr { ip, port: s.port },
            only6: matches!(s.addr, V6::Loopback | V6::Multicast),
            group: (s.addr == V6::Multicast).then_some(s.raw),
        })
    }

    /// Turns IPV6_V6ONLY on for a bind; the old value, for `restore_v6only`.
    pub(super) fn force_v6only(&self) -> Option<i32> {
        self.ip_opts.lock().insert((SOL_IPV6, IPV6_V6ONLY), 1)
    }

    pub(super) fn restore_v6only(&self, old: Option<i32>) {
        let mut o = self.ip_opts.lock();
        match old {
            Some(v) => o.insert((SOL_IPV6, IPV6_V6ONLY), v),
            None => o.remove(&(SOL_IPV6, IPV6_V6ONLY)),
        };
    }

    /// getsockname of a socket bound to multicast group `g` (its interface as sin6_scope_id when the group's
    /// scope needs one, `ipv6_iface_scope_id`).
    pub(super) fn group_name(&self, g: [u8; 16], port: u16) -> Vec<u8> {
        let mut v = encode_in6(SockAddr { ip: Ipv4::ANY, port });
        v[8..24].copy_from_slice(&g);
        if mc_needs_scope(&g) {
            v[24..28].copy_from_slice(&self.opts.bound_dev.load(Ordering::Relaxed).to_le_bytes());
        }
        v
    }

    /// The loopback destination for `::`: ::1, or 127.0.0.1 when the socket is bound to a v4-mapped address.
    fn self_dest(&self) -> Ipv4 {
        match self.bound_ip() {
            Some(ip) if !ip.is_unspecified() && ip != LO6 => Ipv4::LOCALHOST,
            _ => LO6,
        }
    }

    /// `tcp_v6_connect`'s and `__ip6_datagram_connect`'s destination: the kernel address to connect to.
    pub(super) fn connect6(&self, raw: &[u8]) -> Result<SockAddr, Errno> {
        let tcp = matches!(self.proto, Proto::Tcp(_));
        // UDP takes a sockaddr_in too (an IPv4 connect), unless IPv6-only.
        if !tcp && super::family(raw) == Some(AF_INET) {
            if self.v6only() {
                return Err(EAFNOSUPPORT);
            }
            return InetSock::sin(raw, false);
        }
        let s = parse_in6(raw)?;
        let ip = match s.addr {
            V6::Any => self.self_dest(),
            V6::Loopback => LO6,
            V6::Mapped(_) if self.v6only() => return Err(ENETUNREACH),
            V6::Mapped(ip) => ip,
            V6::LinkLocal if s.scope == 0 => return Err(EINVAL),
            V6::Multicast | V6::LinkLocal | V6::Other => return Err(ENETUNREACH),
        };
        if ip != LO6 && self.bound_ip() == Some(LO6) {
            // Bound to ::1 (IPv6-only): no route to an IPv4 peer.
            return Err(ENETUNREACH);
        }
        Ok(SockAddr { ip, port: s.port })
    }

    /// `udpv6_sendmsg`'s destination check: None for AF_UNSPEC (the connected peer is used).
    pub(super) fn sendto6(&self, raw: &[u8]) -> Result<Option<SockAddr>, Errno> {
        if raw.len() < 2 {
            return Err(EINVAL);
        }
        let v4 = |raw: &[u8]| -> Result<Option<SockAddr>, Errno> {
            if self.v6only() {
                return Err(ENETUNREACH);
            }
            if raw.len() < SOCKADDR_IN {
                return Err(EINVAL);
            }
            let a = super::parse_in(raw).ok_or(EINVAL)?;
            if a.port == 0 {
                return Err(EINVAL);
            }
            Ok(Some(a))
        };
        match super::family(raw) {
            Some(AF_INET) => v4(raw),
            Some(AF_UNSPEC) => Ok(None),
            Some(AF_INET6) => {
                let s = parse_in6(raw)?;
                let ip = match s.addr {
                    V6::Mapped(ip) => {
                        if self.v6only() {
                            return Err(ENETUNREACH);
                        }
                        ip
                    }
                    V6::Any => self.self_dest(),
                    V6::Loopback => LO6,
                    _ => {
                        if s.port == 0 {
                            return Err(EINVAL);
                        }
                        return Err(ENETUNREACH);
                    }
                };
                if s.port == 0 {
                    return Err(EINVAL);
                }
                if ip != LO6 && self.bound_ip() == Some(LO6) {
                    return Err(ENETUNREACH);
                }
                Ok(Some(SockAddr { ip, port: s.port }))
            }
            _ => Err(EINVAL),
        }
    }

    fn connected(&self) -> Option<SockAddr> {
        match &self.proto {
            Proto::Tcp(t) => {
                self.sync(t);
                let g = t.lock();
                if matches!(g.s, Tcp::Connected(_)) { g.peer } else { None }
            }
            Proto::Udp(u) => u.lock().peer,
        }
    }

    /// `do_ipv6_setsockopt` for the options a TCP or UDP socket has. `val`: the first bytes the program gave
    /// (as many as `opt6_len` says the option needs).
    pub(super) fn setsockopt6(&self, name: u32, optlen: usize, val: Result<Vec<u8>, Errno>) -> Result<(), Errno> {
        let tcp = matches!(self.proto, Proto::Tcp(_));
        if MRT6.contains(&name) {
            return Err(EOPNOTSUPP);
        }
        // Linux reads an int when optlen holds one and takes 0 otherwise.
        let bytes = if optlen > 0 { val? } else { Vec::new() };
        let v = if optlen >= 4 { bytes.get(..4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or(EFAULT)? } else { 0 };
        let int_ok = || if optlen < 4 { Err(EINVAL) } else { Ok(()) };
        let store = |n: u32, x: i32| {
            self.ip_opts.lock().insert((SOL_IPV6, n), x);
            Ok(())
        };
        match name {
            n if FLAGS.contains(&n) => {
                int_ok()?;
                store(n, (v != 0) as i32)
            }
            n if LOOSE.contains(&n) => store(n, (v != 0) as i32),
            IPV6_V6ONLY => {
                // Against bind (which reads the flag once for the port rules): either before it or EINVAL.
                let _b = self.bind_mx.lock();
                if optlen < 4 || self.is_bound() {
                    return Err(EINVAL);
                }
                store(name, (v != 0) as i32)
            }
            IPV6_ADDRFORM => {
                int_ok()?;
                if v as u16 != AF_INET || v >> 16 != 0 {
                    return Err(EINVAL);
                }
                let _b = self.bind_mx.lock();
                let peer = self.connected().ok_or(ENOTCONN)?;
                if self.v6only() || peer.ip == LO6 {
                    return Err(EADDRNOTAVAIL);
                }
                self.groups6.lock().clear();
                self.to_v4();
                klog::event!(Debug, "net", "inet6.addrform", pid = self.owner.pid);
                Ok(())
            }
            IPV6_UNICAST_HOPS => {
                int_ok()?;
                if !(-1..=255).contains(&v) {
                    return Err(EINVAL);
                }
                store(name, v)
            }
            IPV6_MULTICAST_HOPS | IPV6_MULTICAST_IF if tcp => Err(ENOPROTOOPT),
            IPV6_MULTICAST_HOPS => {
                int_ok()?;
                if !(-1..=255).contains(&v) {
                    return Err(EINVAL);
                }
                store(name, if v == -1 { 1 } else { v })
            }
            IPV6_MULTICAST_IF => {
                int_ok()?;
                if v != 0 && !ifindex_exists(v as u32) {
                    return Err(ENODEV);
                }
                store(name, v)
            }
            IPV6_MULTICAST_LOOP => {
                int_ok()?;
                if v != 0 && v != 1 {
                    return Err(EINVAL);
                }
                store(name, v)
            }
            IPV6_MULTICAST_ALL => {
                int_ok()?;
                store(name, (v != 0) as i32)
            }
            IPV6_TCLASS => {
                int_ok()?;
                if !(-1..=255).contains(&v) {
                    return Err(EINVAL);
                }
                let mut t = if v == -1 { 0 } else { v };
                // A stream keeps its own ECN bits (none are set here).
                if tcp {
                    t &= !3;
                }
                store(name, t)
            }
            IPV6_MTU_DISCOVER => {
                int_ok()?;
                if !(0..=5).contains(&v) {
                    return Err(EINVAL);
                }
                store(name, v)
            }
            IPV6_MTU => {
                int_ok()?;
                if v != 0 && v < 1280 {
                    return Err(EINVAL);
                }
                store(name, v)
            }
            IPV6_MINHOPCOUNT => {
                int_ok()?;
                if !(0..=255).contains(&v) {
                    return Err(EINVAL);
                }
                store(name, v)
            }
            IPV6_RECVERR_RFC4884 => {
                int_ok()?;
                if v != 0 && v != 1 {
                    return Err(EINVAL);
                }
                store(name, v)
            }
            IPV6_TRANSPARENT => {
                if v != 0 && self.owner.uid != 0 {
                    return Err(EPERM);
                }
                int_ok()?;
                store(name, (v != 0) as i32)
            }
            IPV6_ROUTER_ALERT => {
                int_ok()?;
                // `ip6_ra_control`: raw sockets only.
                Err(ENOPROTOOPT)
            }
            IPV6_UNICAST_IF => {
                if optlen != 4 {
                    return Err(EINVAL);
                }
                let idx = u32::from_be(v as u32);
                if idx != 0 && !ifindex_exists(idx) {
                    return Err(EADDRNOTAVAIL);
                }
                store(name, idx as i32)
            }
            IPV6_ADDR_PREFERENCES => {
                int_ok()?;
                let old = self.ip_opts.lock().get(&(SOL_IPV6, name)).copied().unwrap_or(0);
                store(name, addr_prefs(old, v)?)
            }
            IPV6_PKTINFO => {
                // `struct in6_pktinfo`: a sticky source; kept nowhere (every source here is the obvious one).
                if optlen < 20 {
                    return Err(EINVAL);
                }
                bytes.get(..20).ok_or(EFAULT)?;
                Ok(())
            }
            IPV6_ADD_MEMBERSHIP | IPV6_DROP_MEMBERSHIP => {
                if optlen < 20 {
                    return Err(EINVAL);
                }
                if tcp {
                    return Err(EPROTO);
                }
                let b = bytes.get(..20).ok_or(EFAULT)?;
                let mut group = [0u8; 16];
                group.copy_from_slice(&b[..16]);
                let ifindex = i32::from_le_bytes([b[16], b[17], b[18], b[19]]);
                self.membership(name == IPV6_ADD_MEMBERSHIP, group, ifindex)
            }
            IPV6_JOIN_ANYCAST | IPV6_LEAVE_ANYCAST => {
                if optlen < 20 {
                    return Err(EINVAL);
                }
                bytes.get(..20).ok_or(EFAULT)?;
                // `ipv6_sock_ac_join` needs an address of this host's; there is none (leave: not joined).
                Err(if name == IPV6_JOIN_ANYCAST { EINVAL } else { ENOENT })
            }
            IPV6_IPSEC_POLICY | IPV6_XFRM_POLICY => Err(EPERM),
            IPV6_FLOWLABEL_MGR => Err(EINVAL),
            IPV6_HOPOPTS | IPV6_RTHDRDSTOPTS | IPV6_RTHDR | IPV6_DSTOPTS if optlen == 0 => Ok(()),
            _ => Err(ENOPROTOOPT),
        }
    }

    /// IPV6_ADDRFORM's switch to AF_INET (under `bind_mx`): the port rules and the loopback then see an IPv4
    /// socket (`do_ipv6_setsockopt` moves it to the IPv4 protocol tables).
    fn to_v4(&self) {
        self.v6.store(false, Ordering::Release);
        match &self.proto {
            Proto::Tcp(t) => {
                if let Some(p) = t.lock().port {
                    super::port_update(p, self.id(), |u| u.fam = Fam::V4);
                }
            }
            Proto::Udp(u) => {
                let mut g = u.lock();
                if g.fam == Fam::Dual
                    && let (Some(l), Some(r)) = (g.local, &g.lo)
                {
                    super::lo_unhook(l.port, r, &[true]);
                }
                g.fam = Fam::V4;
                let old = g.guard6.take();
                drop(g);
                drop(old);
            }
        }
    }

    /// `ipv6_sock_mc_join` / `ipv6_sock_mc_drop`: kept per socket; no IPv6 multicast reaches this host.
    fn membership(&self, join: bool, group: [u8; 16], ifindex: i32) -> Result<(), Errno> {
        if classify(&group) != V6::Multicast {
            return Err(EINVAL);
        }
        // The interface is looked up before the list's lock (it asks the stack); the checks, in Linux's order,
        // and the push then happen under one lock.
        let dev_ok = !join || ifindex == 0 || ifindex_exists(ifindex as u32);
        let mut g = self.groups6.lock();
        let at = g.iter().position(|x| x.0 == group && (ifindex == 0 || x.1 == ifindex));
        if !join {
            let i = at.ok_or(EADDRNOTAVAIL)?;
            g.swap_remove(i);
            return Ok(());
        }
        if at.is_some() {
            return Err(EADDRINUSE);
        }
        // `sock_kmalloc`: a socket's option memory stops at net.core.optmem_max.
        if (g.len() + 1) * MC_SOCKLIST > OPTMEM_MAX as usize {
            return Err(ENOMEM);
        }
        if !dev_ok {
            return Err(ENODEV);
        }
        g.try_reserve(1).map_err(|_| ENOMEM)?;
        g.push((group, ifindex));
        Ok(())
    }

    /// `do_ipv6_getsockopt`.
    pub(super) fn getsockopt6(&self, name: u32) -> Result<Vec<u8>, Errno> {
        let tcp = matches!(self.proto, Proto::Tcp(_));
        if MRT6.contains(&name) {
            return Err(EOPNOTSUPP);
        }
        let stored = |d: i32| self.ip_opts.lock().get(&(SOL_IPV6, name)).copied().unwrap_or(d);
        let v = match name {
            n if FLAGS.contains(&n) || n == IPV6_DONTFRAG || n == IPV6_RECVFRAGSIZE => stored(0),
            IPV6_V6ONLY | IPV6_TCLASS | IPV6_MINHOPCOUNT | IPV6_RECVERR_RFC4884 | IPV6_MULTICAST_IF | IPV6_TRANSPARENT => stored(0),
            IPV6_AUTOFLOWLABEL | IPV6_MULTICAST_ALL | IPV6_MULTICAST_HOPS | IPV6_MULTICAST_LOOP | IPV6_MTU_DISCOVER => stored(1),
            // -1 (the route's): the default hop limit of net.ipv6.conf.all.hop_limit.
            IPV6_UNICAST_HOPS => match stored(-1) {
                -1 => 64,
                h => h,
            },
            IPV6_UNICAST_IF => (stored(0) as u32).to_be() as i32,
            IPV6_ADDR_PREFERENCES => {
                let p = stored(0);
                let src = if p & PREFER_TMP != 0 {
                    PREFER_TMP
                } else if p & PREFER_PUBLIC != 0 {
                    PREFER_PUBLIC
                } else {
                    PREFER_PUBTMP_DEFAULT
                };
                src | if p & PREFER_COA != 0 { PREFER_COA } else { PREFER_HOME }
            }
            IPV6_ADDRFORM => {
                self.connected().ok_or(ENOTCONN)?;
                AF_INET6 as i32
            }
            IPV6_MTU | IPV6_PATHMTU => {
                let p = self.connected().ok_or(ENOTCONN)?;
                let mtu = if p.ip.is_loopback() { 65536 } else { 1500 };
                if name == IPV6_PATHMTU {
                    // `struct ip6_mtuinfo`: the peer (sockaddr_in6) and the MTU.
                    let mut m = encode_in6(p);
                    m.extend_from_slice(&(mtu as u32).to_le_bytes());
                    return Ok(m);
                }
                mtu
            }
            // Sticky headers: none set.
            IPV6_HOPOPTS | IPV6_RTHDRDSTOPTS | IPV6_RTHDR | IPV6_DSTOPTS => return Ok(Vec::new()),
            IPV6_2292PKTOPTIONS if tcp => return Ok(Vec::new()),
            _ => return Err(ENOPROTOOPT),
        };
        Ok(v.to_le_bytes().to_vec())
    }
}

/// `__ip6_sock_set_addr_preferences`: the new `srcprefs` from the old ones and the program's bits.
fn addr_prefs(old: i32, val: i32) -> Result<i32, Errno> {
    let (mut pref, mut keep) = (0, !0);
    match val & (PREFER_PUBLIC | PREFER_TMP | PREFER_PUBTMP_DEFAULT) {
        0 => {}
        PREFER_PUBLIC => {
            pref |= PREFER_PUBLIC;
            keep &= !(PREFER_PUBLIC | PREFER_TMP);
        }
        PREFER_TMP => {
            pref |= PREFER_TMP;
            keep &= !(PREFER_PUBLIC | PREFER_TMP);
        }
        PREFER_PUBTMP_DEFAULT => keep &= !(PREFER_PUBLIC | PREFER_TMP),
        _ => return Err(EINVAL),
    }
    match val & (PREFER_HOME | PREFER_COA) {
        0 => {}
        PREFER_HOME => keep &= !PREFER_COA,
        PREFER_COA => pref |= PREFER_COA,
        _ => return Err(EINVAL),
    }
    if val & (PREFER_CGA | PREFER_NONCGA) == PREFER_CGA | PREFER_NONCGA {
        return Err(EINVAL);
    }
    Ok((old & keep) | pref)
}

/// How many bytes of a SOL_IPV6 value `setsockopt6` looks at.
pub fn opt6_len(name: u32) -> usize {
    match name {
        IPV6_ADD_MEMBERSHIP | IPV6_DROP_MEMBERSHIP | IPV6_JOIN_ANYCAST | IPV6_LEAVE_ANYCAST | IPV6_PKTINFO => 20,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sin6(addr: [u8; 16], port: u16) -> Vec<u8> {
        let mut v = vec![0u8; 28];
        v[..2].copy_from_slice(&AF_INET6.to_le_bytes());
        v[2..4].copy_from_slice(&port.to_be_bytes());
        v[8..24].copy_from_slice(&addr);
        v
    }

    #[test]
    fn addresses_like_linux() {
        assert_eq!(classify(&[0; 16]), V6::Any);
        assert_eq!(classify(&LO6_BYTES), V6::Loopback);
        let mut m = [0u8; 16];
        m[10] = 0xff;
        m[11] = 0xff;
        m[12..].copy_from_slice(&[127, 0, 0, 1]);
        assert_eq!(classify(&m), V6::Mapped(Ipv4::LOCALHOST));
        let mut ll = [0u8; 16];
        ll[0] = 0xfe;
        ll[1] = 0x80;
        assert_eq!(classify(&ll), V6::LinkLocal);
        ll[0] = 0xff;
        assert_eq!(classify(&ll), V6::Multicast);
        let mut compat = [0u8; 16];
        compat[15] = 2;
        assert_eq!(classify(&compat), V6::Other, "::2 is no special address");
        let raw = sin6(LO6_BYTES, 8080);
        let s = parse_in6(&raw).unwrap();
        assert_eq!((s.addr, s.port, s.scope), (V6::Loopback, 8080, 0));
        assert!(parse_in6(&raw[..24]).is_ok(), "RFC 2133 length");
        assert_eq!(parse_in6(&raw[..23]).err(), Some(EINVAL));
        let mut wrong = raw.clone();
        wrong[0] = AF_INET as u8;
        assert_eq!(parse_in6(&wrong).err(), Some(EAFNOSUPPORT));
        assert_eq!(encode_in6(SockAddr { ip: LO6, port: 8080 }), raw);
        assert_eq!(&encode_in6(SockAddr { ip: Ipv4::LOCALHOST, port: 1 })[8..24], &m);
        assert_eq!(&encode_in6(SockAddr { ip: Ipv4::ANY, port: 1 })[8..24], &[0u8; 16]);
    }

    #[test]
    fn bind_conflicts_across_families() {
        let any = Ipv4::ANY;
        assert!(overlap((any, Fam::V4), (Ipv4::LOCALHOST, Fam::V4)));
        assert!(overlap((Ipv4::LOCALHOST, Fam::V4), (any, Fam::Dual)), "dual :: covers IPv4");
        assert!(!overlap((any, Fam::V4), (any, Fam::Only6)), "v6-only :: beside 0.0.0.0");
        assert!(!overlap((Ipv4::LOCALHOST, Fam::V4), (LO6, Fam::Only6)), "127.0.0.1 beside ::1");
        assert!(overlap((any, Fam::Only6), (LO6, Fam::Only6)));
        assert!(overlap((any, Fam::Dual), (any, Fam::Only6)));
    }

    #[test]
    fn address_preferences() {
        assert_eq!(addr_prefs(0, PREFER_TMP), Ok(PREFER_TMP));
        assert_eq!(addr_prefs(PREFER_TMP, PREFER_PUBLIC), Ok(PREFER_PUBLIC));
        assert_eq!(addr_prefs(PREFER_TMP | PREFER_COA, PREFER_HOME), Ok(PREFER_TMP));
        assert_eq!(addr_prefs(0, PREFER_TMP | PREFER_PUBLIC), Err(EINVAL));
        assert_eq!(addr_prefs(0, PREFER_CGA | PREFER_NONCGA), Err(EINVAL));
    }
}
