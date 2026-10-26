//! NW-6 in the kernel: an rtnetlink socket dumps the kernel stack's interfaces (lo first, then every card under
//! its driver name with index 2, 3, ...) and addresses (127.0.0.1/8 and the cards' leases), and the interface
//! ioctls answer from the same view. Without processes: the socket and a small fake program memory are made
//! here (the ABI programs compare the full behaviour with Linux). Safe and rerunnable: the sockets are released.

use super::{configured, st};
use crate::sock::devioctl::{self, Asker};
use crate::sock::netlink::NetlinkSock;
use crate::sock::nlmsg::*;
use crate::sock::rtnl::{RTM_GETADDR, RTM_GETLINK, RTM_NEWADDR, RTM_NEWLINK};
use uapi::errno::{EADDRNOTAVAIL, EAGAIN, EINVAL, ERANGE};
use crate::sock::{Caller, Kind, SliceSource, SockFile, inet::InetSock};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use ksync::SpinLock;

fn caller() -> Caller {
    Caller { pid: 900_006, uid: 1000, gid: 1000, subject: policy::Subject::KERNEL }
}

/// Sends one request and reads the dump to its DONE: the messages of type `want` (payloads).
fn dump(n: &Arc<NetlinkSock>, ty: u16, family: u8, want: u16) -> Result<Vec<Vec<u8>>, String> {
    let mut m = Msg::new(ty, NLM_F_REQUEST | NLM_F_DUMP, 7, 0, &[family]);
    m.set_flags(NLM_F_REQUEST | NLM_F_DUMP);
    let req = m.done();
    let c = caller();
    n.send(&mut SliceSource { data: &req, at: 0 }, req.len(), 0, true, None, &c, None).map_err(|e| format!("send: {e}"))?;
    let mut out = Vec::new();
    for _ in 0..64 {
        let mut d = Vec::new();
        n.recv(65536, 0, true, &mut |b| {
            d.extend_from_slice(b);
            Ok(())
        })
        .map_err(|e| format!("recv: {e} (the dump must be there without waiting)"))?;
        let mut rest = &d[..];
        while let Some(h) = Hdr::parse(rest) {
            let len = h.len as usize;
            if len < NLMSG_HDRLEN || len > rest.len() {
                return Err(format!("broken message of length {len}"));
            }
            if h.ty == NLMSG_DONE {
                let e = i32::from_le_bytes(rest[16..20].try_into().unwrap_or([0; 4]));
                kassert!(e == 0, "DONE carries {e}");
                return Ok(out);
            }
            kassert!(h.seq == 7 && h.flags & NLM_F_MULTI != 0, "seq {} flags {:#x}", h.seq, h.flags);
            if h.ty == want {
                out.push(rest[NLMSG_HDRLEN..len].to_vec());
            }
            rest = &rest[align(len).min(rest.len())..];
        }
    }
    Err(String::from("no NLMSG_DONE"))
}

fn attr<'a>(payload: &'a [u8], fixed: usize, ty: u16) -> Option<&'a [u8]> {
    attrs(&payload[fixed.min(payload.len())..]).into_iter().find(|(t, _)| *t == ty).map(|(_, d)| d)
}

fn dumps() -> TestResult {
    let s = st()?;
    let card = configured(s)?;
    let n = NetlinkSock::create(3, 0, &caller()).map_err(|e| format!("create: {e}"))?;
    let links = dump(&n, RTM_GETLINK, 0, RTM_NEWLINK)?;
    let cards = s.interfaces();
    kassert!(links.len() == cards.len() + 1, "{} links for {} cards", links.len(), cards.len());
    kassert!(attr(&links[0], 16, 3) == Some(b"lo\0"), "lo first");
    kassert!(u32::from_le_bytes(links[0][4..8].try_into().unwrap_or([0; 4])) == 1, "lo is index 1");
    for (i, c) in cards.iter().enumerate() {
        let l = &links[i + 1];
        let index = u32::from_le_bytes(l[4..8].try_into().unwrap_or([0; 4]));
        let name = attr(l, 16, 3).unwrap_or(&[]);
        kassert!(index == i as u32 + 2 && name.strip_suffix(&[0]) == Some(c.name.as_bytes()), "card {} as index {index} {:?}", c.name, name);
        kassert!(attr(l, 16, 1) == Some(&c.mac[..]), "{}: IFLA_ADDRESS is the MAC", c.name);
    }
    let addrs = dump(&n, RTM_GETADDR, 2, RTM_NEWADDR)?;
    kassert!(addrs.iter().any(|a| a[4] == 1 && attr(a, 8, 2) == Some(&[127, 0, 0, 1][..]) && a[1] == 8), "127.0.0.1/8 on lo");
    if let Some(c) = card {
        let (ip, prefix) = c.info.addr.ok_or("configured card without address")?;
        kassert!(
            addrs.iter().any(|a| attr(a, 8, 2) == Some(&ip.0[..]) && a[1] == prefix && attr(a, 8, 3).and_then(|l| l.strip_suffix(&[0])) == Some(c.name.as_bytes())),
            "{}'s lease {} in the address dump",
            c.name,
            crate::stack::ip_str(ip)
        );
    }
    kassert!(dump(&n, RTM_GETADDR, 10, RTM_NEWADDR)?.is_empty(), "no IPv6 addresses");
    n.release();
    Ok(())
}
ktest!("nw.netlink.dumps", Safe, dumps);

/// A program memory of 4 KiB at 0x1000 for the ioctl structs.
struct Mem(SpinLock<Vec<u8>>);

impl vfs::sys::UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let m = self.0.lock();
        let Some(o) = addr.checked_sub(0x1000).map(|o| o as usize) else { return false };
        if o + buf.len() > m.len() {
            return false;
        }
        buf.copy_from_slice(&m[o..o + buf.len()]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut m = self.0.lock();
        let Some(o) = addr.checked_sub(0x1000).map(|o| o as usize) else { return false };
        if o + data.len() > m.len() {
            return false;
        }
        m[o..o + data.len()].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, i64> {
        Err(uapi::errno::EFAULT)
    }
}

fn ioctls() -> TestResult {
    let s = st()?;
    let card = configured(s)?;
    let mem = Mem(SpinLock::new(alloc::vec![0; 4096]));
    let a = Asker { mem: &mem, uid: 1000 };
    let inet = InetSock::create(2, 0, &caller()).map_err(|e| format!("udp socket: {e}"))?;
    let f = SockFile::new(Kind::Inet(inet)).map_err(|e| format!("{e}"))?;
    let ifr = |name: &[u8], int: u32| {
        let mut r = [0u8; 40];
        r[..name.len()].copy_from_slice(name);
        r[16..20].copy_from_slice(&int.to_le_bytes());
        vfs::sys::UserMem::write(&mem, 0x1000, &r);
    };
    let int = || {
        let mut b = [0u8; 4];
        vfs::sys::UserMem::read(&mem, 0x1010, &mut b);
        u32::from_le_bytes(b)
    };
    ifr(b"lo", 0);
    kassert!(devioctl::ioctl(&a, &f, devioctl::SIOCGIFINDEX, 0x1000) == Some(0) && int() == 1, "SIOCGIFINDEX lo");
    ifr(b"nope", 0);
    kassert!(devioctl::ioctl(&a, &f, devioctl::SIOCGIFINDEX, 0x1000) == Some(-uapi::errno::ENODEV), "unknown name");
    kassert!(devioctl::ioctl(&a, &f, devioctl::SIOCGIFINDEX, 0x9000) == Some(-uapi::errno::EFAULT), "unmapped ifreq");
    for (i, c) in s.interfaces().iter().enumerate() {
        ifr(b"", i as u32 + 2);
        kassert!(devioctl::ioctl(&a, &f, devioctl::SIOCGIFNAME, 0x1000) == Some(0), "SIOCGIFNAME {}", i + 2);
        let mut nb = [0u8; 16];
        vfs::sys::UserMem::read(&mem, 0x1000, &mut nb);
        kassert!(nb.starts_with(c.name.as_bytes()) && nb[c.name.len()] == 0, "index {} is {}", i + 2, c.name);
    }
    if let Some(c) = card {
        let (ip, _) = c.info.addr.ok_or("no address")?;
        ifr(c.name.as_bytes(), 0);
        kassert!(devioctl::ioctl(&a, &f, devioctl::SIOCGIFADDR, 0x1000) == Some(0), "SIOCGIFADDR {}", c.name);
        let mut b = [0u8; 4];
        vfs::sys::UserMem::read(&mem, 0x1014, &mut b);
        kassert!(b == ip.0, "{}: {:?} vs {:?}", c.name, b, ip.0);
    }
    // SIOCGIFCONF: one 40-byte entry per IPv4 address (lo's included).
    let mut ic = [0u8; 16];
    ic[8..16].copy_from_slice(&0u64.to_le_bytes());
    vfs::sys::UserMem::write(&mem, 0x1100, &ic);
    kassert!(devioctl::ioctl(&a, &f, devioctl::SIOCGIFCONF, 0x1100) == Some(0), "SIOCGIFCONF count");
    let mut b = [0u8; 4];
    vfs::sys::UserMem::read(&mem, 0x1100, &mut b);
    let want = 40 * (1 + s.interfaces().iter().filter(|i| i.info.addr.is_some()).count()) as u32;
    kassert!(u32::from_le_bytes(b) == want, "SIOCGIFCONF length {} vs {want}", u32::from_le_bytes(b));
    Ok(())
}
ktest!("nw.netlink.ioctls", Safe, ioctls);

/// Every datagram queued for `n` now (non-blocking): the messages of each, as (header, payload).
fn answers(n: &Arc<NetlinkSock>, req: &[u8]) -> Result<Vec<Vec<(Hdr, Vec<u8>)>>, String> {
    let c = caller();
    let sent = n.send(&mut SliceSource { data: req, at: 0 }, req.len(), 0, true, None, &c, None).map_err(|e| format!("send: {e}"))?;
    kassert!(sent == req.len(), "send took {sent} of {}", req.len());
    let mut out = Vec::new();
    loop {
        let mut d = Vec::new();
        match n.recv(65536, 0, true, &mut |b| {
            d.extend_from_slice(b);
            Ok(())
        }) {
            Ok(_) => {}
            Err(e) if e == EAGAIN => return Ok(out),
            Err(e) => return Err(format!("recv: {e}")),
        }
        let mut v = Vec::new();
        let mut rest = &d[..];
        while let Some(h) = Hdr::parse(rest) {
            let len = h.len as usize;
            if len < NLMSG_HDRLEN || len > rest.len() {
                return Err(format!("the kernel sent a broken message of length {len}"));
            }
            v.push((h, rest[NLMSG_HDRLEN..len].to_vec()));
            rest = &rest[align(len).min(rest.len())..];
        }
        out.push(v);
        kassert!(out.len() < 64, "endless answers");
    }
}

fn req(ty: u16, flags: u16, seq: u32, body: &[u8]) -> Vec<u8> {
    let mut m = Msg::new(ty, flags, seq, 0, body);
    m.set_flags(flags);
    m.done()
}

fn err_of(m: &[(Hdr, Vec<u8>)]) -> i32 {
    match m.first() {
        Some((h, b)) if h.ty == NLMSG_ERROR && b.len() >= 4 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        _ => i32::MIN,
    }
}

/// Malformed requests on a real socket get Linux's answers (`netlink_rcv_skb`, `rtnetlink_rcv_msg`): broken
/// headers end the datagram silently, a short payload or attribute is EINVAL/ERANGE, IPv6 GETADDR is
/// EADDRNOTAVAIL. The kernel never answers with a broken message and never stops answering.
fn hostile() -> TestResult {
    st()?;
    let n = NetlinkSock::create(3, 0, &caller()).map_err(|e| format!("create: {e}"))?;
    let lo = |flags: u16, seq: u32, tail: &[u8]| {
        let mut b = alloc::vec![0u8; 16];
        b[4] = 1;
        b.extend_from_slice(tail);
        req(RTM_GETLINK, flags, seq, &b)
    };
    let ack = NLM_F_REQUEST | NLM_F_ACK;
    let mut two = lo(ack, 1, &[]);
    let mut broken = req(RTM_GETLINK, ack, 2, &[0; 16]);
    broken[0..4].copy_from_slice(&200u32.to_le_bytes());
    two.extend_from_slice(&broken);
    let a = answers(&n, &two)?;
    kassert!(a.len() == 2 && a[0][0].0.ty == RTM_NEWLINK && err_of(&a[1]) == 0, "a request then a broken header: {} datagrams", a.len());
    for len in [0u32, 8, 15, u32::MAX] {
        broken[0..4].copy_from_slice(&len.to_le_bytes());
        kassert!(answers(&n, &broken)?.is_empty(), "nlmsg_len {len}: no answer");
    }
    kassert!(answers(&n, &broken[..10])?.is_empty(), "10 bytes: no answer");
    let a = answers(&n, &req(RTM_GETLINK, ack, 3, &[0]))?;
    kassert!(err_of(&a[0]) == -(EINVAL as i32), "1-byte get link: {}", err_of(&a[0]));
    let a = answers(&n, &lo(ack, 4, &[20, 0, 3, 0, b'l', b'o', 0, 0]))?;
    kassert!(a.len() == 2 && a[0][0].0.ty == RTM_NEWLINK, "truncated attribute without strict checking: answered");
    let a = answers(&n, &req(RTM_GETLINK, ack, 5, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 3, 0]))?;
    kassert!(err_of(&a[0]) == -(ERANGE as i32), "empty IFLA_IFNAME: {}", err_of(&a[0]));
    let mut v6 = alloc::vec![10u8, 0, 0, 0, 0, 0, 0, 0, 20, 0, 1, 0];
    v6.extend_from_slice(&[0xfe, 0x80]);
    v6.extend_from_slice(&[0; 13]);
    v6.push(1);
    let a = answers(&n, &req(RTM_GETADDR, ack, 6, &v6))?;
    kassert!(err_of(&a[0]) == -(EADDRNOTAVAIL as i32), "IPv6 GETADDR: {}", err_of(&a[0]));
    let c = caller();
    n.setsockopt(crate::sock::netlink::NETLINK_GET_STRICT_CHK, 1, &c).map_err(|e| format!("strict: {e}"))?;
    let a = answers(&n, &lo(ack, 7, &[20, 0, 3, 0, b'l', b'o', 0, 0]))?;
    kassert!(a.len() == 1 && err_of(&a[0]) == -(EINVAL as i32), "truncated attribute with strict checking: {}", err_of(&a[0]));
    // Still answering after all that.
    let a = answers(&n, &lo(ack, 8, &[]))?;
    kassert!(a.len() == 2 && a[0][0].0.ty == RTM_NEWLINK, "lo after the hostile requests");
    n.release();
    Ok(())
}
ktest!("nw.netlink.hostile", Safe, hostile);
