//! Host tests of NW-6: AF_NETLINK NETLINK_ROUTE and the interface ioctls through the syscalls, on a stack with
//! one fake card ("eth9", 10.1.2.3/24, link switchable). Each assertion names the Linux behaviour it pins down;
//! the same cases run against CachyOS in `abi-netlink` (user/abi-tests/c/netlink.c).

use super::abi::*;
use super::devioctl::*;
use super::netlink::*;
use super::nlmsg::*;
use super::rtnl::{Dump, RTM_DELADDR, RTM_GETADDR, RTM_GETLINK, RTM_NEWADDR, RTM_NEWLINK};
use super::sys;
use super::tests::{Prog, ns};
use crate::Ipv4;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::net::{LinkState, MacAddr, NetDevice};
use uapi::errno::*;

/// A card whose link a test switches.
struct Card {
    up: AtomicBool,
}

impl NetDevice for Card {
    fn name(&self) -> &str {
        "eth9"
    }
    fn mac(&self) -> MacAddr {
        MacAddr([2, 0, 0, 0, 0, 9])
    }
    fn link(&self) -> LinkState {
        LinkState { up: self.up.load(Ordering::Relaxed), speed_mbps: 1000, full_duplex: true }
    }
    fn transmit(&self, _f: &[u8]) -> Result<(), devmodel::DevError> {
        Ok(())
    }
    fn receive(&self, _b: &mut [u8]) -> Option<usize> {
        None
    }
    fn poll_tx(&self, _f: &[u8]) -> bool {
        true
    }
}

static CARD: Card = Card { up: AtomicBool::new(true) };

/// The tests share eth9 (its address and link are changed by some of them): one at a time.
fn shared() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits until no notification round is running or due: a round another test's stack started may be the one
/// that carries this test's change (`changed` hands it to the running caller).
fn settle() {
    while !rounds_idle() {
        std::thread::yield_now();
    }
}

/// The stack the interface view shows (once per test binary).
fn view() -> &'static crate::stack::Stack {
    static ONCE: std::sync::OnceLock<&'static crate::stack::Stack> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let st = crate::stack::Stack::new_static(super::now, 11, false);
        struct Ref;
        impl NetDevice for Ref {
            fn name(&self) -> &str {
                CARD.name()
            }
            fn mac(&self) -> MacAddr {
                CARD.mac()
            }
            fn link(&self) -> LinkState {
                CARD.link()
            }
            fn transmit(&self, f: &[u8]) -> Result<(), devmodel::DevError> {
                CARD.transmit(f)
            }
            fn receive(&self, b: &mut [u8]) -> Option<usize> {
                CARD.receive(b)
            }
            fn poll_tx(&self, f: &[u8]) -> bool {
                CARD.poll_tx(f)
            }
        }
        st.add_device(Arc::new(Ref), false);
        st.set_static("eth9", Ipv4([10, 1, 2, 3]), 24, None).unwrap();
        crate::ifview::set_stack(st);
        st
    })
}

fn nl(p: &Prog) -> u64 {
    view();
    let fd = p.call(sys::socket, [AF_NETLINK as u64, SOCK_RAW as u64, 0, 0, 0, 0]);
    assert!(fd >= 0, "socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)");
    fd as u64
}

fn nladdr(pid: u32, groups: u32) -> Vec<u8> {
    let mut v = AF_NETLINK.to_le_bytes().to_vec();
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&pid.to_le_bytes());
    v.extend_from_slice(&groups.to_le_bytes());
    v
}

fn req(ty: u16, flags: u16, seq: u32, body: &[u8]) -> Vec<u8> {
    let mut m = Msg::new(ty, flags, seq, 0, body);
    m.set_flags(flags);
    m.done()
}

/// The messages of one datagram: (header, payload).
fn msgs(d: &[u8]) -> Vec<(Hdr, Vec<u8>)> {
    let mut v = Vec::new();
    let mut rest = d;
    while let Some(h) = Hdr::parse(rest) {
        let len = h.len as usize;
        if len < NLMSG_HDRLEN || len > rest.len() {
            break;
        }
        v.push((h, rest[NLMSG_HDRLEN..len].to_vec()));
        rest = &rest[align(len).min(rest.len())..];
    }
    v
}

fn recv_now(p: &Prog, fd: u64, n: usize) -> (i64, Vec<u8>) {
    p.recv(fd, n, MSG_DONTWAIT)
}

/// Reads one dump to its DONE: (messages of `want` type, datagrams, DONE's error, DONE alone in its datagram).
fn read_dump(p: &Prog, fd: u64, want: u16) -> (Vec<(Hdr, Vec<u8>)>, usize, i32, bool) {
    let mut got = Vec::new();
    for dgrams in 1..100 {
        let (r, d) = recv_now(p, fd, 65536);
        assert!(r > 0, "the dump ends with DONE without waiting ({r})");
        let m = msgs(&d);
        for (i, (h, b)) in m.iter().enumerate() {
            if h.ty == NLMSG_DONE {
                return (got, dgrams, i32::from_le_bytes(b[..4].try_into().unwrap()), i == 0 && m.len() == 1);
            }
            if h.ty == want {
                got.push((*h, b.clone()));
            }
        }
    }
    panic!("no DONE");
}

fn err_of(m: &(Hdr, Vec<u8>)) -> i32 {
    assert_eq!(m.0.ty, NLMSG_ERROR);
    i32::from_le_bytes(m.1[..4].try_into().unwrap())
}

#[test]
fn socket_bind_connect_like_linux() {
    let _g = shared();
    let ns = ns();
    let p = Prog::new(&ns, 4001, 1000);
    view();
    let sock = |ty: u32, proto: i64| p.call(sys::socket, [AF_NETLINK as u64, ty as u64, proto as u64, 0, 0, 0]);
    assert_eq!(sock(SOCK_STREAM, 0), -ESOCKTNOSUPPORT, "netlink_create: RAW or DGRAM only");
    assert_eq!(sock(SOCK_RAW, 32), -EPROTONOSUPPORT, "protocol >= MAX_LINKS");
    assert_eq!(sock(SOCK_RAW, -1), -EPROTONOSUPPORT);
    assert_eq!(sock(SOCK_RAW, 15), -EPROTONOSUPPORT, "only NETLINK_ROUTE exists in V2");
    let dg = sock(SOCK_DGRAM, 0);
    assert!(dg >= 0);
    let s = nl(&p);
    assert_eq!((p.getopt_int(s, SOL_SOCKET, SO_TYPE), p.getopt_int(s, SOL_SOCKET, SO_PROTOCOL), p.getopt_int(s, SOL_SOCKET, SO_DOMAIN)), (3, 0, 16));
    assert_eq!(p.getname(s, false).unwrap(), nladdr(0, 0), "unbound: port id 0");
    assert_eq!(p.getname(s, true).unwrap(), nladdr(0, 0));
    let bind = |fd: u64, a: &[u8]| {
        let at = p.put(a);
        p.call(sys::bind, [fd, at, a.len() as u64, 0, 0, 0])
    };
    assert_eq!(bind(s, &nladdr(0, 0)[..11]), -EINVAL, "short address");
    assert_eq!(bind(s, &nladdr(0, 0)), 0, "autobind");
    assert_eq!(p.getname(s, false).unwrap(), nladdr(4001, 0), "the process id first");
    assert_eq!(bind(s, &nladdr(0, 0)), -EINVAL, "bound: another id");
    assert_eq!(bind(s, &nladdr(4001, 1)), 0, "same id with a group");
    assert_eq!(p.getname(s, false).unwrap(), nladdr(4001, 1));
    let t = nl(&p);
    assert_eq!(bind(t, &nladdr(4001, 0)), -EADDRINUSE);
    let connect = |fd: u64, a: &[u8]| {
        let at = p.put(a);
        p.call(sys::connect, [fd, at, a.len() as u64, 0, 0, 0])
    };
    assert_eq!(connect(t, &nladdr(0, 0)), 0, "connect to the kernel autobinds");
    let name = p.getname(t, false).unwrap();
    assert!((u32::from_le_bytes(name[4..8].try_into().unwrap()) as i32) < -4096, "the process id is taken: a negative id");
    assert_eq!(connect(t, &nladdr(4001, 0)), -EPERM, "to a user socket: CAP_NET_ADMIN");
    assert_eq!(p.sendto(t, b"x", &nladdr(4001, 0)), -EPERM);
    assert_eq!(p.sendto(t, b"x", &nladdr(0, 1)), -EPERM, "to a group");
    assert_eq!(connect(t, &[0, 0]), 0, "AF_UNSPEC disconnects");
    // Options.
    assert_eq!(p.setopt_int(s, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, 0), -EINVAL);
    assert_eq!(p.setopt_int(s, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, 34), -EINVAL, "beyond RTNLGRP_MAX");
    assert_eq!(p.setopt_int(s, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, 5), 0);
    assert_eq!(p.getname(s, false).unwrap(), nladdr(4001, 0x11));
    assert_eq!(p.getopt(s, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, 16).unwrap(), [0x11, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(p.getopt(s, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, 2).unwrap().len(), 8, "into 2 bytes: length 8 back");
    assert_eq!(p.getopt(t, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, 16).unwrap().len(), 0, "never joined: no group bitmap");
    assert_eq!(p.setopt_int(s, SOL_NETLINK, NETLINK_EXT_ACK, 7), 0);
    assert_eq!(p.getopt_int(s, SOL_NETLINK, NETLINK_EXT_ACK), 1);
    assert_eq!(p.getopt(s, SOL_NETLINK, NETLINK_EXT_ACK, 2), Err(-EINVAL));
    assert_eq!(p.getopt_int(s, SOL_NETLINK, 99), -ENOPROTOOPT);
    assert_eq!(p.setopt_int(s, SOL_IP, 1, 1), -ENOPROTOOPT);
    assert_eq!(p.setopt_int(s, SOL_NETLINK, NETLINK_LISTEN_ALL_NSID, 1), -EPERM);
    // Calls netlink does not have.
    assert_eq!(p.call(sys::listen, [s, 1, 0, 0, 0, 0]), -EOPNOTSUPP);
    assert_eq!(p.call(sys::accept, [s, 0, 0, 0, 0, 0]), -EOPNOTSUPP);
    assert_eq!(p.call(sys::shutdown, [s, u64::MAX, 0, 0, 0, 0]), -EOPNOTSUPP, "sock_no_shutdown before any how check");
    let sv = p.buf(8);
    assert_eq!(p.call(sys::socketpair, [AF_NETLINK as u64, SOCK_RAW as u64, 0, sv, 0, 0]), -EOPNOTSUPP);
    assert_eq!(p.send(s, b"", 0), -ENODATA);
    assert_eq!(p.send(s, &[0; 20], MSG_OOB), -EOPNOTSUPP);
    // (`s` is in groups the notification test talks to; `t` hears nothing.)
    assert_eq!(recv_now(&p, t, 16).0, -EAGAIN);
    assert_eq!(p.send(t, &alloc::vec![0; 300_000], 0), -EMSGSIZE, "above SO_SNDBUF - 32");
    assert_eq!(p.events(t), vfs::Events::OUT);
    // Root may unicast to another user socket; a connected receiver takes only its peer.
    let r = Prog::new(&ns, 4002, 0);
    let a = nl(&r);
    let b = nl(&r);
    assert_eq!(r.sendto(a, b"hi", &nladdr(7777, 0)), -ECONNREFUSED, "nobody has 7777");
    assert_eq!(r.getname(a, false).unwrap(), nladdr(4002, 0), "the send autobound to the process id");
    let at = r.put(&nladdr(7777, 0));
    assert_eq!(r.call(sys::bind, [b, at, 12, 0, 0, 0]), 0);
    assert_eq!(r.sendto(a, b"hi", &nladdr(7777, 0)), 2);
    let (n, d, from) = r.recvfrom(b, 64);
    assert_eq!((n, d.as_slice(), from), (2, &b"hi"[..], nladdr(4002, 0)), "the sender's port id");
    let at = r.put(&nladdr(9999, 0));
    assert_eq!(r.call(sys::connect, [b, at, 12, 0, 0, 0]), 0);
    assert_eq!(r.sendto(a, b"hi", &nladdr(7777, 0)), -ECONNREFUSED, "a connected receiver takes its peer only");
    for fd in [dg as u64, s, t] {
        assert_eq!(p.close(fd), 0);
    }
}

#[test]
fn requests_acks_and_dumps() {
    let _g = shared();
    let ns = ns();
    let p = Prog::new(&ns, 4011, 1000);
    let s = nl(&p);
    let dump = |ty: u16, fam: u8, seq: u32| p.send(s, &req(ty, NLM_F_REQUEST | NLM_F_DUMP, seq, &[fam]), 0);
    assert_eq!(dump(RTM_GETLINK, 0, 1), 20, "sendmsg returns the length");
    let me = u32::from_le_bytes(p.getname(s, false).unwrap()[4..8].try_into().unwrap());
    let (links, _, err, alone) = read_dump(&p, s, RTM_NEWLINK);
    assert_eq!((err, alone), (0, true), "DONE 0 in a datagram of its own after the last messages");
    assert!(links.len() >= 2 && links.iter().all(|(h, _)| h.flags == NLM_F_MULTI && h.seq == 1 && h.pid == me));
    assert_eq!(i32::from_le_bytes(links[0].1[4..8].try_into().unwrap()), 1, "lo first, index 1");
    // A small buffer: MSG_TRUNC, that datagram's rest is gone, the dump goes on.
    dump(RTM_GETLINK, 0, 2);
    let (r, _, _, fl) = p.recvmsg(s, 16, 0, MSG_DONTWAIT);
    assert_eq!((r, fl & MSG_TRUNC), (16, MSG_TRUNC));
    let (r2, d) = recv_now(&p, s, 65536);
    assert!(r2 > 0 && msgs(&d).iter().any(|m| m.0.ty == NLMSG_DONE));
    // Not requests, control messages, broken headers.
    let one = |m: Vec<u8>| -> Vec<(Hdr, Vec<u8>)> {
        assert_eq!(p.send(s, &m, 0), m.len() as i64);
        let (r, d) = recv_now(&p, s, 65536);
        if r < 0 { Vec::new() } else { msgs(&d) }
    };
    assert!(one(req(RTM_GETLINK, 0, 10, &[0; 16])).is_empty(), "no NLM_F_REQUEST, no NLM_F_ACK: nothing");
    let a = one(req(RTM_GETLINK, NLM_F_ACK, 11, &[0; 16]));
    assert_eq!((err_of(&a[0]), a[0].0.len, a[0].0.flags), (0, 36, NLM_F_CAPPED));
    assert!(one(req(NLMSG_NOOP, NLM_F_REQUEST, 12, &[])).is_empty());
    let a = one(req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK | NLM_F_DUMP, 13, &[]));
    assert_eq!(err_of(&a[0]), 0, "no payload byte: ignored, acked");
    let a = one(req(0x7fff, NLM_F_REQUEST | NLM_F_ACK, 14, &[0; 16]));
    assert_eq!((err_of(&a[0]), a[0].0.len, a[0].0.flags), (-(EOPNOTSUPP as i32), 52, 0), "the request comes back");
    let a = one(req(RTM_DELADDR, NLM_F_REQUEST, 15, &[0; 16]));
    assert_eq!(err_of(&a[0]), -(EPERM as i32), "an error is acked without NLM_F_ACK");
    let mut bad = req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 16, &[0; 16]);
    bad[0] = 8;
    assert!(one(bad.clone()).is_empty(), "nlmsg_len below the header: the datagram is dropped");
    bad[0] = 60;
    assert!(one(bad).is_empty(), "nlmsg_len beyond the datagram");
    // Two requests in one datagram: answered in order, each in its own datagram.
    let mut two = req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 20, &[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    two.extend_from_slice(&req(NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 20, &[0; 4]));
    assert_eq!(p.send(s, &two, 0), two.len() as i64);
    let kinds: Vec<(u16, i32)> = (0..3).map(|_| recv_now(&p, s, 65536)).map(|(_, d)| msgs(&d)).map(|m| (m[0].0.ty, if m[0].0.ty == NLMSG_ERROR { err_of(&m[0]) } else { 0 })).collect();
    assert_eq!(kinds, [(RTM_NEWLINK, 0), (NLMSG_ERROR, 0), (NLMSG_ERROR, 0)]);
    // A second dump while one runs: EBUSY right after the first datagram, the first still ends.
    dump(RTM_GETLINK, 0, 30);
    dump(RTM_GETADDR, 0, 31);
    let (_, d1) = recv_now(&p, s, 65536);
    assert!(msgs(&d1).iter().all(|m| m.0.ty == RTM_NEWLINK));
    let (_, d2) = recv_now(&p, s, 65536);
    let m2 = msgs(&d2);
    assert_eq!((m2[0].0.seq, err_of(&m2[0])), (31, -(EBUSY as i32)));
    let (_, d3) = recv_now(&p, s, 65536);
    assert_eq!(msgs(&d3)[0].0.ty, NLMSG_DONE);
    // NETLINK_CAP_ACK caps error acks; NETLINK_EXT_ACK adds the text of a strict-check refusal.
    p.setopt_int(s, SOL_NETLINK, NETLINK_CAP_ACK, 1);
    let a = one(req(RTM_NEWLINK, NLM_F_REQUEST, 40, &[0; 16]));
    assert_eq!((a[0].0.len, a[0].0.flags), (36, NLM_F_CAPPED));
    p.setopt_int(s, SOL_NETLINK, NETLINK_CAP_ACK, 0);
    p.setopt_int(s, SOL_NETLINK, NETLINK_GET_STRICT_CHK, 1);
    p.setopt_int(s, SOL_NETLINK, NETLINK_EXT_ACK, 1);
    dump(RTM_GETLINK, 0, 41);
    let (_, d) = recv_now(&p, s, 65536);
    let m = msgs(&d);
    assert_eq!((m[0].0.ty, m[0].0.flags), (NLMSG_DONE, NLM_F_MULTI | NLM_F_ACK_TLVS));
    assert_eq!(i32::from_le_bytes(m[0].1[..4].try_into().unwrap()), -(EINVAL as i32), "strict: the error rides in DONE");
    assert!(attrs(&m[0].1[4..]).iter().any(|(t, v)| *t == NLMSGERR_ATTR_MSG && v.starts_with(b"Invalid header for link dump")));
    let mut ifa = [0u8; 8];
    ifa[0] = 2;
    ifa[4] = 1;
    assert_eq!(p.send(s, &req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 42, &ifa), 0), 24);
    let (addrs, _, err, _) = read_dump(&p, s, RTM_NEWADDR);
    assert_eq!((addrs.len(), err), (1, 0), "strict: filtered by index");
    assert_eq!(addrs[0].0.flags, NLM_F_MULTI | NLM_F_DUMP_FILTERED);
    // PKTINFO and the sender's address on recvmsg; SO_PASSCRED gives the kernel's credentials.
    p.setopt_int(s, SOL_NETLINK, NETLINK_PKTINFO, 1);
    p.send(s, &req(NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 43, &[]), 0);
    let (r, _, ctl, _) = p.recvmsg(s, 256, 64, MSG_DONTWAIT);
    assert_eq!(r, 36);
    assert_eq!(&ctl[8..16], &[0x0e, 1, 0, 0, 3, 0, 0, 0], "SOL_NETLINK / NETLINK_PKTINFO");
    assert_eq!(&ctl[16..20], &[0, 0, 0, 0], "group 0: unicast");
    p.setopt_int(s, SOL_NETLINK, NETLINK_PKTINFO, 0);
    p.setopt_int(s, SOL_SOCKET, SO_PASSCRED, 1);
    p.send(s, &req(NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 44, &[]), 0);
    let (_, _, ctl, _) = p.recvmsg(s, 256, 64, MSG_DONTWAIT);
    assert_eq!(&ctl[12..16], &SCM_CREDENTIALS.to_le_bytes());
    assert_eq!(&ctl[16..28], &[0u8; 12], "pid 0 uid 0 gid 0: the kernel");
    p.send(s, &req(NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 45, &[]), 0);
    let (n, _, from) = p.recvfrom(s, 64);
    assert_eq!((n, from), (36, nladdr(0, 0)), "from the kernel");
    assert_eq!(p.events(s), vfs::Events::OUT);
    p.close(s);
}

#[test]
fn interface_ioctls() {
    let _g = shared();
    let ns = ns();
    let p = Prog::new(&ns, 4021, 1000);
    view();
    let udp = p.socket(AF_INET, SOCK_DGRAM) as u64;
    let unix = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    let ifr = |name: &[u8], int: u32| {
        let mut r = [0x55u8; 40];
        r[..16].fill(0);
        r[..name.len()].copy_from_slice(name);
        r[16..20].copy_from_slice(&int.to_le_bytes());
        p.put(&r)
    };
    let io = |fd: u64, cmd: u64, at: u64| sys::dev_ioctl(&p.sys(), [fd, cmd, at, 0, 0, 0]);
    let r = ifr(b"", 1);
    assert_eq!(io(unix, SIOCGIFNAME, r), Some(0));
    assert_eq!(&p.get(r, 3), b"lo\0");
    let r = ifr(b"", 2);
    assert_eq!(io(udp, SIOCGIFNAME, r), Some(0));
    assert_eq!(&p.get(r, 5), b"eth9\0");
    assert_eq!(io(udp, SIOCGIFNAME, ifr(b"", 99)), Some(-ENODEV));
    let r = ifr(b"eth9:3", 0);
    assert_eq!(io(unix, SIOCGIFINDEX, r), Some(0));
    assert_eq!((p.u32_at(r + 16), &p.get(r, 6)[..]), (2, &b"eth9:3"[..]), "alias cut for the lookup, kept in the answer");
    assert_eq!(io(unix, SIOCGIFINDEX, ifr(b"0123456789abcdef", 0)), Some(-ENODEV), "byte 15 becomes NUL");
    let r = ifr(b"lo", 0);
    assert_eq!(io(udp, SIOCGIFFLAGS, r), Some(0));
    assert_eq!(&p.get(r + 16, 4), &[0x49, 0, 0, 0], "a short: IFF_LOWER_UP does not fit");
    let r = ifr(b"eth9", 0);
    assert_eq!(io(udp, SIOCGIFHWADDR, r), Some(0));
    assert_eq!(&p.get(r + 16, 10), &[1, 0, 2, 0, 0, 0, 0, 9, 0x55, 0x55]);
    let r = ifr(b"eth9", 0);
    assert_eq!(io(udp, SIOCGIFMTU, r), Some(0));
    assert_eq!(p.u32_at(r + 16), 1500);
    let r = ifr(b"eth9", 0);
    assert_eq!(io(udp, SIOCGIFADDR, r), Some(0));
    assert_eq!(&p.get(r + 16, 17), &[2, 0, 0, 0, 10, 1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0x55]);
    let r = ifr(b"eth9", 0);
    assert_eq!(io(udp, SIOCGIFBRDADDR, r), Some(0));
    assert_eq!(&p.get(r + 20, 4), &[10, 1, 2, 255]);
    let r = ifr(b"lo", 0);
    assert_eq!(io(udp, SIOCGIFNETMASK, r), Some(0));
    assert_eq!(&p.get(r + 20, 4), &[255, 0, 0, 0]);
    assert_eq!(io(udp, SIOCGIFADDR, ifr(b"lo:1", 0)), Some(-EADDRNOTAVAIL), "no such label");
    assert_eq!(io(unix, SIOCGIFADDR, ifr(b"lo", 0)), Some(-ENOTTY), "address ioctls are AF_INET's");
    assert_eq!(io(udp, SIOCGIFPFLAGS, ifr(b"lo", 0)), Some(-EINVAL));
    assert_eq!(io(udp, SIOCSIFFLAGS, ifr(b"lo", 0)), Some(-EPERM));
    assert_eq!(io(unix, SIOCSIFMTU, ifr(b"nope", 0)), Some(-EPERM), "permission before the name");
    assert_eq!(io(unix, SIOCGIFINDEX, 8), Some(-EFAULT));
    assert_eq!(io(unix, 0x8999, 8), Some(-EFAULT), "every 0x89xx command copies the ifreq first");
    assert_eq!(io(unix, 0x8999, ifr(b"lo", 0)), Some(-ENOTTY));
    assert_eq!(io(unix, SIOCDEVPRIVATE, ifr(b"nope", 0)), Some(-ENODEV));
    assert_eq!(io(unix, SIOCGIFMEM, ifr(b"lo", 0)), Some(-ENOTTY));
    assert_eq!(io(unix, 0x541B, ifr(b"lo", 0)), None, "FIONREAD is the socket's own");
    assert_eq!(io(udp, SIOCGPGRP, ifr(b"lo", 0)), None);
    assert_eq!(io(udp, SIOCATMARK, 8), Some(-EFAULT), "UDP has no SIOCATMARK: the device path reads the ifreq first");
    assert_eq!(io(udp, SIOCATMARK, ifr(b"lo", 0)), Some(-ENOTTY));
    let tcp = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(io(tcp, SIOCATMARK, 8), None, "tcp_ioctl's own");
    // SIOCGIFCONF: count with no buffer, then the entries.
    let c = p.put(&[0u8; 16]);
    assert_eq!(io(unix, SIOCGIFCONF, c), Some(0));
    assert_eq!(p.u32_at(c), 80, "lo and eth9");
    let buf = p.buf(80);
    let mut ic = 80u32.to_le_bytes().to_vec();
    ic.extend_from_slice(&[0; 4]);
    ic.extend_from_slice(&buf.to_le_bytes());
    let c = p.put(&ic);
    assert_eq!(io(unix, SIOCGIFCONF, c), Some(0));
    assert_eq!(&p.get(buf + 40, 24), &[b'e', b't', b'h', b'9', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 10, 1, 2, 3]);
    let mut short = 39u32.to_le_bytes().to_vec();
    short.extend_from_slice(&ic[4..]);
    let c = p.put(&short);
    assert_eq!(io(unix, SIOCGIFCONF, c), Some(0));
    assert_eq!(p.u32_at(c), 0);
    // ethtool's link state.
    let ev = p.put(&[0x0a, 0, 0, 0, 77, 0, 0, 0]);
    let mut er = [0u8; 40];
    er[..2].copy_from_slice(b"lo");
    er[16..24].copy_from_slice(&ev.to_le_bytes());
    assert_eq!(io(udp, SIOCETHTOOL, p.put(&er)), Some(0));
    assert_eq!(p.u32_at(ev + 4), 1);
    // SO_BINDTODEVICE with names now.
    let setdev = |fd: u64, n: &[u8]| {
        let a = p.put(n);
        p.call(sys::setsockopt, [fd, SOL_SOCKET as u64, SO_BINDTODEVICE as u64, a, n.len() as u64, 0])
    };
    assert_eq!(setdev(udp, b"nope\0"), -ENODEV);
    assert_eq!(p.getopt(udp, SOL_SOCKET, SO_BINDTODEVICE, 16).unwrap().len(), 0);
    assert_eq!(setdev(udp, b"eth9\0"), 0);
    assert_eq!(p.getopt(udp, SOL_SOCKET, SO_BINDTODEVICE, 16).unwrap(), b"eth9\0");
    assert_eq!(p.getopt(udp, SOL_SOCKET, SO_BINDTODEVICE, 15), Err(-EINVAL));
    assert_eq!(setdev(udp, b""), -EPERM, "changing a binding needs CAP_NET_RAW");
}

#[test]
fn notifications_and_overruns() {
    let _g = shared();
    let ns = ns();
    let p = Prog::new(&ns, 4031, 1000);
    let st = view();
    let s = nl(&p);
    let at = p.put(&nladdr(0, 1 | 0x10));
    assert_eq!(p.call(sys::bind, [s, at, 12, 0, 0, 0]), 0, "RTMGRP_LINK | RTMGRP_IPV4_IFADDR, no root needed");
    // A link change: RTM_NEWLINK to group 1, from the kernel.
    CARD.up.store(false, Ordering::Relaxed);
    crate::stack::Poller::new(st).round();
    settle();
    let mut seen = Vec::new();
    while let (r, d) = recv_now(&p, s, 65536)
        && r > 0
    {
        seen.extend(msgs(&d).into_iter().map(|m| (m.0.ty, i32::from_le_bytes(m.1[4..8].try_into().unwrap()), u32::from_le_bytes(m.1[8..12].try_into().unwrap()))));
    }
    assert!(seen.contains(&(RTM_NEWLINK, 2, 0x1003)), "eth9 without carrier: {seen:?}");
    CARD.up.store(true, Ordering::Relaxed);
    crate::stack::Poller::new(st).round();
    settle();
    let (r, _, ctl, _) = {
        p.setopt_int(s, SOL_NETLINK, NETLINK_PKTINFO, 1);
        p.recvmsg(s, 65536, 64, MSG_DONTWAIT)
    };
    assert!(r > 0);
    assert_eq!(&ctl[16..20], &1u32.to_le_bytes(), "PKTINFO: group RTNLGRP_LINK");
    // A new address: RTM_DELADDR of the old one, RTM_NEWADDR of the new one, to group 5.
    while recv_now(&p, s, 65536).0 > 0 {}
    st.set_static("eth9", Ipv4([10, 1, 2, 4]), 24, None).unwrap();
    settle();
    let mut kinds = Vec::new();
    while let (r, d) = recv_now(&p, s, 65536)
        && r > 0
    {
        kinds.extend(msgs(&d).into_iter().map(|m| (m.0.ty, m.1[8..].windows(4).any(|w| w == [10, 1, 2, 4]))));
    }
    assert_eq!(kinds, [(RTM_DELADDR, false), (RTM_NEWADDR, true)]);
    st.set_static("eth9", Ipv4([10, 1, 2, 3]), 24, None).unwrap();
    // A full queue: the first drop sets ENOBUFS (reported once), later ones are only counted.
    p.setopt_int(s, SOL_SOCKET, SO_RCVBUF, 1);
    for _ in 0..8 {
        st.set_static("eth9", Ipv4([10, 1, 2, 5]), 24, None).unwrap();
        st.set_static("eth9", Ipv4([10, 1, 2, 3]), 24, None).unwrap();
    }
    settle();
    assert_eq!(p.events(s) & vfs::Events::ERR, vfs::Events::ERR);
    assert_eq!(recv_now(&p, s, 65536).0, -ENOBUFS, "the error first");
    assert!(recv_now(&p, s, 65536).0 > 0, "then the queue");
    p.close(s);
}

#[test]
fn dump_parts_follow_the_reader() {
    let _g = shared();
    // `netlink_dump`: the first part fits a page (NLMSG_GOODSIZE); once the reader has read with a larger
    // buffer, parts grow to its size (at most 32 KiB less the overhead); DONE follows in a part of its own.
    let c = super::Caller { pid: 4041, uid: 1000, gid: 1000, subject: policy::Subject::KERNEL };
    let n = NetlinkSock::create(SOCK_RAW, 0, &c).unwrap();
    let msgs_in: alloc::collections::VecDeque<Vec<u8>> = (0..40u8)
        .map(|i| {
            let mut m = Msg::new(RTM_NEWLINK, NLM_F_MULTI, 9, 0, &[0u8; 16]);
            m.attr(1, &[i; 300]);
            m.done()
        })
        .collect();
    let each = msgs_in[0].len();
    n.start_dump(Dump { msgs: msgs_in, done_errno: i32::MAX, done_msg: None, seq: 9, answer_flags: 0 }, 77).unwrap();
    let mut sizes = Vec::new();
    loop {
        let mut got = Vec::new();
        let r = n.recv(65536, MSG_DONTWAIT, true, &mut |d| {
            got.extend_from_slice(d);
            Ok(())
        });
        let Ok(r) = r else { break };
        sizes.push((r.n, msgs(&got).iter().any(|m| m.0.ty == NLMSG_DONE)));
    }
    assert!(sizes[0].0 <= 4096 - 320 && sizes[0].0 > 4096 - 320 - each, "first part: one page ({sizes:?})");
    assert!(sizes[1].0 > 4096, "then as large as the reader's buffer: {sizes:?}");
    assert_eq!(sizes.last(), Some(&(20, true)), "DONE alone");
    assert_eq!(sizes.iter().map(|s| s.0).sum::<usize>(), 40 * each + 20);
    n.release();
}

#[test]
fn membership_follows_groups_under_races() {
    let _g = shared();
    view();
    // Two threads join and leave different groups of one socket at once: afterwards the member table agrees
    // with the groups the socket really has (membership is decided under the table's lock, checker NW-6 #1).
    let c = super::Caller { pid: 4061, uid: 1000, gid: 1000, subject: policy::Subject::KERNEL };
    for keep in [false, true] {
        let s = NetlinkSock::create(SOCK_RAW, 0, &c).unwrap();
        let t: Vec<_> = [1u32, 5]
            .into_iter()
            .map(|g| {
                let s = s.clone();
                std::thread::spawn(move || {
                    let c = super::Caller { pid: 4061, uid: 1000, gid: 1000, subject: policy::Subject::KERNEL };
                    for _ in 0..2000 {
                        s.setsockopt(NETLINK_ADD_MEMBERSHIP, g, &c).unwrap();
                        s.setsockopt(NETLINK_DROP_MEMBERSHIP, g, &c).unwrap();
                    }
                    if keep && g == 5 {
                        s.setsockopt(NETLINK_ADD_MEMBERSHIP, g, &c).unwrap();
                    }
                })
            })
            .collect();
        for h in t {
            h.join().unwrap();
        }
        let groups = s.getsockopt(NETLINK_LIST_MEMBERSHIPS, 8).unwrap().0;
        let joined = groups.iter().any(|b| *b != 0);
        assert_eq!(joined, keep);
        assert_eq!(is_member(&s), keep, "the member table follows the groups");
        s.release();
        assert!(!is_member(&s), "a released socket is no member");
    }
}

#[test]
fn hostile_requests_and_bindtoifindex() {
    let _g = shared();
    let ns = ns();
    let p = Prog::new(&ns, 4071, 1000);
    let s = nl(&p);
    let answers = |m: &[u8]| -> Vec<Vec<(Hdr, Vec<u8>)>> {
        assert_eq!(p.send(s, m, 0), m.len() as i64, "sendmsg takes every datagram whole");
        let mut v = Vec::new();
        while let (r, d) = recv_now(&p, s, 65536)
            && r > 0
        {
            v.push(msgs(&d));
        }
        v
    };
    let lo_req = |flags: u16, seq: u32, tail: &[u8]| {
        let mut b = alloc::vec![0u8; 16];
        b[4] = 1;
        b.extend_from_slice(tail);
        req(RTM_GETLINK, flags, seq, &b)
    };
    // A request then a header whose length runs past the datagram: the first is answered, the rest dropped.
    let mut m = lo_req(NLM_F_REQUEST | NLM_F_ACK, 1, &[]);
    let mut broken = req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 2, &[0; 16]);
    broken[0..4].copy_from_slice(&200u32.to_le_bytes());
    m.extend_from_slice(&broken);
    let a = answers(&m);
    assert_eq!(a.len(), 2, "NEWLINK and its ack, nothing for the broken one");
    assert_eq!((a[0][0].0.ty, err_of(&a[1][0]), a[1][0].0.seq), (RTM_NEWLINK, 0, 1));
    // nlmsg_len 0xffffffff, and fewer than 16 bytes: nothing.
    broken[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(answers(&broken).is_empty());
    assert!(answers(&broken[..10]).is_empty());
    // A GETLINK without payload: "at least 1 byte", acked as done.
    let a = answers(&req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 3, &[]));
    assert_eq!((a.len(), err_of(&a[0][0])), (1, 0));
    // A 1-byte payload is too short for a get link: EINVAL.
    let a = answers(&req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 4, &[0]));
    assert_eq!(err_of(&a[0][0]), -(EINVAL as i32));
    // A truncated attribute (rta_len 20 in 8 bytes) and one shorter than its header: ignored without strict
    // checking; the link is answered.
    let a = answers(&lo_req(NLM_F_REQUEST | NLM_F_ACK, 5, &[20, 0, 3, 0, b'l', b'o', 0, 0]));
    assert_eq!((a[0][0].0.ty, err_of(&a[1][0])), (RTM_NEWLINK, 0));
    let a = answers(&lo_req(NLM_F_REQUEST | NLM_F_ACK, 6, &[2, 0, 3, 0]));
    assert_eq!(a[0][0].0.ty, RTM_NEWLINK);
    // An empty IFLA_IFNAME: ERANGE (policy).
    let a = answers(&req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 7, &[0u8; 16].iter().copied().chain([4, 0, 3, 0]).collect::<Vec<u8>>()));
    assert_eq!(err_of(&a[0][0]), -(ERANGE as i32));
    // IPv6 GETADDR of one address (`inet6_rtm_getaddr`): V2 has none.
    let ifa6 = |tail: &[u8]| [10u8, 0, 0, 0, 0, 0, 0, 0].iter().copied().chain(tail.iter().copied()).collect::<Vec<u8>>();
    let mut v6 = alloc::vec![20u8, 0, 1, 0, 0x20, 1, 0x0d, 0xb8];
    v6.extend_from_slice(&[0; 11]);
    v6.push(1);
    let e = |m: Vec<u8>| err_of(&answers(&m)[0][0]);
    assert_eq!(e(req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 8, &ifa6(&v6))), -(EADDRNOTAVAIL as i32));
    assert_eq!(e(req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 9, &ifa6(&[]))), -(EINVAL as i32), "no address");
    assert_eq!(e(req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 10, &[10])), -(EINVAL as i32), "short header");
    assert_eq!(e(req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 11, &ifa6(&[8, 0, 1, 0, 1, 2, 3, 4]))), -(ERANGE as i32), "4-byte IFA_ADDRESS");
    assert_eq!(e(req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 12, &[2, 0, 0, 0, 0, 0, 0, 0])), -(EOPNOTSUPP as i32), "AF_INET: dump only");
    // Strict checking with texts: leftover bytes, IPv6 header values and attributes.
    p.setopt_int(s, SOL_NETLINK, NETLINK_GET_STRICT_CHK, 1);
    p.setopt_int(s, SOL_NETLINK, NETLINK_EXT_ACK, 1);
    let text = |m: Vec<u8>| -> (i32, Vec<u8>) {
        let a = answers(&m);
        let (h, b) = &a[0][0];
        assert_eq!(h.ty, NLMSG_ERROR);
        let skip = if h.flags & NLM_F_CAPPED != 0 { 20 } else { 4 + u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize };
        let t = attrs(&b[align(skip)..]).into_iter().find(|(t, _)| *t == NLMSGERR_ATTR_MSG).map(|(_, d)| d.strip_suffix(&[0]).unwrap_or(d).to_vec()).unwrap_or_default();
        (err_of(&a[0][0]), t)
    };
    let mut lo = alloc::vec![0u8; 16];
    lo[4] = 1;
    lo.extend_from_slice(&[20, 0, 3, 0, b'l', b'o', 0, 0]);
    assert_eq!(text(req(RTM_GETLINK, NLM_F_REQUEST, 13, &lo)), (-(EINVAL as i32), b"bytes leftover after parsing attributes".to_vec()));
    assert_eq!(text(req(RTM_GETADDR, NLM_F_REQUEST, 14, &[10])), (-(EINVAL as i32), b"ipv6: Invalid header for get address request".to_vec()));
    assert_eq!(text(req(RTM_GETADDR, NLM_F_REQUEST, 15, &[10, 64, 0, 0, 0, 0, 0, 0])), (-(EINVAL as i32), b"ipv6: Invalid values in header for get address request".to_vec()));
    assert_eq!(text(req(RTM_GETADDR, NLM_F_REQUEST, 16, &ifa6(&[7, 0, 3, 0, b'l', b'o', 0, 0]))), (-(EINVAL as i32), b"ipv6: Unsupported attribute in get address request".to_vec()));
    let (_, d) = {
        p.send(s, &req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 17, &[10]), 0);
        recv_now(&p, s, 65536)
    };
    let done = &msgs(&d)[0];
    assert_eq!(done.0.ty, NLMSG_DONE);
    assert!(done.1.windows(6).any(|w| w == b"ipv6: "), "IPv6 texts carry the module name as on Linux");
    p.close(s);
    // SO_BINDTOIFINDEX (`sock_bindtoindex_locked`): any index >= 0, EPERM to change it without root.
    let u = p.call(sys::socket, [AF_INET as u64, SOCK_DGRAM as u64, 0, 0, 0, 0]) as u64;
    let set = |v: i32| {
        let a = p.put(&v.to_le_bytes());
        p.call(sys::setsockopt, [u, SOL_SOCKET as u64, SO_BINDTOIFINDEX as u64, a, 4, 0])
    };
    assert_eq!(p.getopt(u, SOL_SOCKET, SO_BINDTOIFINDEX, 4).unwrap(), 0u32.to_le_bytes());
    assert_eq!(set(-1), -EINVAL);
    assert_eq!(set(9999), 0, "an index without an interface is taken");
    assert_eq!(p.getopt(u, SOL_SOCKET, SO_BINDTOIFINDEX, 4).unwrap(), 9999u32.to_le_bytes());
    assert_eq!(p.getopt(u, SOL_SOCKET, SO_BINDTODEVICE, 16), Err(-ENODEV));
    assert_eq!(set(1), -EPERM);
    p.close(u);
    let u = p.call(sys::socket, [AF_INET as u64, SOCK_DGRAM as u64, 0, 0, 0, 0]) as u64;
    let a = p.put(&1i32.to_le_bytes());
    assert_eq!(p.call(sys::setsockopt, [u, SOL_SOCKET as u64, SO_BINDTOIFINDEX as u64, a, 4, 0]), 0);
    assert_eq!(p.getopt(u, SOL_SOCKET, SO_BINDTODEVICE, 16).unwrap(), b"lo\0");
    p.close(u);
}
