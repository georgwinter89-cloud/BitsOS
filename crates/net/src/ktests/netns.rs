//! NW-11 in the kernel: network namespace views beside the initial one, without processes (the views are made
//! here as `proc::ns::misc::NetNs` makes them for CLONE_NEWNET). Safe and rerunnable: every socket is released
//! and every view dropped at the end; each test checks that nothing of its namespaces reached the initial one.

use super::{SEC, lo_at, st};
use crate::NetError;
use crate::netns::{Net, NetView};
use crate::sock::addr::encode_in;
use crate::sock::unix::{UType, UnixSock};
use crate::sock::abi::{cmsg_len, cmsg_space};
use crate::sock::{Caller, Ids, Kind, SliceSource, SockFile, Ucred, cmsg, inet::InetSock};
use crate::{Ipv4, KSocket, SockAddr};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::{EADDRINUSE, EADDRNOTAVAIL, EAGAIN, ECONNREFUSED, EINVAL, ENETUNREACH};

/// Ports of these tests (lane range 24500-24999; the loopback tests use 24990 up).
const NS_PORT: u16 = 24960;
const SOCK_STREAM: u32 = 1;
const SOCK_DGRAM: u32 = 2;

fn caller(net: &Net, ids: Option<Arc<dyn Ids>>) -> Caller {
    Caller { net: net.clone(), ids, ..Caller::new(900_011, 1000, 1000, policy::Subject::KERNEL) }
}

fn view(inum: u32, up: bool) -> Result<Net, String> {
    let v = NetView::new(inum);
    if up {
        v.change_lo_flags(crate::ifview::IFF_UP).map_err(|e| format!("lo up: {e}"))?;
    }
    Ok(Some(v))
}

/// An AF_INET socket of `c`'s namespace, released when the file goes.
fn inet(ty: u32, c: &Caller) -> Result<(Arc<SockFile>, Arc<InetSock>), String> {
    let s = InetSock::create(ty, 0, c).map_err(|e| format!("socket: {e}"))?;
    Ok((SockFile::new(Kind::Inet(s.clone())).map_err(|e| format!("{e}"))?, s))
}

fn lo(port: u16) -> Vec<u8> {
    encode_in(SockAddr { ip: Ipv4::LOCALHOST, port })
}

fn recv_now(s: &InetSock) -> Result<Vec<u8>, i64> {
    let mut got = Vec::new();
    s.recv(64, 0, true, &mut |b| {
        got.extend_from_slice(b);
        Ok(())
    })?;
    Ok(got)
}

fn only_view(n: &Net) -> bool {
    n.as_ref().is_some_and(|v| Arc::strong_count(v) == 1)
}

/// Two namespaces listen on the same 127.0.0.1 port; a connection in each reaches its own listener; the
/// initial namespace has none on that port.
fn same_port() -> TestResult {
    let s = st()?;
    let (a, b) = (view(0xF100_0001, true)?, view(0xF100_0002, true)?);
    let (ca, cb) = (caller(&a, None), caller(&b, None));
    let (_fa, la) = inet(SOCK_STREAM, &ca)?;
    la.bind(&lo(NS_PORT), &ca).map_err(|e| format!("bind in a: {e}"))?;
    la.listen(4, &ca).map_err(|e| format!("listen in a: {e}"))?;
    let (_fb, lb) = inet(SOCK_STREAM, &cb)?;
    kassert!(lb.bind(&lo(NS_PORT), &cb).is_ok(), "the same port in namespace b");
    lb.listen(4, &cb).map_err(|e| format!("listen in b: {e}"))?;
    kassert!(s.tcp_connect(lo_at(NS_PORT), Some(SEC)).err() == Some(NetError::ConnectionRefused), "no namespace listener in the initial namespace");
    for (c, l, other, what) in [(&ca, &la, &lb, "a"), (&cb, &lb, &la, "b")] {
        let (_fc, cl) = inet(SOCK_STREAM, c)?;
        cl.connect(&lo(NS_PORT), c, false).map_err(|e| format!("connect in {what}: {e}"))?;
        kassert!(other.accept(true, c).err() == Some(EAGAIN), "{what}'s connection went to the other namespace");
        let (acc, _) = l.accept(true, c).map_err(|e| format!("accept in {what}: {e}"))?;
        let _fac = SockFile::new(Kind::Inet(acc.clone())).map_err(|e| format!("{e}"))?;
        cl.send(&mut SliceSource { data: b"nw11", at: 0 }, 4, 0, false, None).map_err(|e| format!("send: {e}"))?;
        kassert!(recv_now(&acc) == Ok(b"nw11".to_vec()), "data in {what}");
    }
    drop((_fa, _fb, la, lb, ca, cb));
    kassert!(only_view(&a) && only_view(&b), "released sockets keep no view");
    Ok(())
}
ktest!("nw.ns.same_port", Safe, same_port);

/// UDP and TCP of a namespace stay on its loopback: an initial-namespace receiver on the same port hears
/// nothing, a card address is unreachable, and a namespace whose lo is DOWN reaches nothing.
fn no_leak() -> TestResult {
    let _ = st()?;
    let a = view(0xF100_0003, true)?;
    let down = view(0xF100_0004, false)?;
    let (ca, cd, ci) = (caller(&a, None), caller(&down, None), caller(&None, None));
    let (_fi, init_rx) = inet(SOCK_DGRAM, &ci)?;
    init_rx.bind(&lo(NS_PORT + 1), &ci).map_err(|e| format!("bind in the initial namespace: {e}"))?;
    let (_fr, ns_rx) = inet(SOCK_DGRAM, &ca)?;
    ns_rx.bind(&lo(NS_PORT + 1), &ca).map_err(|e| format!("bind in a: {e}"))?;
    let (_ft, tx) = inet(SOCK_DGRAM, &ca)?;
    let to = lo(NS_PORT + 1);
    tx.send(&mut SliceSource { data: b"in-a", at: 0 }, 4, 0, true, Some(&to)).map_err(|e| format!("sendto: {e}"))?;
    kassert!(recv_now(&ns_rx) == Ok(b"in-a".to_vec()), "a's receiver");
    kassert!(recv_now(&init_rx) == Err(EAGAIN), "nothing reached the initial namespace");
    let (_fit, itx) = inet(SOCK_DGRAM, &ci)?;
    itx.send(&mut SliceSource { data: b"init", at: 0 }, 4, 0, true, Some(&to)).map_err(|e| format!("sendto: {e}"))?;
    kassert!(recv_now(&init_rx) == Ok(b"init".to_vec()) && recv_now(&ns_rx) == Err(EAGAIN), "the initial namespace's datagram stays there");
    let card = encode_in(SockAddr { ip: Ipv4([10, 0, 2, 2]), port: 24510 });
    kassert!(tx.send(&mut SliceSource { data: b"x", at: 0 }, 1, 0, true, Some(&card)).err() == Some(ENETUNREACH), "no card in a namespace");
    let (_fc, tc) = inet(SOCK_STREAM, &ca)?;
    kassert!(tc.connect(&card, &ca, false).err() == Some(ENETUNREACH), "TCP beyond the loopback");
    kassert!(crate::ifview::links_in(&a).len() == 1 && crate::ifview::links_in(&None).len() == 1 + st()?.interfaces().len(), "interface views");
    let (_fd, td) = inet(SOCK_STREAM, &cd)?;
    kassert!(td.connect(&lo(NS_PORT), &cd, false).err() == Some(ENETUNREACH), "connect while lo is DOWN");
    let (_fd2, td2) = inet(SOCK_STREAM, &ca)?;
    kassert!(td2.bind(&card, &ca).err() == Some(EADDRNOTAVAIL), "a card address is none of a namespace's");
    let l = crate::ifview::links_in(&down).remove(0);
    kassert!(l.flags() == 0x8 && l.addrs.is_empty(), "a new namespace's lo: flags {:#x}", l.flags());
    drop((_fi, _fr, _ft, _fit, _fc, _fd, _fd2, init_rx, ns_rx, tx, itx, tc, td, td2, ca, cd));
    kassert!(only_view(&a) && only_view(&down), "released sockets keep no view");
    Ok(())
}
ktest!("nw.ns.no_leak", Safe, no_leak);

/// An abstract AF_UNIX name bound in one namespace is not found from another one or the initial one.
fn abstract_hidden() -> TestResult {
    let (a, b) = (view(0xF100_0005, false)?, view(0xF100_0006, false)?);
    let (ca, cb, ci) = (caller(&a, None), caller(&b, None), caller(&None, None));
    let srv = UnixSock::new_in(UType::Stream, a.clone());
    let _fs = SockFile::new(Kind::Unix(srv.clone())).map_err(|e| format!("{e}"))?;
    srv.autobind().map_err(|e| format!("autobind: {e}"))?;
    srv.listen(4, &ca).map_err(|e| format!("listen: {e}"))?;
    let name = srv.getname(false).map_err(|e| format!("getname: {e}"))?;
    for (c, what) in [(&cb, "b"), (&ci, "the initial namespace")] {
        let u = UnixSock::new_in(UType::Stream, c.net.clone());
        let _f = SockFile::new(Kind::Unix(u.clone())).map_err(|e| format!("{e}"))?;
        kassert!(u.connect(&name, None, c, true).err() == Some(ECONNREFUSED), "a's name seen from {what}");
    }
    let u = UnixSock::new_in(UType::Stream, a.clone());
    let _fu = SockFile::new(Kind::Unix(u.clone())).map_err(|e| format!("{e}"))?;
    u.connect(&name, None, &ca, true).map_err(|e| format!("connect in a: {e}"))?;
    let acc = srv.accept(true).map_err(|e| format!("accept: {e}"))?;
    let _fa = SockFile::new(Kind::Unix(acc)).map_err(|e| format!("{e}"))?;
    drop((_fs, _fu, _fa, srv, u, ca, cb));
    kassert!(only_view(&a) && only_view(&b), "released sockets keep no view");
    Ok(())
}
ktest!("nw.ns.abstract_hidden", Safe, abstract_hidden);

/// A pid and user namespace: kernel pid 900011 is 1 there, kernel uid/gid 1000 is 0.
struct Inside(bool);

impl Ids for Inside {
    fn capable(&self, _cap: u32) -> bool {
        self.0
    }
    fn pid_to_kernel(&self, pid: u32) -> Option<u32> {
        (pid == 1).then_some(900_011)
    }
    fn pid_from_kernel(&self, kpid: u32) -> u32 {
        if kpid == 900_011 { 1 } else { 0 }
    }
    fn uid_to_kernel(&self, uid: u32) -> Option<u32> {
        (uid == 0).then_some(1000)
    }
    fn gid_to_kernel(&self, gid: u32) -> Option<u32> {
        (gid == 0).then_some(1000)
    }
    fn uid_from_kernel(&self, kuid: u32) -> u32 {
        if kuid == 1000 { 0 } else { 65534 }
    }
    fn gid_from_kernel(&self, kgid: u32) -> u32 {
        if kgid == 1000 { 0 } else { 65534 }
    }
}

/// SO_PEERCRED and SCM_CREDENTIALS are kept in kernel ids and read in the reader's namespaces; sent ones are
/// named in the sender's.
fn credentials() -> TestResult {
    let ids: Arc<dyn Ids> = Arc::new(Inside(true));
    let sender = caller(&None, Some(ids.clone()));
    let (x, y) = UnixSock::pair(UType::Dgram, &sender);
    let _f = (SockFile::new(Kind::Unix(x)).map_err(|e| format!("{e}"))?, SockFile::new(Kind::Unix(y.clone())).map_err(|e| format!("{e}"))?);
    let k = y.peercred().ok_or("no peer credentials")?;
    kassert!(k == Ucred { pid: 900_011, uid: 1000, gid: 1000 }, "kept in kernel ids: {k:?}");
    kassert!(sender.view_of(k) == Ucred { pid: 1, uid: 0, gid: 0 }, "read inside");
    kassert!(caller(&None, None).view_of(k) == k, "read in the initial namespaces");
    let outsider: Arc<dyn Ids> = Arc::new(Inside(false));
    let stranger = Caller { pid: 900_012, ..caller(&None, Some(outsider)) };
    kassert!(stranger.view_of(Ucred { pid: 4242, uid: 5, gid: 5 }) == Ucred { pid: 0, uid: 65534, gid: 65534 }, "not visible, not mapped");
    let ctl = |c: Ucred| {
        let mut v = (cmsg_len(12) as u64).to_le_bytes().to_vec();
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&c.bytes());
        v.resize(cmsg_space(12), 0);
        v
    };
    let sent = cmsg::parse_unix(&ctl(Ucred { pid: 1, uid: 0, gid: 0 }), &sender, |_| None).map_err(|e| format!("own credentials: {e}"))?;
    kassert!(sent.creds == Some(k), "sent as named inside, kept as kernel ids: {:?}", sent.creds);
    kassert!(cmsg::parse_unix(&ctl(Ucred { pid: 1, uid: 7, gid: 0 }), &sender, |_| None).err() == Some(EINVAL), "an unmapped uid");
    Ok(())
}
ktest!("nw.ns.credentials", Safe, credentials);

/// rtnetlink and the interface ioctls of a new namespace: only lo, DOWN and without address; SIOCSIFFLAGS
/// sets it UP (CAP_NET_ADMIN over the namespace), the namespace's group members hear RTM_NEWLINK and
/// RTM_NEWADDR, and the next dumps show lo UP with 127.0.0.1/8.
fn rtnl() -> TestResult {
    use crate::sock::devioctl::{self, Asker};
    use crate::sock::netlink::{NETLINK_ADD_MEMBERSHIP, NetlinkSock};
    use crate::sock::nlmsg::{Hdr, NLMSG_HDRLEN, align};
    use crate::sock::rtnl::{RTM_GETADDR, RTM_GETLINK, RTM_NEWADDR, RTM_NEWLINK, RTNLGRP_LINK};
    use super::netlink::{Mem, attr, dump_as};
    let b = view(0xF100_0007, false)?;
    let cb = caller(&b, Some(Arc::new(Inside(true))));
    let n = NetlinkSock::create(3, 0, &cb).map_err(|e| format!("netlink: {e}"))?;
    let flags = |l: &[u8]| u32::from_le_bytes(l.get(8..12).and_then(|x| x.try_into().ok()).unwrap_or([0; 4]));
    let links = dump_as(&n, RTM_GETLINK, 0, RTM_NEWLINK, &cb)?;
    kassert!(links.len() == 1 && attr(&links[0], 16, 3) == Some(b"lo\0"), "{} links, lo only", links.len());
    kassert!(flags(&links[0]) == 0x8, "lo DOWN: flags {:#x}", flags(&links[0]));
    kassert!(dump_as(&n, RTM_GETADDR, 2, RTM_NEWADDR, &cb)?.is_empty(), "no address before the first UP");
    n.setsockopt(NETLINK_ADD_MEMBERSHIP, RTNLGRP_LINK, &cb).map_err(|e| format!("join: {e}"))?;
    let mut d = Vec::new();
    while n.recv(65536, 0, true, &mut |x| {
        d.extend_from_slice(x);
        Ok(())
    }).is_ok() {}
    let mem = Mem(ksync::SpinLock::new(alloc::vec![0; 4096]));
    let (_fu, _) = inet(SOCK_DGRAM, &cb)?;
    let mut r = [0u8; 40];
    r[..2].copy_from_slice(b"lo");
    r[16..18].copy_from_slice(&0x9u16.to_le_bytes());
    vfs::sys::UserMem::write(&mem, 0x1000, &r);
    let outsider = Asker { mem: &mem, caller: caller(&b, Some(Arc::new(Inside(false)))), net: b.clone() };
    kassert!(devioctl::ioctl(&outsider, &_fu, devioctl::SIOCSIFFLAGS, 0x1000) == Some(-uapi::errno::EPERM), "SIOCSIFFLAGS without CAP_NET_ADMIN");
    let a = Asker { mem: &mem, caller: cb.clone(), net: b.clone() };
    kassert!(devioctl::ioctl(&a, &_fu, devioctl::SIOCSIFFLAGS, 0x1000) == Some(0), "SIOCSIFFLAGS lo UP");
    let mut heard = Vec::new();
    for _ in 0..8 {
        let mut d = Vec::new();
        if n.recv(65536, 0, true, &mut |x| {
            d.extend_from_slice(x);
            Ok(())
        }).is_err() {
            break;
        }
        let mut rest = &d[..];
        while let Some(h) = Hdr::parse(rest) {
            let len = (h.len as usize).clamp(NLMSG_HDRLEN, rest.len());
            heard.push((h.ty, rest[NLMSG_HDRLEN..len].to_vec()));
            rest = &rest[align(len).min(rest.len())..];
        }
    }
    kassert!(heard.iter().any(|(t, l)| *t == RTM_NEWLINK && flags(l) & 1 != 0), "RTM_NEWLINK with lo UP to the member");
    let links = dump_as(&n, RTM_GETLINK, 0, RTM_NEWLINK, &cb)?;
    kassert!(links.len() == 1 && flags(&links[0]) & 1 != 0, "lo UP in the dump: {:#x}", links.first().map_or(0, |l| flags(l)));
    let addrs = dump_as(&n, RTM_GETADDR, 2, RTM_NEWADDR, &cb)?;
    kassert!(addrs.len() == 1 && addrs[0][1] == 8 && attr(&addrs[0], 8, 2) == Some(&[127, 0, 0, 1][..]), "127.0.0.1/8 after UP ({} addresses)", addrs.len());
    let init = crate::ifview::links_in(&None);
    kassert!(init.first().is_some_and(|l| l.flags() & 1 != 0), "the initial namespace's lo is untouched");
    n.release();
    drop((_fu, a, outsider, cb, n));
    kassert!(only_view(&b), "released sockets keep no view");
    Ok(())
}
ktest!("nw.ns.rtnl", Safe, rtnl);

/// `[::1]:port`.
fn lo6(port: u16) -> Vec<u8> {
    let mut ip = [0u8; 16];
    ip[15] = 1;
    let mut v = 10u16.to_le_bytes().to_vec();
    v.extend_from_slice(&port.to_be_bytes());
    v.extend_from_slice(&[0; 4]);
    v.extend_from_slice(&ip);
    v.extend_from_slice(&[0; 4]);
    v
}

/// UDP over IPv6: two namespaces bind the same [::1] port, each one's datagram reaches only its own receiver;
/// a second bind in one namespace is EADDRINUSE as usual.
fn udp6() -> TestResult {
    let (a, b) = (view(0xF100_0008, true)?, view(0xF100_0009, true)?);
    let (ca, cb) = (caller(&a, None), caller(&b, None));
    let six = |c: &Caller| -> Result<(Arc<SockFile>, Arc<InetSock>), String> {
        let s = InetSock::create6(SOCK_DGRAM, 0, c).map_err(|e| format!("socket6: {e}"))?;
        Ok((SockFile::new(Kind::Inet(s.clone())).map_err(|e| format!("{e}"))?, s))
    };
    let to = lo6(NS_PORT + 2);
    let (_fa, ra) = six(&ca)?;
    ra.bind(&to, &ca).map_err(|e| format!("bind in a: {e}"))?;
    let (_fb, rb) = six(&cb)?;
    kassert!(rb.bind(&to, &cb).is_ok(), "the same [::1] port in namespace b");
    let (_fx, dup) = six(&ca)?;
    kassert!(dup.bind(&to, &ca).err() == Some(EADDRINUSE), "the port is taken in a");
    for (c, rx, other, what) in [(&ca, &ra, &rb, "a"), (&cb, &rb, &ra, "b")] {
        let (_ft, tx) = six(c)?;
        tx.send(&mut SliceSource { data: b"six", at: 0 }, 3, 0, true, Some(&to)).map_err(|e| format!("sendto in {what}: {e}"))?;
        kassert!(recv_now(rx) == Ok(b"six".to_vec()), "{what}'s receiver");
        kassert!(recv_now(other) == Err(EAGAIN), "{what}'s datagram stayed in {what}");
    }
    drop((_fa, _fb, _fx, ra, rb, dup, ca, cb));
    kassert!(only_view(&a) && only_view(&b), "released sockets keep no view");
    Ok(())
}
ktest!("nw.ns.udp6", Safe, udp6);

/// One rtnetlink request of an admin of namespace `net` (the answer: 0 or -errno).
fn rtnl_req(net: &Net, ty: u16, flags: u16, fixed: &[u8], attrs: &[(u16, &[u8])]) -> i64 {
    use crate::sock::nlmsg::{Msg, NLM_F_REQUEST};
    use crate::sock::rtnl::{ReqCtx, rcv_msg};
    let mut m = Msg::new(ty, NLM_F_REQUEST | flags, 1, 0, fixed);
    for (t, d) in attrs {
        m.attr(*t, d);
    }
    m.set_flags(NLM_F_REQUEST | flags);
    let ctx = ReqCtx { portid: 0, admin: true, strict: false, net: net.clone() };
    match rcv_msg(&m.done(), &ctx) {
        Ok(_) => 0,
        Err(e) => -e.errno,
    }
}

fn ifaddr(prefix: u8, scope: u8) -> [u8; 8] {
    let mut f = [0u8; 8];
    f[0] = 2;
    f[1] = prefix;
    f[3] = scope;
    f[4..8].copy_from_slice(&1u32.to_le_bytes());
    f
}

fn at(ip: [u8; 4], port: u16) -> Vec<u8> {
    encode_in(SockAddr { ip: Ipv4(ip), port })
}

/// NW-12: addresses on a namespace's lo. RTM_NEWADDR 192.0.2.1/24 while lo is down makes only that address
/// local; UP adds 127.0.0.1/8 and ::1 and makes the subnet local; UDP and TCP reach lo's other addresses inside
/// the namespace; RTM_DELADDR, SIOCSIFMTU below 68 (IPv4 gone, ENOBUFS); nothing reaches the initial lo.
fn addr() -> TestResult {
    use crate::sock::devioctl::{self, Asker};
    use crate::sock::netlink::NetlinkSock;
    use crate::sock::rtnl::{RTM_DELADDR, RTM_GETADDR, RTM_NEWADDR};
    use super::netlink::{Mem, attr, dump_as};
    use uapi::errno::{EEXIST, ENOBUFS};
    let b = view(0xF100_0012, false)?;
    let cb = caller(&b, Some(Arc::new(Inside(true))));
    let n = NetlinkSock::create(3, 0, &cb).map_err(|e| format!("netlink: {e}"))?;
    let local = [192, 0, 2, 1];
    kassert!(rtnl_req(&b, RTM_NEWADDR, 0x400, &ifaddr(24, 0), &[(2, &local)]) == 0, "RTM_NEWADDR 192.0.2.1/24");
    kassert!(rtnl_req(&b, RTM_NEWADDR, 0x400, &ifaddr(24, 0), &[(2, &local)]) == -EEXIST, "the same address again");
    let bind_ok = |ip: [u8; 4]| -> Result<bool, String> {
        let (_f, s) = inet(SOCK_DGRAM, &cb)?;
        Ok(s.bind(&at(ip, 0), &cb).is_ok())
    };
    kassert!(bind_ok(local)? && !bind_ok([192, 0, 2, 7])? && !bind_ok([127, 0, 0, 1])?, "down: only 192.0.2.1 itself is local");
    b.as_ref().unwrap().change_lo_flags(crate::ifview::IFF_UP).map_err(|e| format!("up: {e}"))?;
    let v4 = dump_as(&n, RTM_GETADDR, 2, RTM_NEWADDR, &cb)?;
    let ips: Vec<Option<&[u8]>> = v4.iter().map(|m| attr(m, 8, 2)).collect();
    kassert!(ips == [Some(&[127, 0, 0, 1][..]), Some(&local[..])], "127.0.0.1/8 before 192.0.2.1/24: {ips:?}");
    let v6 = dump_as(&n, RTM_GETADDR, 10, RTM_NEWADDR, &cb)?;
    kassert!(v6.len() == 1 && v6[0][1] == 128 && attr(&v6[0], 8, 1).is_some_and(|a| a[15] == 1), "::1/128 after UP ({} addresses)", v6.len());
    kassert!(bind_ok([192, 0, 2, 7])? && bind_ok([127, 0, 0, 5])? && !bind_ok([10, 0, 0, 1])?, "up: both subnets are local");
    // UDP to another address of the subnet reaches a receiver on 0.0.0.0 from 192.0.2.1.
    let (_fr, rx) = inet(SOCK_DGRAM, &cb)?;
    rx.bind(&at([0; 4], NS_PORT + 4), &cb).map_err(|e| format!("bind rx: {e}"))?;
    let (_ft, tx) = inet(SOCK_DGRAM, &cb)?;
    tx.send(&mut SliceSource { data: b"lo2", at: 0 }, 3, 0, true, Some(&at([192, 0, 2, 7], NS_PORT + 4))).map_err(|e| format!("sendto 192.0.2.7: {e}"))?;
    kassert!(recv_now(&rx) == Ok(b"lo2".to_vec()), "the datagram to 192.0.2.7");
    // TCP to 192.0.2.1.
    let (_fl, l) = inet(SOCK_STREAM, &cb)?;
    l.bind(&at([0; 4], NS_PORT + 5), &cb).map_err(|e| format!("bind listener: {e}"))?;
    l.listen(4, &cb).map_err(|e| format!("listen: {e}"))?;
    let (_fc, c) = inet(SOCK_STREAM, &cb)?;
    c.connect(&at(local, NS_PORT + 5), &cb, false).map_err(|e| format!("connect 192.0.2.1: {e}"))?;
    kassert!(l.accept(true, &cb).is_ok(), "accepted from 192.0.2.1");
    // RTM_DELADDR; a second one finds nothing.
    kassert!(rtnl_req(&b, RTM_DELADDR, 0, &ifaddr(24, 0), &[(2, &local)]) == 0, "RTM_DELADDR");
    kassert!(rtnl_req(&b, RTM_DELADDR, 0, &ifaddr(24, 0), &[(2, &local)]) == -EADDRNOTAVAIL, "deleted already");
    // SIOCSIFMTU 60: no IPv4 any more.
    let mem = Mem(ksync::SpinLock::new(alloc::vec![0; 4096]));
    let mut r = [0u8; 40];
    r[..2].copy_from_slice(b"lo");
    r[16..20].copy_from_slice(&60u32.to_le_bytes());
    vfs::sys::UserMem::write(&mem, 0x1000, &r);
    let a = Asker { mem: &mem, caller: cb.clone(), net: b.clone() };
    kassert!(devioctl::ioctl(&a, &_fr, devioctl::SIOCSIFMTU, 0x1000) == Some(0), "SIOCSIFMTU 60");
    kassert!(dump_as(&n, RTM_GETADDR, 0, RTM_NEWADDR, &cb)?.is_empty(), "no address below MTU 68 and 1280");
    kassert!(rtnl_req(&b, RTM_NEWADDR, 0, &ifaddr(8, 254), &[(2, &[127, 0, 0, 1])]) == -ENOBUFS, "no IPv4 below 68");
    let init = crate::ifview::links_in(&None);
    kassert!(init.first().is_some_and(|l| l.addrs.len() == 1 && l.mtu == crate::ifview::LOOPBACK_MTU), "the initial lo is untouched");
    n.release();
    drop((_fr, rx, _ft, tx, _fl, l, _fc, c, a, cb, n));
    kassert!(only_view(&b), "released sockets keep no view");
    Ok(())
}
ktest!("nw.ns.addr", Safe, addr);

/// NW-12: /proc/net of a namespace shows its own sockets and lo only, /proc/net/dhcp only in the initial one.
fn procnet() -> TestResult {
    use crate::procnet::{Kind as P, Reader, names, render};
    let b = view(0xF100_0013, true)?;
    let cb = caller(&b, Some(Arc::new(Inside(true))));
    let r = Reader { net: b.clone(), ids: Some(Arc::new(Inside(true))) };
    kassert!(!names(&r).iter().any(|(n, _)| *n == "dhcp") && names(&Reader { net: None, ids: None }).iter().any(|(n, _)| *n == "dhcp"), "dhcp only in the initial namespace");
    let (_fu, u) = inet(SOCK_DGRAM, &cb)?;
    u.bind(&lo(NS_PORT + 6), &cb).map_err(|e| format!("bind: {e}"))?;
    let (_fl, l) = inet(SOCK_STREAM, &cb)?;
    l.bind(&lo(NS_PORT + 7), &cb).map_err(|e| format!("bind: {e}"))?;
    l.listen(1, &cb).map_err(|e| format!("listen: {e}"))?;
    let want_udp = format!("{:5}: 0100007F:{:04X} 00000000:0000 07 ", NS_PORT + 6, NS_PORT + 6);
    let udp = render(P::Udp, &r);
    kassert!(udp.lines().count() == 2 && udp.lines().nth(1).is_some_and(|x| x.starts_with(&want_udp)), "udp: {udp}");
    let tcp = render(P::Tcp, &r);
    let want_tcp = format!("   0: 0100007F:{:04X} 00000000:0000 0A ", NS_PORT + 7);
    kassert!(tcp.lines().count() == 2 && tcp.lines().nth(1).is_some_and(|x| x.starts_with(&want_tcp) && x.len() == 149), "tcp: {tcp}");
    let un = UnixSock::new_in(UType::Stream, b.clone());
    let _fx = SockFile::new(Kind::Unix(un.clone())).map_err(|e| format!("{e}"))?;
    un.autobind().map_err(|e| format!("autobind: {e}"))?;
    un.listen(1, &cb).map_err(|e| format!("listen unix: {e}"))?;
    let name = un.getname(false).map_err(|e| format!("getname: {e}"))?;
    let path = crate::procnet::unix_path(&name);
    let unix = render(P::Unix, &r);
    let want = format!(": 00000002 00000000 00010000 0001 01 {:5}{path}\n", _fx.ino());
    kassert!(unix.lines().count() == 2 && path.starts_with(" @") && unix.ends_with(&want), "unix: {unix}");
    let dev = render(P::Dev, &r);
    kassert!(dev.lines().count() == 3 && dev.lines().nth(2).is_some_and(|x| x.starts_with("    lo:")), "dev: {dev}");
    kassert!(render(P::Route, &r).lines().count() == 1, "route: lo's routes are local, the main table is empty");
    let init = Reader { net: None, ids: None };
    kassert!(!render(P::Unix, &init).contains(&path) && !render(P::Udp, &init).contains(&format!(":{:04X} ", NS_PORT + 6)), "the initial namespace sees none of them");
    drop((_fu, u, _fl, l, _fx, un, cb, r));
    kassert!(only_view(&b), "released sockets keep no view");
    Ok(())
}
ktest!("nw.ns.procnet", Safe, procnet);
