//! NW-11 host tests: network namespace views (lo, ports, abstract names, rtnetlink, ioctls) and credentials
//! translated between pid and user namespaces. The test thread's namespaces come from a thread-local the
//! namespace hook reads, so every other test (other threads) stays in the initial namespaces.

use super::abi::*;
use super::addr::encode_in;
use super::devioctl::{SIOCGIFCONF, SIOCGIFFLAGS, SIOCGIFINDEX, SIOCSIFFLAGS};
use super::nlmsg::*;
use super::tests::{Prog, ns, sun};
use super::{Ids, NsHooks, Ucred, sys};
use crate::netns::{Net, NetView};
use crate::{Ipv4, SockAddr};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use uapi::errno::*;

type NsPair = (Net, Option<Arc<dyn Ids>>);

std::thread_local! {
    static CUR: RefCell<NsPair> = const { RefCell::new((None, None)) };
}

fn cur() -> super::NsNow {
    CUR.with(|c| {
        let (net, ids) = c.borrow().clone();
        super::NsNow { net, ids, cred: None }
    })
}

static HOOKS: NsHooks = NsHooks { current: cur };

/// Calls on this thread from now on come from namespaces `net` / `ids`.
fn enter(net: &Net, ids: Option<Arc<dyn Ids>>) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| super::set_ns_hooks(&HOOKS));
    CUR.with(|c| *c.borrow_mut() = (net.clone(), ids));
}

fn init() {
    enter(&None, None);
}

fn stack() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| super::inet::set_stack(crate::stack::Stack::new_static(super::now, 7, false)));
}

/// A pid and user namespace: `pids` maps kernel pids to the namespace's numbers, `uid`/`gid` one kernel id to
/// one namespace id (as `unshare -r` writes `0 1000 1`); `caps`: every capability there.
struct FakeIds {
    pids: Vec<(u32, u32)>,
    uid: (u32, u32),
    gid: (u32, u32),
    caps: bool,
    /// Every capability over the initial user namespace too (root of it in a new pid or net namespace).
    init_caps: bool,
}

impl Ids for FakeIds {
    fn capable(&self, _cap: u32) -> bool {
        self.caps
    }
    fn capable_initial(&self, _cap: u32) -> bool {
        self.init_caps
    }
    fn pid_to_kernel(&self, pid: u32) -> Option<u32> {
        self.pids.iter().find(|p| p.1 == pid).map(|p| p.0)
    }
    fn pid_from_kernel(&self, kpid: u32) -> u32 {
        self.pids.iter().find(|p| p.0 == kpid).map_or(0, |p| p.1)
    }
    fn uid_to_kernel(&self, uid: u32) -> Option<u32> {
        (uid == self.uid.1).then_some(self.uid.0)
    }
    fn gid_to_kernel(&self, gid: u32) -> Option<u32> {
        (gid == self.gid.1).then_some(self.gid.0)
    }
    fn uid_from_kernel(&self, kuid: u32) -> u32 {
        if kuid == self.uid.0 { self.uid.1 } else { 65534 }
    }
    fn gid_from_kernel(&self, kgid: u32) -> u32 {
        if kgid == self.gid.0 { self.gid.1 } else { 65534 }
    }
}

/// Root of a user namespace mapping kernel uid/gid 1000 to 0, kernel pid `kpid` as 1.
fn ns_root(kpid: u32, caps: bool) -> Arc<dyn Ids> {
    Arc::new(FakeIds { pids: alloc::vec![(kpid, 1)], uid: (1000, 0), gid: (1000, 0), caps, init_caps: false })
}

fn view(inum: u32) -> Net {
    Some(NetView::new(inum))
}

fn up(net: &Net) {
    net.as_ref().unwrap().change_lo_flags(crate::ifview::IFF_UP).unwrap();
}

fn lo(port: u16) -> Vec<u8> {
    encode_in(SockAddr { ip: Ipv4::LOCALHOST, port })
}

fn any(port: u16) -> Vec<u8> {
    encode_in(SockAddr { ip: Ipv4::ANY, port })
}

fn bind(p: &Prog, fd: u64, a: &[u8]) -> i64 {
    let at = p.put(a);
    p.call(sys::bind, [fd, at, a.len() as u64, 0, 0, 0])
}

fn connect(p: &Prog, fd: u64, a: &[u8]) -> i64 {
    let at = p.put(a);
    p.call(sys::connect, [fd, at, a.len() as u64, 0, 0, 0])
}

/// A non-blocking accept (the listener is made O_NONBLOCK first).
fn accept(p: &Prog, fd: u64) -> i64 {
    p.set_nonblock(fd);
    p.call(sys::accept4, [fd, 0, 0, SOCK_NONBLOCK, 0, 0])
}

/// `[::1]:port`.
fn sin6_lo(port: u16) -> Vec<u8> {
    let mut v = AF_INET6.to_le_bytes().to_vec();
    v.extend_from_slice(&port.to_be_bytes());
    v.extend_from_slice(&[0; 4]);
    let mut one = [0u8; 16];
    one[15] = 1;
    v.extend_from_slice(&one);
    v.extend_from_slice(&[0; 4]);
    v
}

fn ioctl(p: &Prog, fd: u64, cmd: u64, arg: u64) -> i64 {
    sys::dev_ioctl(&p.sys(), [fd, cmd, arg, 0, 0, 0]).expect("an interface ioctl")
}

/// An ifreq naming `name`, with `flags` in ifr_flags.
fn ifreq(p: &Prog, name: &[u8], flags: u16) -> u64 {
    let mut r = [0u8; 40];
    r[..name.len()].copy_from_slice(name);
    r[16..18].copy_from_slice(&flags.to_le_bytes());
    p.put(&r)
}

/// SIOCGIFCONF's byte count (NULL buffer: what the entries need).
fn ifconf_len(p: &Prog, fd: u64) -> i32 {
    let c = p.put(&[0u8; 16]);
    assert_eq!(ioctl(p, fd, SIOCGIFCONF, c), 0);
    i32::from_le_bytes(p.get(c, 4).try_into().unwrap())
}

#[test]
fn lo_of_a_new_namespace_starts_down() {
    let fs = ns();
    let p = Prog::new(&fs, 61, 1000);
    let net = view(0xF000_0001);
    let links = crate::ifview::links_in(&net);
    assert_eq!(links.len(), 1, "only lo");
    let l = &links[0];
    assert_eq!((l.index, l.name.as_str(), l.flags(), l.operstate(), l.addrs.len()), (1, "lo", 0x8, crate::ifview::IF_OPER_DOWN, 0));
    enter(&net, Some(ns_root(61, false)));
    let s = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(ifconf_len(&p, s), 0, "no address before the first UP");
    let r = ifreq(&p, b"lo", 0);
    assert_eq!(ioctl(&p, s, SIOCGIFINDEX, r), 0);
    assert_eq!(p.u32_at(r + 16), 1);
    assert_eq!(ioctl(&p, s, SIOCGIFINDEX, ifreq(&p, b"eth0", 0)), -ENODEV, "no card in a namespace");
    // No local table before lo's first UP: CachyOS binds any IPv4 address; ::1 is no address of lo yet.
    let fresh = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, fresh, &encode_in(SockAddr { ip: Ipv4([192, 0, 2, 1]), port: 0 })), 0);
    let fresh6 = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, fresh6, &sin6_lo(0)), -EADDRNOTAVAIL);
    assert_eq!(p.sendto(fresh6, b"x", &sin6_lo(9)), -EADDRNOTAVAIL, "no source address for ::1");
    assert_eq!(p.sendto(s, b"x", &lo(9)), -ENETUNREACH, "no route");
    assert_eq!(ioctl(&p, s, SIOCSIFFLAGS, ifreq(&p, b"lo", 1)), -EPERM, "no CAP_NET_ADMIN");
    enter(&net, Some(ns_root(61, true)));
    assert_eq!(ioctl(&p, s, SIOCSIFFLAGS, ifreq(&p, b"nope", 1)), -ENODEV);
    let flags = |p: &Prog| {
        let r = ifreq(p, b"lo", 0);
        assert_eq!(ioctl(p, s, SIOCGIFFLAGS, r), 0);
        u16::from_le_bytes(p.get(r + 16, 2).try_into().unwrap())
    };
    assert_eq!(ioctl(&p, s, SIOCSIFFLAGS, ifreq(&p, b"lo", 0x101)), 0, "IFF_PROMISC is kept");
    assert_eq!(flags(&p), 0x149, "UP, LOOPBACK, RUNNING, PROMISC");
    assert_eq!(ioctl(&p, s, SIOCSIFFLAGS, ifreq(&p, b"lo", 0x41)), 0, "RUNNING is not the program's");
    assert_eq!(flags(&p), 0x49);
    assert_eq!(ifconf_len(&p, s), 40, "127.0.0.1");
    let b = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, b, &encode_in(SockAddr { ip: Ipv4([192, 0, 2, 1]), port: 0 })), -EADDRNOTAVAIL, "a local table now");
    assert_eq!(bind(&p, b, &lo(24710)), 0);
    assert_eq!(bind(&p, fresh6, &sin6_lo(0)), 0, "::1 while lo is UP");
    // Down again: 127.0.0.1 and its route stay (IPv4), datagrams to it are dropped; ::1 goes.
    assert_eq!(ioctl(&p, s, SIOCSIFFLAGS, ifreq(&p, b"lo", 0)), 0);
    let l = crate::ifview::links_in(&net).remove(0);
    assert_eq!((l.flags(), l.addrs.len()), (0x8, 1));
    let u = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, u, &lo(24711)), 0);
    assert_eq!(p.sendto(u, b"x", &lo(24711)), 1, "sent and dropped");
    assert_eq!(p.recv(u, 4, MSG_DONTWAIT).0, -EAGAIN);
    let u6 = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(u6, b"x", &sin6_lo(9)), -EADDRNOTAVAIL);
    // The initial namespace's interfaces are the kernel's: unchanged answers there.
    init();
    let i = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(ioctl(&p, i, SIOCSIFFLAGS, ifreq(&p, b"lo", 1)), -EPERM, "uid 1000");
    let root = Prog::new(&fs, 62, 0);
    let j = root.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(ioctl(&root, j, SIOCSIFFLAGS, ifreq(&root, b"lo", 1)), -EOPNOTSUPP, "root: not changeable");
    let lo0 = crate::ifview::links_in(&None).remove(0);
    assert_eq!((lo0.flags(), lo0.addrs.len()), (0x10049, 1), "initial lo untouched");
    // A socket keeps its namespace: the one made in `net` still sees lo down from the initial namespace.
    assert_eq!(ifconf_len(&p, s), 40);
    assert_eq!(ioctl(&p, s, SIOCGIFINDEX, ifreq(&p, b"lo", 0)), 0);
    for fd in [s, u, i, fresh, fresh6, b, u6] {
        p.close(fd);
    }
    root.close(j);
    assert_eq!(Arc::strong_count(net.as_ref().unwrap()), 1, "closed sockets keep no view");
}

fn req(ty: u16, flags: u16, seq: u32, body: &[u8]) -> Vec<u8> {
    let mut m = Msg::new(ty, flags, seq, 0, body);
    m.set_flags(flags);
    m.done()
}

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

/// Every message waiting on `fd` (non-blocking reads until EAGAIN).
fn drain(p: &Prog, fd: u64) -> Vec<(Hdr, Vec<u8>)> {
    let mut v = Vec::new();
    loop {
        let (r, d) = p.recv(fd, 65536, MSG_DONTWAIT);
        if r <= 0 {
            return v;
        }
        v.extend(msgs(&d[..r as usize]));
    }
}

fn ifinfo(index: i32, flags: u32, change: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[4..8].copy_from_slice(&index.to_le_bytes());
    b[8..12].copy_from_slice(&flags.to_le_bytes());
    b[12..16].copy_from_slice(&change.to_le_bytes());
    b
}

/// The error of the NLMSG_ERROR answering a request (0: an ACK).
fn ack(m: &[(Hdr, Vec<u8>)]) -> i32 {
    let (_, b) = m.iter().find(|(h, _)| h.ty == NLMSG_ERROR).expect("an ack");
    i32::from_le_bytes(b[..4].try_into().unwrap())
}

const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;

#[test]
fn rtnetlink_of_a_namespace() {
    let fs = ns();
    let p = Prog::new(&fs, 63, 1000);
    // A member of the initial namespace's link group: nothing about the namespace's lo reaches it.
    init();
    let outside = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    assert_eq!(p.setopt_int(outside, super::netlink::SOL_NETLINK, 1, 1), 0, "NETLINK_ADD_MEMBERSHIP RTNLGRP_LINK");
    let net = view(0xF000_0002);
    enter(&net, Some(ns_root(63, false)));
    let n = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    let member = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    for g in [1, 5] {
        assert_eq!(p.setopt_int(member, super::netlink::SOL_NETLINK, 1, g), 0, "RTNLGRP_LINK and RTNLGRP_IPV4_IFADDR");
    }
    assert_eq!(p.send(n, &req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, &[0u8; 16]), 0), 32);
    let links: Vec<_> = drain(&p, n).into_iter().filter(|(h, _)| h.ty == RTM_NEWLINK).collect();
    assert_eq!(links.len(), 1, "lo only");
    assert_eq!((i32::from_le_bytes(links[0].1[4..8].try_into().unwrap()), u32::from_le_bytes(links[0].1[8..12].try_into().unwrap())), (1, 0x8));
    let mut ga = alloc::vec![2u8];
    ga.resize(8, 0);
    p.send(n, &req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 2, &ga), 0);
    assert!(drain(&p, n).iter().all(|(h, _)| h.ty != RTM_NEWADDR), "no address yet");
    let set_up = req(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, 3, &ifinfo(1, 1, 1));
    p.send(n, &set_up, 0);
    assert_eq!(ack(&drain(&p, n)), -(EPERM as i32), "no CAP_NET_ADMIN");
    enter(&net, Some(ns_root(63, true)));
    p.send(n, &set_up, 0);
    assert_eq!(ack(&drain(&p, n)), -(EPERM as i32), "opened without CAP_NET_ADMIN (`netlink_net_capable`)");
    p.close(n);
    let n = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    p.send(n, &req(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK | 0x200, 4, &ifinfo(1, 1, 1)), 0);
    assert_eq!(ack(&drain(&p, n)), -(EEXIST as i32), "NLM_F_EXCL on an existing device");
    p.send(n, &req(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 5, &ifinfo(0, 1, 1)), 0);
    assert_eq!(ack(&drain(&p, n)), -(EINVAL as i32), "RTM_SETLINK without index or name");
    p.send(n, &req(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 6, &ifinfo(7, 1, 1)), 0);
    assert_eq!(ack(&drain(&p, n)), -(ENODEV as i32));
    p.send(n, &set_up, 0);
    assert_eq!(ack(&drain(&p, n)), 0, "lo is up");
    assert!(net.as_ref().unwrap().lo_up());
    let heard = drain(&p, member);
    assert!(heard.iter().any(|(h, b)| h.ty == RTM_NEWLINK && u32::from_le_bytes(b[8..12].try_into().unwrap()) & 1 == 1), "RTM_NEWLINK to the namespace's members");
    assert!(heard.iter().any(|(h, _)| h.ty == RTM_NEWADDR), "and 127.0.0.1");
    // ifi_flags 0 and ifi_change 0 change nothing (`do_setlink`).
    p.send(n, &req(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, 7, &ifinfo(1, 0, 0)), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    assert!(net.as_ref().unwrap().lo_up());
    p.send(n, &req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 8, &[0u8; 16]), 0);
    let links: Vec<_> = drain(&p, n).into_iter().filter(|(h, _)| h.ty == RTM_NEWLINK).collect();
    assert_eq!(u32::from_le_bytes(links[0].1[8..12].try_into().unwrap()), 0x10049);
    init();
    assert!(drain(&p, outside).iter().all(|(h, b)| h.ty != RTM_NEWLINK || i32::from_le_bytes(b[4..8].try_into().unwrap()) != 1), "the initial namespace heard nothing of it");
    for fd in [n, member, outside] {
        p.close(fd);
    }
}

#[test]
fn ports_and_loopback_per_namespace() {
    stack();
    let fs = ns();
    let p = Prog::new(&fs, 64, 1000);
    let (a, b) = (view(0xF000_0003), view(0xF000_0004));
    up(&a);
    up(&b);
    enter(&a, None);
    let la = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, la, &lo(24720)), 0);
    assert_eq!(p.call(sys::listen, [la, 4, 0, 0, 0, 0]), 0);
    let ua = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, ua, &lo(24721)), 0);
    enter(&b, None);
    let lb = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, lb, &lo(24720)), 0, "the same port in another namespace");
    assert_eq!(p.call(sys::listen, [lb, 4, 0, 0, 0, 0]), 0);
    let ub = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, ub, &lo(24721)), 0);
    let again = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, again, &any(24721)), -EADDRINUSE, "taken in this namespace");
    // The initial namespace has neither listener nor port taken.
    init();
    let i = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, i, &lo(24720)), -ECONNREFUSED);
    let iu = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, iu, &any(24721)), 0);
    // A connection in `a` reaches `a`'s listener only.
    enter(&a, None);
    let ca = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, ca, &lo(24720)), 0);
    let name = p.getname(ca, false).unwrap();
    assert!(u16::from_be_bytes([name[2], name[3]]) >= 40000, "an ephemeral port");
    assert_eq!(accept(&p, lb), -EAGAIN, "nothing for b");
    let sa = accept(&p, la);
    assert!(sa >= 0, "accept in a: {sa}");
    assert_eq!(p.send(ca, b"ping", 0), 4);
    assert_eq!(p.recv(sa as u64, 16, 0), (4, b"ping".to_vec()));
    let x = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(x, b"to-a", &lo(24721)), 4);
    assert_eq!(p.recv(ua, 16, MSG_DONTWAIT), (4, b"to-a".to_vec()));
    assert_eq!(p.recv(ub, 16, MSG_DONTWAIT).0, -EAGAIN, "b's receiver heard nothing");
    assert_eq!(p.recv(iu, 16, MSG_DONTWAIT).0, -EAGAIN, "nor the initial namespace's");
    // Nothing beyond the loopback.
    let far = encode_in(SockAddr { ip: Ipv4([10, 1, 2, 3]), port: 80 });
    let cf = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, cf, &far), -ENETUNREACH);
    assert_eq!(p.sendto(x, b"x", &far), -ENETUNREACH);
    assert_eq!(bind(&p, cf, &encode_in(SockAddr { ip: Ipv4([10, 1, 2, 3]), port: 0 })), -EADDRNOTAVAIL);
    // A namespace whose lo is down reaches nothing at all.
    let c = view(0xF000_0005);
    enter(&c, None);
    let cc = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, cc, &lo(24720)), -ENETUNREACH);
    let c6 = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c6, &sin6_lo(24720)), -EADDRNOTAVAIL, "no source address for ::1");
    assert_eq!(bind(&p, cc, &any(0)), 0, "0.0.0.0 can be bound");
    init();
    for fd in [la, ua, lb, ub, again, i, iu, ca, sa as u64, x, cf, cc, c6] {
        p.close(fd);
    }
    for n in [&a, &b, &c] {
        assert_eq!(Arc::strong_count(n.as_ref().unwrap()), 1, "closed sockets keep no view");
    }
}

#[test]
fn abstract_names_per_namespace() {
    let fs = ns();
    let p = Prog::new(&fs, 65, 1000);
    let (a, b) = (view(0xF000_0006), view(0xF000_0007));
    let name = sun(b"\0nw11-abstract");
    enter(&a, None);
    let sa = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, sa, &name), 0);
    assert_eq!(p.call(sys::listen, [sa, 4, 0, 0, 0, 0]), 0);
    let path = sun(b"/tmp/nw11.sock");
    let sp = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, sp, &path), 0);
    assert_eq!(p.call(sys::listen, [sp, 4, 0, 0, 0, 0]), 0);
    let auto = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, auto, &AF_UNIX.to_le_bytes()), 0, "autobind");
    let auto_name = p.getname(auto, false).unwrap();
    enter(&b, None);
    let cb = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, cb, &name), -ECONNREFUSED, "a's abstract name is not b's");
    let sb = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, sb, &name), 0, "free in b");
    let db = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(db, b"x", &auto_name), -ECONNREFUSED);
    let pb = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, pb, &path), 0, "path names are the file system's: every namespace reaches them");
    init();
    let ci = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, ci, &name), -ECONNREFUSED, "nor the initial namespace's");
    enter(&a, None);
    let ca = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, ca, &name), 0);
    let s = accept(&p, sa);
    assert!(s >= 0);
    init();
    for fd in [sa, sp, auto, cb, sb, db, pb, ci, ca, s as u64] {
        p.close(fd);
    }
    let _ = vfs::Ctx::kernel(&fs).unlink(None, b"/tmp/nw11.sock");
}

/// SCM_CREDENTIALS with `c` as the sender names it.
fn creds(c: Ucred) -> Vec<u8> {
    let mut v = (cmsg_len(12) as u64).to_le_bytes().to_vec();
    v.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    v.extend_from_slice(&SCM_CREDENTIALS.to_le_bytes());
    v.extend_from_slice(&c.bytes());
    v.resize(cmsg_space(12), 0);
    v
}

fn received(p: &Prog, fd: u64) -> Ucred {
    let (r, _, ctl, _) = p.recvmsg(fd, 16, cmsg_space(12), MSG_DONTWAIT);
    assert!(r > 0, "recvmsg {r}");
    let u = |o: usize| u32::from_le_bytes(ctl[o..o + 4].try_into().unwrap());
    Ucred { pid: u(16), uid: u(20), gid: u(24) }
}

fn peercred(p: &Prog, fd: u64) -> Ucred {
    let v = p.getopt(fd, SOL_SOCKET, SO_PEERCRED, 12).unwrap();
    let u = |o: usize| u32::from_le_bytes(v[o..o + 4].try_into().unwrap());
    Ucred { pid: u(0), uid: u(4), gid: u(8) }
}

#[test]
fn credentials_between_namespaces() {
    let fs = ns();
    // Kernel pid 5101, uid/gid 1000: pid 1 and root in its namespaces.
    let p = Prog::new(&fs, 5101, 1000);
    let inside = ns_root(5101, true);
    let other: Arc<dyn Ids> = Arc::new(FakeIds { pids: Vec::new(), uid: (2000, 0), gid: (2000, 0), caps: false, init_caps: false });
    enter(&None, Some(inside.clone()));
    let (a, b) = p.pair(SOCK_DGRAM);
    // Peer credentials are kept in kernel ids and read in the reader's namespaces.
    init();
    assert_eq!(peercred(&p, b), Ucred { pid: 5101, uid: 1000, gid: 1000 });
    enter(&None, Some(inside.clone()));
    assert_eq!(peercred(&p, b), Ucred { pid: 1, uid: 0, gid: 0 });
    enter(&None, Some(other.clone()));
    assert_eq!(peercred(&p, b), Ucred { pid: 0, uid: 65534, gid: 65534 }, "not visible, not mapped");
    // Sent explicitly as the sender names itself.
    enter(&None, Some(inside.clone()));
    assert_eq!(p.setopt_int(b, SOL_SOCKET, SO_PASSCRED, 1), 0);
    assert_eq!(p.sendmsg(a, b"1", &creds(Ucred { pid: 1, uid: 0, gid: 0 }), None, 0), 1);
    init();
    assert_eq!(received(&p, b), Ucred { pid: 5101, uid: 1000, gid: 1000 }, "the initial namespace reads kernel ids");
    enter(&None, Some(inside.clone()));
    assert_eq!(p.sendmsg(a, b"2", &creds(Ucred { pid: 1, uid: 0, gid: 0 }), None, 0), 1);
    assert_eq!(received(&p, b), Ucred { pid: 1, uid: 0, gid: 0 });
    assert_eq!(p.sendmsg(a, b"3", &creds(Ucred { pid: 1, uid: 5, gid: 0 }), None, 0), -EINVAL, "uid 5 is not mapped");
    assert_eq!(p.sendmsg(a, b"4", &creds(Ucred { pid: 2, uid: 0, gid: 0 }), None, 0), -ESRCH, "no pid 2 there");
    assert_eq!(p.sendmsg(a, b"5", &creds(Ucred { pid: 5101, uid: 0, gid: 0 }), None, 0), -ESRCH, "the kernel pid is no pid there");
    enter(&None, Some(ns_root(5101, false)));
    assert_eq!(p.sendmsg(a, b"6", &creds(Ucred { pid: 2, uid: 0, gid: 0 }), None, 0), -EPERM, "another pid without CAP_SYS_ADMIN");
    // Attached by the kernel (no explicit credentials): read where the receiver is.
    enter(&None, Some(inside.clone()));
    assert_eq!(p.send(a, b"7", 0), 1);
    enter(&None, Some(other));
    assert_eq!(received(&p, b), Ucred { pid: 0, uid: 65534, gid: 65534 });
    // A netlink socket's automatic port id is the pid its namespace gives the caller (`task_tgid_vnr`).
    let net = view(0xF000_0008);
    enter(&net, Some(inside));
    let n = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    assert_eq!(p.send(n, &req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, &[0u8; 16]), 0), 32);
    let nl = p.getname(n, false).unwrap();
    assert_eq!(u32::from_le_bytes(nl[4..8].try_into().unwrap()), 1);
    init();
    for fd in [a, b, n] {
        p.close(fd);
    }
}

// ---- NW-12: addresses, MTU and queue length of a namespace's lo; /proc/net -------------------------------

const RTM_DELADDR: u16 = 21;

/// An RTM_NEWADDR/RTM_DELADDR request: ifaddrmsg (AF_INET, `prefix`, scope, index 1) and attributes.
fn addr_req(ty: u16, flags: u16, seq: u32, prefix: u8, scope: u8, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    let mut fixed = [0u8; 8];
    fixed[0] = 2;
    fixed[1] = prefix;
    fixed[3] = scope;
    fixed[4..8].copy_from_slice(&1u32.to_le_bytes());
    let mut m = Msg::new(ty, flags, seq, 0, &fixed);
    for (t, d) in attrs {
        m.attr(*t, d);
    }
    m.set_flags(flags);
    m.done()
}

/// The (address, prefix, flags, scope, label) of every IPv4 address an AF_INET dump shows, and the IPv6
/// addresses of an AF_INET6 dump.
fn dump4(p: &Prog, fd: u64, family: u8) -> Vec<(Vec<u8>, u8, u8, u8, Vec<u8>)> {
    let mut ga = alloc::vec![family];
    ga.resize(8, 0);
    p.send(fd, &req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 40, &ga), 0);
    drain(p, fd)
        .into_iter()
        .filter(|(h, _)| h.ty == RTM_NEWADDR)
        .map(|(_, b)| {
            let a = attrs(&b[8..]);
            let get = |t: u16| a.iter().find(|(x, _)| *x == t).map(|(_, d)| d.to_vec()).unwrap_or_default();
            let label = get(3).into_iter().take_while(|c| *c != 0).collect();
            (get(1), b[1], b[2], b[3], label)
        })
        .collect()
}

fn poke(p: &Prog, at: u64, d: &[u8]) {
    use vfs::sys::UserMem;
    assert!(p.mem.write(at, d));
}

fn ip_bind(p: &Prog, ip: [u8; 4]) -> i64 {
    let fd = p.socket(AF_INET, SOCK_DGRAM) as u64;
    let r = bind(p, fd, &encode_in(SockAddr { ip: Ipv4(ip), port: 0 }));
    p.close(fd);
    r
}

#[test]
fn addresses_on_a_namespace_lo() {
    use crate::ifview::{IFF_UP, IFNAME_MAX};
    let fs = ns();
    let p = Prog::new(&fs, 64, 1000);
    let net = view(0xF000_0012);
    enter(&net, Some(ns_root(64, true)));
    let v = net.as_ref().unwrap();
    let n = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    let member = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    for g in [1, 5, 9] {
        assert_eq!(p.setopt_int(member, super::netlink::SOL_NETLINK, 1, g), 0);
    }
    // Before any address every IPv4 bind passes (no local table).
    assert_eq!(ip_bind(&p, [127, 0, 0, 1]), 0);
    // `ip addr add 192.0.2.1/24 dev lo` while lo is down: its own /32 is local, its subnet not yet.
    let local = [192, 0, 2, 1];
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | 0x400, 1, 24, 0, &[(2, &local), (1, &local)]), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    assert!(drain(&p, member).iter().any(|(h, _)| h.ty == RTM_NEWADDR), "the member hears the address");
    assert_eq!(dump4(&p, n, 2), alloc::vec![(local.to_vec(), 24, 0x80, 0, b"lo".to_vec())]);
    assert_eq!(ip_bind(&p, [192, 0, 2, 1]), 0);
    assert_eq!(ip_bind(&p, [192, 0, 2, 7]), -(EADDRNOTAVAIL as i64));
    assert_eq!(ip_bind(&p, [127, 0, 0, 1]), -(EADDRNOTAVAIL as i64), "the local table exists now");
    // The same again: EEXIST; NLM_F_REPLACE: accepted.
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | 0x400, 2, 24, 0, &[(2, &local)]), 0);
    assert_eq!(ack(&drain(&p, n)), -(EEXIST as i32));
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | 0x100, 3, 24, 0, &[(2, &local)]), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    // Errors in Linux's order.
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 4, 33, 0, &[(2, &local)]), 0);
    assert_eq!(ack(&drain(&p, n)), -(EINVAL as i32), "prefix > 32");
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 5, 8, 0, &[(1, &local)]), 0);
    assert_eq!(ack(&drain(&p, n)), -(EINVAL as i32), "no IFA_LOCAL");
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 6, 8, 0, &[(2, &[1, 2])]), 0);
    assert_eq!(ack(&drain(&p, n)), -(ERANGE as i32), "a short IFA_LOCAL");
    let long = [b'x'; IFNAME_MAX + 1];
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 7, 8, 0, &[(2, &local), (3, &long)]), 0);
    assert_eq!(ack(&drain(&p, n)), -(ERANGE as i32), "a label longer than IFNAMSIZ-1");
    // UP: 127.0.0.1/8 goes in front (a primary goes after the primaries whose scope is not narrower than its
    // own: host scope before universe), ::1 appears, the subnets become local.
    up(&net);
    let d = dump4(&p, n, 2);
    assert_eq!(d.iter().map(|x| (x.0.clone(), x.3)).collect::<Vec<_>>(), alloc::vec![(alloc::vec![127, 0, 0, 1], 254), (local.to_vec(), 0)]);
    let six = dump4(&p, n, 10);
    let mut one = alloc::vec![0u8; 16];
    one[15] = 1;
    assert_eq!(six, alloc::vec![(one.clone(), 128, 0x80, 254, Vec::new())]);
    let heard = drain(&p, member);
    assert!(heard.iter().any(|(h, b)| h.ty == RTM_NEWADDR && b[0] == 10), "::1 is announced");
    assert_eq!(ip_bind(&p, [192, 0, 2, 7]), 0);
    assert_eq!(ip_bind(&p, [127, 0, 0, 5]), 0);
    assert_eq!(ip_bind(&p, [10, 0, 0, 1]), -(EADDRNOTAVAIL as i64));
    // 127.0.0.2/8 is a secondary of 127.0.0.1/8; deleting the primary promotes it (promote_secondaries).
    let two = [127, 0, 0, 2];
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 8, 8, 254, &[(2, &two)]), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 9, 8, 0, &[(2, &[127, 0, 0, 3])]), 0);
    assert_eq!(ack(&drain(&p, n)), -(EINVAL as i32), "another scope in a primary's subnet");
    assert_eq!(dump4(&p, n, 2).last().map(|x| (x.0.clone(), x.2)), Some((two.to_vec(), 0x81)));
    drain(&p, member);
    p.send(n, &addr_req(RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, 10, 8, 0, &[(2, &[127, 0, 0, 1])]), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    let heard: Vec<(u16, Vec<u8>)> = drain(&p, member).into_iter().filter(|(h, _)| h.ty == RTM_DELADDR || h.ty == RTM_NEWADDR).map(|(h, b)| (h.ty, attrs(&b[8..])[0].1.to_vec())).collect();
    assert_eq!(heard, alloc::vec![(RTM_DELADDR, alloc::vec![127, 0, 0, 1]), (RTM_NEWADDR, two.to_vec())], "the primary goes, the secondary is promoted");
    assert_eq!(dump4(&p, n, 2).first().map(|x| (x.0.clone(), x.2)), Some((two.to_vec(), 0x80)), "in the primary's place");
    assert_eq!(ip_bind(&p, [127, 0, 0, 9]), 0, "the subnet's route stays with the promoted address");
    p.send(n, &addr_req(RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, 11, 8, 0, &[(2, &[127, 0, 0, 1])]), 0);
    assert_eq!(ack(&drain(&p, n)), -(EADDRNOTAVAIL as i32));
    // The ioctls: SIOCSIFADDR with an alias, SIOCSIFNETMASK, SIOCGIFADDR by label, SIOCSIFFLAGS on the alias.
    let s = p.socket(AF_INET, SOCK_DGRAM) as u64;
    let r = ifreq(&p, b"lo:1", 0);
    let mut sin = [0u8; 16];
    sin[0] = 2;
    sin[4..8].copy_from_slice(&[198, 51, 100, 9]);
    poke(&p, r + 16, &sin);
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFADDR, r), 0);
    let d = dump4(&p, n, 2);
    assert!(d.contains(&(alloc::vec![198, 51, 100, 9], 24, 0x80, 0, b"lo:1".to_vec())), "{d:?}");
    sin[4..8].copy_from_slice(&[255, 255, 255, 128]);
    poke(&p, r + 16, &sin);
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFNETMASK, r), 0);
    sin[4..8].copy_from_slice(&[255, 0, 255, 0]);
    poke(&p, r + 16, &sin);
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFNETMASK, r), -(EINVAL as i64), "a mask with a hole");
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCGIFNETMASK, r), 0);
    assert_eq!(p.get(r + 20, 4), [255, 255, 255, 128]);
    assert_eq!(ioctl(&p, s, SIOCSIFFLAGS, ifreq(&p, b"lo:1", 0)), 0, "down on the alias deletes its address");
    assert!(!dump4(&p, n, 2).iter().any(|x| x.4 == b"lo:1"));
    assert_eq!(ioctl(&p, s, SIOCSIFFLAGS, ifreq(&p, b"lo:1", 0)), -(EADDRNOTAVAIL as i64));
    // MTU and queue length: SIOCSIFMTU below 68 takes IPv4 away, ::1 goes below 1280.
    let mut m = ifreq(&p, b"lo", 0);
    poke(&p, m + 16, &7u32.to_le_bytes());
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFTXQLEN, m), -(EPERM as i64), "SIOCSIFTXQLEN wants the initial namespace's CAP_NET_ADMIN");
    assert_eq!(v.lo_dev().txqlen, 1000);
    poke(&p, m + 16, &(-1i32).to_le_bytes());
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFMTU, m), -(EINVAL as i64));
    m = ifreq(&p, b"lo", 0);
    poke(&p, m + 16, &60u32.to_le_bytes());
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFMTU, m), 0);
    assert!(dump4(&p, n, 2).is_empty() && dump4(&p, n, 10).is_empty());
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 12, 8, 254, &[(2, &two)]), 0);
    assert_eq!(ack(&drain(&p, n)), -(ENOBUFS as i32), "no IPv4 below 68");
    // RTM_NEWLINK with IFLA_MTU 1500 and IFLA_TXQLEN 5; down and up again brings 127.0.0.1/8 and ::1 back.
    let mut body = Msg::new(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, 13, 0, &ifinfo(1, 0, 0));
    body.u32(4, 1500).u32(13, 5);
    body.set_flags(NLM_F_REQUEST | NLM_F_ACK);
    p.send(n, &body.done(), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    let d = v.lo_dev();
    assert_eq!((d.mtu, d.txqlen, d.inet, d.addrs.len()), (1500, 5, true, 0));
    assert!(dump4(&p, n, 10).is_empty(), "a raised MTU brings ::1 back only at the next UP");
    v.change_lo_flags(0).unwrap();
    up(&net);
    assert_eq!(dump4(&p, n, 2).len(), 1);
    assert_eq!(dump4(&p, n, 10).len(), 1);
    // (A fresh ifreq: the dumps' buffers may have reused the old one's memory.)
    let m = ifreq(&p, b"lo", 0);
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCGIFTXQLEN, m), 0);
    assert_eq!(p.get(m + 16, 4), 5u32.to_le_bytes());
    // Without CAP_NET_ADMIN: EPERM; the initial namespace stays the kernel's (EPERM / EOPNOTSUPP).
    enter(&net, Some(ns_root(64, false)));
    let r = ifreq(&p, b"lo:1", 0);
    poke(&p, r + 16, &sin);
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFADDR, r), -(EPERM as i64));
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFMTU, m), -(EPERM as i64));
    let _ = IFF_UP;
    for fd in [s, n, member] {
        p.close(fd);
    }
    init();
    let s = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFMTU, m), -(EPERM as i64));
    p.close(s);
}

/// Set lo alias `name`'s address or netmask by ioctl (a fresh ifreq each call).
fn set_in(p: &Prog, s: u64, cmd: u64, name: &[u8], ip: [u8; 4]) -> i64 {
    let r = ifreq(p, name, 0);
    let mut sin = [0u8; 16];
    sin[0] = 2;
    sin[4..8].copy_from_slice(&ip);
    poke(p, r + 16, &sin);
    ioctl(p, s, cmd, r)
}

#[test]
fn odd_address_requests_on_a_namespace_lo() {
    use super::devioctl::{SIOCSIFADDR, SIOCSIFNETMASK};
    let fs = ns();
    let p = Prog::new(&fs, 66, 1000);
    let net = view(0xF000_0014);
    enter(&net, Some(ns_root(66, true)));
    up(&net);
    let n = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    let member = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    assert_eq!(p.setopt_int(member, super::netlink::SOL_NETLINK, 1, 5), 0);
    // Short and truncated requests: EINVAL, nothing changes, no panic.
    let full = addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 1, 24, 0, &[(2, &[192, 0, 2, 5])]);
    let mut short = full[..16 + 4].to_vec();
    let l = short.len() as u32;
    short[..4].copy_from_slice(&l.to_le_bytes());
    p.send(n, &short, 0);
    assert_eq!(ack(&drain(&p, n)), -(EINVAL as i32), "body shorter than ifaddrmsg");
    let mut cut = full[..full.len() - 2].to_vec();
    let l = cut.len() as u32;
    cut[..4].copy_from_slice(&l.to_le_bytes());
    p.send(n, &cut, 0);
    assert_eq!(ack(&drain(&p, n)), -(EINVAL as i32), "a cut IFA_LOCAL is no IFA_LOCAL (nla_parse stops there)");
    assert_eq!(dump4(&p, n, 2).len(), 1, "only 127.0.0.1/8");
    // Two IFA_LOCAL: the last one counts (`nla_parse`), as on Linux.
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 2, 24, 0, &[(2, &[192, 0, 2, 5]), (2, &[192, 0, 2, 6])]), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    let d = dump4(&p, n, 2);
    assert!(d.iter().any(|x| x.0 == [192, 0, 2, 6]) && !d.iter().any(|x| x.0 == [192, 0, 2, 5]), "{d:?}");
    // SIOCSIFADDR onto an existing address: Linux deletes the old one first and answers the insert's EEXIST;
    // the deletion stays and is announced.
    let s = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(set_in(&p, s, SIOCSIFADDR, b"lo:1", [198, 51, 100, 9]), 0);
    drain(&p, member);
    assert_eq!(set_in(&p, s, SIOCSIFADDR, b"lo:1", [127, 0, 0, 1]), -(EEXIST as i64));
    assert!(!dump4(&p, n, 2).iter().any(|x| x.4 == b"lo:1"), "the old address is gone");
    assert!(drain(&p, member).iter().any(|(h, _)| h.ty == RTM_DELADDR), "and its deletion announced");
    // SIOCSIFNETMASK whose changed address clashes (another scope in a primary's subnet): Linux ignores the
    // insert's error (answers 0) and the address is gone.
    p.send(n, &addr_req(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 3, 16, 254, &[(2, &[10, 0, 0, 2])]), 0);
    assert_eq!(ack(&drain(&p, n)), 0);
    assert_eq!(set_in(&p, s, SIOCSIFADDR, b"lo:3", [10, 0, 0, 1]), 0);
    assert!(dump4(&p, n, 2).iter().any(|x| x.4 == b"lo:3"));
    assert_eq!(set_in(&p, s, SIOCSIFNETMASK, b"lo:3", [255, 255, 0, 0]), 0);
    assert!(!dump4(&p, n, 2).iter().any(|x| x.4 == b"lo:3"), "deleted, not inserted again");
    // /proc/net/dhcp: only the initial namespace has the name.
    let in_ns = crate::procnet::dir_of(crate::procnet::Reader { net: net.clone(), ids: Some(ns_root(66, true)) });
    assert!(in_ns.lookup(b"dhcp").unwrap().is_none());
    assert!(in_ns.lookup(b"dev").unwrap().is_some());
    let initial = crate::procnet::dir_of(crate::procnet::Reader { net: None, ids: None });
    assert!(initial.lookup(b"dhcp").unwrap().is_some());
    // SIOCSIFTXQLEN: root of the initial user namespace in a new network namespace has `capable` (the hook says
    // so from its credentials), a namespace's root has not.
    let m = ifreq(&p, b"lo", 0);
    poke(&p, m + 16, &7u32.to_le_bytes());
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFTXQLEN, m), -(EPERM as i64));
    let root: Arc<dyn Ids> = Arc::new(FakeIds { pids: alloc::vec![(66, 66)], uid: (0, 0), gid: (0, 0), caps: true, init_caps: true });
    enter(&net, Some(root));
    assert_eq!(ioctl(&p, s, super::devioctl::SIOCSIFTXQLEN, m), 0);
    assert_eq!(net.as_ref().unwrap().lo_dev().txqlen, 7);
    for fd in [s, n, member] {
        p.close(fd);
    }
    init();
}

#[test]
fn proc_net_of_a_namespace() {
    use crate::procnet::{Kind, Reader, render};
    let fs = ns();
    let p = Prog::new(&fs, 65, 1000);
    let net = view(0xF000_0013);
    enter(&net, Some(ns_root(65, true)));
    let reader = Reader { net: net.clone(), ids: Some(ns_root(65, true)) };
    assert_eq!(render(Kind::Udp, &reader).lines().count(), 1, "the header only");
    assert_eq!(render(Kind::Unix, &reader), "Num       RefCount Protocol Flags    Type St Inode Path\n");
    up(&net);
    let u = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, u, &lo(5353)), 0);
    let t = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, t, &lo(8080)), 0);
    assert_eq!(render(Kind::Tcp, &reader).lines().count(), 1, "a bound TCP socket is in no table");
    assert_eq!(p.call(sys::listen, [t, 4, 0, 0, 0, 0]), 0);
    let tcp = render(Kind::Tcp, &reader);
    let l: Vec<&str> = tcp.lines().collect();
    assert_eq!(l.len(), 2);
    assert!(l.iter().all(|x| x.len() == 149));
    assert!(l[1].starts_with("   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0 "), "{}", l[1]);
    let udp = render(Kind::Udp, &reader);
    assert!(udp.lines().nth(1).unwrap().starts_with(" 5353: 0100007F:14E9 00000000:0000 07 "), "{udp}");
    let x = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let name = [&[1u8, 0, 0][..], b"nw12"].concat();
    let at = p.put(&name);
    assert_eq!(p.call(sys::bind, [x, at, name.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::listen, [x, 1, 0, 0, 0, 0]), 0);
    let unix = render(Kind::Unix, &reader);
    assert!(unix.ends_with(": 00000002 00000000 00010000 0001 01     0 @nw12\n"), "{unix}");
    // The initial namespace sees none of them.
    let init_r = Reader { net: None, ids: None };
    assert!(!render(Kind::Unix, &init_r).contains("@nw12"));
    assert!(!render(Kind::Tcp, &init_r).contains(":1F90"));
    let dev = render(Kind::Dev, &reader);
    assert_eq!(dev.lines().count(), 3);
    assert!(dev.lines().nth(2).unwrap().starts_with("    lo:"));
    for fd in [u, t, x] {
        p.close(fd);
    }
    init();
}
