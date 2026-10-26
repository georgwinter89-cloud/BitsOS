//! Host tests of AF_INET6 program sockets (NW-v6): ::1, v4-mapped peers, IPV6_V6ONLY, SOL_IPV6 options. Ports
//! 24660-24679.

use super::abi::*;
use super::addr::encode_in;
use super::inet::{IPV6_V6ONLY, SOL_IPV6};
use super::sys;
use super::tests::{Prog, ns};
use crate::{Ipv4, SockAddr};
use uapi::errno::*;

fn stack() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| super::inet::set_stack(crate::stack::Stack::new_static(super::now, 7, false)));
}

fn in6(addr: [u8; 16], port: u16) -> Vec<u8> {
    let mut v = vec![0u8; 28];
    v[..2].copy_from_slice(&AF_INET6.to_le_bytes());
    v[2..4].copy_from_slice(&port.to_be_bytes());
    v[8..24].copy_from_slice(&addr);
    v
}

const ANY6: [u8; 16] = [0; 16];
const LO6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

fn mapped(ip: [u8; 4]) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[10] = 0xff;
    a[11] = 0xff;
    a[12..].copy_from_slice(&ip);
    a
}

fn bind(p: &Prog, fd: u64, a: &[u8]) -> i64 {
    let at = p.put(a);
    p.call(sys::bind, [fd, at, a.len() as u64, 0, 0, 0])
}

fn connect(p: &Prog, fd: u64, a: &[u8]) -> i64 {
    let at = p.put(a);
    p.call(sys::connect, [fd, at, a.len() as u64, 0, 0, 0])
}

fn setopt(p: &Prog, fd: u64, level: u32, name: u32, v: &[u8]) -> i64 {
    let at = p.put(v);
    p.call(sys::setsockopt, [fd, level as u64, name as u64, at, v.len() as u64, 0])
}

fn port_of(name: &[u8]) -> u16 {
    u16::from_be_bytes([name[2], name[3]])
}

fn accept(p: &Prog, srv: u64) -> (u64, Vec<u8>) {
    let (pa, pl) = (p.buf(64), p.put(&64u32.to_le_bytes()));
    let fd = p.call(sys::accept, [srv, pa, pl, 0, 0, 0]);
    assert!(fd >= 0, "accept: {fd}");
    let n = p.u32_at(pl) as usize;
    (fd as u64, p.get(pa, n))
}

#[test]
fn tcp_over_ipv6_loopback() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 40, 1000);
    let srv = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(p.getopt_int(srv, SOL_SOCKET, SO_DOMAIN), AF_INET6 as i64);
    assert_eq!(p.getname(srv, false).unwrap(), in6(ANY6, 0), "a fresh socket is [::]:0");
    assert_eq!(bind(&p, srv, &in6(LO6, 24660)), 0);
    assert_eq!(p.getname(srv, false).unwrap(), in6(LO6, 24660));
    assert_eq!(p.getopt_int(srv, SOL_IPV6, IPV6_V6ONLY), 1, "a bind to ::1 makes the socket IPv6-only");
    assert_eq!(p.call(sys::listen, [srv, 4, 0, 0, 0, 0]), 0);
    // An IPv4 client does not reach an IPv6-only listener.
    let v4 = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, v4, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24660 })), -ECONNREFUSED);
    let cli = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, cli, &in6(LO6, 24660)), 0);
    let (s, peer) = accept(&p, srv);
    let me = p.getname(cli, false).unwrap();
    assert_eq!(peer.len(), 28);
    assert_eq!(peer, in6(LO6, port_of(&me)), "the accepted peer is ::1 at the client's port");
    assert_eq!(p.getname(cli, true).unwrap(), in6(LO6, 24660));
    assert_eq!(p.getname(s, false).unwrap(), in6(LO6, 24660));
    assert_eq!(p.getopt_int(s, SOL_SOCKET, SO_DOMAIN), AF_INET6 as i64, "accept keeps the family");
    assert_eq!(p.send(cli, b"ping", 0), 4);
    assert_eq!(p.recv(s, 16, 0), (4, b"ping".to_vec()));
    assert_eq!(p.send(s, b"pong", 0), 4);
    assert_eq!(p.recv(cli, 16, 0), (4, b"pong".to_vec()));
    // An AF_INET listener does not take ::1.
    let l4 = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, l4, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24661 })), 0);
    assert_eq!(p.call(sys::listen, [l4, 4, 0, 0, 0, 0]), 0);
    let c6 = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c6, &in6(LO6, 24661)), -ECONNREFUSED);
    // ...but a v4-mapped destination is IPv4.
    let c6m = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c6m, &in6(mapped([127, 0, 0, 1]), 24661)), 0);
    let (s4, peer4) = accept(&p, l4);
    assert_eq!(peer4.len(), 16, "the AF_INET listener sees sockaddr_in");
    assert_eq!(&p.getname(c6m, false).unwrap()[8..24], &mapped([127, 0, 0, 1]));
    for fd in [srv, v4, cli, s, l4, c6, c6m, s4] {
        p.close(fd);
    }
}

#[test]
fn dual_stack_listener() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 41, 1000);
    let srv = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, srv, &in6(ANY6, 24662)), 0);
    let v4 = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, v4, &encode_in(SockAddr { ip: Ipv4::ANY, port: 24662 })), -EADDRINUSE, ":: covers 0.0.0.0");
    assert_eq!(bind(&p, v4, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24662 })), -EADDRINUSE);
    assert_eq!(p.call(sys::listen, [srv, 4, 0, 0, 0, 0]), 0);
    let c4 = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c4, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24662 })), 0);
    let (s4, peer4) = accept(&p, srv);
    let me4 = p.getname(c4, false).unwrap();
    assert_eq!(peer4, in6(mapped([127, 0, 0, 1]), port_of(&me4)), "an IPv4 peer shows v4-mapped");
    assert_eq!(&p.getname(s4, false).unwrap()[8..24], &mapped([127, 0, 0, 1]));
    let c6 = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c6, &in6(ANY6, 24662)), 0, ":: as a destination is ::1");
    let (s6, peer6) = accept(&p, srv);
    assert_eq!(&peer6[8..24], &LO6);
    // IPV6_V6ONLY: :: beside 0.0.0.0 on one port.
    let o = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(p.setopt_int(o, SOL_IPV6, IPV6_V6ONLY, 1), 0);
    assert_eq!(bind(&p, o, &in6(ANY6, 24663)), 0);
    assert_eq!(p.setopt_int(o, SOL_IPV6, IPV6_V6ONLY, 0), -EINVAL, "only before bind");
    let w = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, w, &encode_in(SockAddr { ip: Ipv4::ANY, port: 24663 })), 0);
    for fd in [srv, v4, c4, s4, c6, s6, o, w] {
        p.close(fd);
    }
}

#[test]
fn ipv6_address_errors_like_linux() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 42, 1000);
    let t = p.socket(AF_INET6, SOCK_STREAM) as u64;
    let raw = in6(LO6, 24664);
    assert_eq!(bind(&p, t, &raw[..23]), -EINVAL, "below SIN6_LEN_RFC2133");
    assert_eq!(bind(&p, t, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24664 })), -EINVAL, "a sockaddr_in is short");
    let mut fam = raw.clone();
    fam[0] = AF_INET as u8;
    assert_eq!(bind(&p, t, &fam), -EAFNOSUPPORT);
    let mut other = LO6;
    other[15] = 2;
    assert_eq!(bind(&p, t, &in6(other, 24664)), -EADDRNOTAVAIL, "::2 is no address of this host");
    let mut ll = [0u8; 16];
    ll[0] = 0xfe;
    ll[1] = 0x80;
    ll[15] = 1;
    assert_eq!(bind(&p, t, &in6(ll, 24664)), -EINVAL, "link-local without an interface");
    let mut mc = [0u8; 16];
    mc[0] = 0xff;
    mc[1] = 2;
    mc[15] = 1;
    assert_eq!(bind(&p, t, &in6(mc, 24664)), -EINVAL, "TCP on a multicast address");
    assert_eq!(bind(&p, t, &in6(LO6, 80)), -EACCES);
    let mut doc = [0u8; 16];
    doc[..4].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8]);
    doc[15] = 1;
    assert_eq!(connect(&p, t, &in6(doc, 80)), -ENETUNREACH, "no IPv6 route");
    assert_eq!(connect(&p, t, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 1 })), -EINVAL, "TCPv6 wants a sockaddr_in6");
    let o = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(p.setopt_int(o, SOL_IPV6, IPV6_V6ONLY, 1), 0);
    assert_eq!(bind(&p, o, &in6(mapped([127, 0, 0, 1]), 24664)), -EINVAL, "v4-mapped on an IPv6-only socket");
    assert_eq!(connect(&p, o, &in6(mapped([127, 0, 0, 1]), 24664)), -ENETUNREACH);
    let u = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.setopt_int(u, SOL_IPV6, IPV6_V6ONLY, 1), 0);
    assert_eq!(connect(&p, u, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 1 })), -EAFNOSUPPORT);
    assert_eq!(p.sendto(u, b"x", &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 1 })), -ENETUNREACH);
    assert_eq!(p.sendto(u, b"x", &in6(LO6, 0)), -EINVAL, "port 0");
    assert_eq!(p.sendto(u, b"x", &in6(doc, 9)), -ENETUNREACH);
    for fd in [t, o, u] {
        p.close(fd);
    }
}

#[test]
fn udp_over_ipv6_loopback() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 43, 1000);
    let a = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, a, &in6(LO6, 24665)), 0);
    let b = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(b, b"hello", &in6(LO6, 24665)), 5);
    let (r, d, from) = p.recvfrom(a, 16);
    assert_eq!((r, d.as_slice()), (5, &b"hello"[..]));
    assert_eq!(from, in6(LO6, port_of(&p.getname(b, false).unwrap())), "from ::1");
    // ::1 is IPv6-only: an IPv4 datagram to 127.0.0.1 does not arrive.
    let c = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(c, b"v4", &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24665 })), 2);
    assert_eq!(p.recv(a, 8, MSG_DONTWAIT).0, -EAGAIN);
    // A dual-stack :: hears both and names IPv4 senders v4-mapped.
    let d2 = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, d2, &in6(ANY6, 24666)), 0);
    assert_eq!(p.sendto(c, b"v4", &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24666 })), 2);
    let (_, _, f4) = p.recvfrom(d2, 8);
    assert_eq!(&f4[8..24], &mapped([127, 0, 0, 1]));
    assert_eq!(p.sendto(b, b"v6", &in6(LO6, 24666)), 2);
    let (_, _, f6) = p.recvfrom(d2, 8);
    assert_eq!(&f6[8..24], &LO6);
    // A sockaddr_in on a dual socket is IPv4.
    assert_eq!(p.sendto(b, b"x", &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24666 })), 1);
    let (_, _, fx) = p.recvfrom(d2, 8);
    assert_eq!(&fx[8..24], &mapped([127, 0, 0, 1]));
    // musl's AI_ADDRCONFIG probe: a UDP connect to [::1]:65535 works.
    let m = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(connect(&p, m, &in6(LO6, 65535)), 0);
    assert_eq!(p.getname(m, true).unwrap(), in6(LO6, 65535));
    assert_eq!(&p.getname(m, false).unwrap()[..2], &AF_INET6.to_le_bytes());
    for fd in [a, b, c, d2, m] {
        p.close(fd);
    }
}

#[test]
fn ipv6_options_like_linux() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 44, 1000);
    let t = p.socket(AF_INET6, SOCK_STREAM) as u64;
    let u = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    // nplb's PosixSocketSetOptionsTest.TryThemAll on AF_INET6.
    for (fd, lvl, name) in [(t, SOL_SOCKET, SO_REUSEADDR), (t, SOL_SOCKET, SO_KEEPALIVE), (t, SOL_SOCKET, SO_BROADCAST), (t, SOL_TCP, TCP_NODELAY), (t, SOL_TCP, TCP_KEEPIDLE), (t, SOL_TCP, TCP_KEEPINTVL), (u, SOL_SOCKET, SO_REUSEADDR), (u, SOL_SOCKET, SO_BROADCAST)] {
        assert_eq!(p.setopt_int(fd, lvl, name, 1), 0, "level {lvl} name {name}");
    }
    assert_eq!(p.getopt_int(t, SOL_IPV6, IPV6_V6ONLY), 0);
    assert_eq!(p.getopt_int(t, SOL_IPV6, 16), 64, "IPV6_UNICAST_HOPS default");
    assert_eq!(p.setopt_int(t, SOL_IPV6, 16, 256), -EINVAL);
    assert_eq!(p.setopt_int(t, SOL_IPV6, 16, 7), 0);
    assert_eq!(p.getopt_int(t, SOL_IPV6, 16), 7);
    assert_eq!(p.setopt_int(t, SOL_IPV6, 67, -1), 0, "IPV6_TCLASS -1 is the default");
    assert_eq!(p.setopt_int(t, SOL_IPV6, 67, 0xbb), 0);
    assert_eq!(p.getopt_int(t, SOL_IPV6, 67), 0xb8, "a stream keeps its ECN bits");
    assert_eq!(p.setopt_int(t, SOL_IPV6, 18, 5), -ENOPROTOOPT, "IPV6_MULTICAST_HOPS: not on TCP");
    assert_eq!(p.setopt_int(u, SOL_IPV6, 18, -1), 0);
    assert_eq!(p.getopt_int(u, SOL_IPV6, 18), 1);
    assert_eq!(p.setopt_int(u, SOL_IPV6, 19, 2), -EINVAL, "IPV6_MULTICAST_LOOP takes 0 or 1");
    assert_eq!(p.setopt_int(u, SOL_IPV6, 49, 1), 0, "IPV6_RECVPKTINFO");
    assert_eq!(p.getopt_int(u, SOL_IPV6, 49), 1);
    assert_eq!(p.getopt_int(u, SOL_IPV6, 72), 0x500, "IPV6_ADDR_PREFERENCES default");
    assert_eq!(p.setopt_int(u, SOL_IPV6, 999, 1), -ENOPROTOOPT);
    assert_eq!(p.getopt_int(u, SOL_IPV6, 24), -ENOTCONN, "IPV6_MTU unconnected");
    assert_eq!(p.getopt_int(u, SOL_IP, 2), 64, "SOL_IP on an AF_INET6 socket");
    assert_eq!(setopt(&p, u, SOL_IPV6, IPV6_V6ONLY, &[1, 0]), -EINVAL, "below an int");
    let v4 = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(p.setopt_int(v4, SOL_IPV6, IPV6_V6ONLY, 1), -ENOPROTOOPT, "no SOL_IPV6 on AF_INET");
    // ipv6_mreq: group ff02::fb on any interface.
    let mut mreq = [0u8; 20];
    mreq[0] = 0xff;
    mreq[1] = 2;
    mreq[15] = 0xfb;
    assert_eq!(setopt(&p, u, SOL_IPV6, 20, &mreq), 0);
    assert_eq!(setopt(&p, u, SOL_IPV6, 20, &mreq), -EADDRINUSE);
    assert_eq!(setopt(&p, u, SOL_IPV6, 21, &mreq), 0);
    assert_eq!(setopt(&p, u, SOL_IPV6, 21, &mreq), -EADDRNOTAVAIL);
    assert_eq!(setopt(&p, u, SOL_IPV6, 20, &mreq[..19]), -EINVAL);
    assert_eq!(setopt(&p, t, SOL_IPV6, 20, &mreq), -EPROTO_TEST);
    let mut not_mc = mreq;
    not_mc[0] = 0;
    assert_eq!(setopt(&p, u, SOL_IPV6, 20, &not_mc), -EINVAL);
    for fd in [t, u, v4] {
        p.close(fd);
    }
}

const EPROTO_TEST: i64 = 71;

#[test]
fn addrform_turns_a_mapped_socket_into_ipv4() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 45, 1000);
    let u = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.setopt_int(u, SOL_IPV6, 1, AF_INET as i32), -ENOTCONN);
    assert_eq!(connect(&p, u, &in6(LO6, 24667)), 0);
    assert_eq!(p.setopt_int(u, SOL_IPV6, 1, AF_INET as i32), -EADDRNOTAVAIL, "::1 has no IPv4 form");
    assert_eq!(connect(&p, u, &in6(mapped([127, 0, 0, 1]), 24667)), 0);
    assert_eq!(p.getopt_int(u, SOL_IPV6, 1), AF_INET6 as i64);
    assert_eq!(p.setopt_int(u, SOL_IPV6, 1, AF_INET as i32), 0);
    assert_eq!(p.getopt_int(u, SOL_SOCKET, SO_DOMAIN), AF_INET as i64);
    assert_eq!(p.getname(u, true).unwrap(), encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24667 }));
    p.close(u);
}

/// The control messages of a recvmsg: (level, type, data).
fn cmsgs(mut c: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
    let mut v = Vec::new();
    while c.len() >= 16 {
        let len = u64::from_le_bytes(c[..8].try_into().unwrap()) as usize;
        let (level, ty) = (u32::from_le_bytes(c[8..12].try_into().unwrap()), u32::from_le_bytes(c[12..16].try_into().unwrap()));
        v.push((level, ty, c[16..len].to_vec()));
        c = &c[cmsg_space(len - 16).min(c.len())..];
    }
    v
}

#[test]
fn failed_bind_gives_v6only_back() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 46, 1000);
    for ty in [SOCK_STREAM, SOCK_DGRAM] {
        let a = p.socket(AF_INET6, ty) as u64;
        assert_eq!(bind(&p, a, &in6(LO6, 24668)), 0);
        let b = p.socket(AF_INET6, ty) as u64;
        assert_eq!(bind(&p, b, &in6(LO6, 24668)), -EADDRINUSE);
        assert_eq!(p.getopt_int(b, SOL_IPV6, IPV6_V6ONLY), 0, "__inet6_bind restores saved_ipv6only");
        assert_eq!(bind(&p, b, &in6(ANY6, 24669)), 0);
        assert_eq!(p.getopt_int(b, SOL_IPV6, IPV6_V6ONLY), 0, "the retry on :: is dual-stack");
        let c = p.socket(AF_INET, ty) as u64;
        assert_eq!(bind(&p, c, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24669 })), -EADDRINUSE);
        for fd in [a, b, c] {
            p.close(fd);
        }
    }
}

#[test]
fn ipv6_only_and_ipv4_listeners_share_a_port() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 47, 1000);
    let l6 = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(p.setopt_int(l6, SOL_IPV6, IPV6_V6ONLY, 1), 0);
    assert_eq!(bind(&p, l6, &in6(ANY6, 24670)), 0);
    assert_eq!(p.call(sys::listen, [l6, 4, 0, 0, 0, 0]), 0);
    let l4 = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, l4, &encode_in(SockAddr { ip: Ipv4::ANY, port: 24670 })), 0);
    assert_eq!(p.call(sys::listen, [l4, 4, 0, 0, 0, 0]), 0, "an IPv6-only listener beside an AF_INET one");
    let c4 = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c4, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24670 })), 0);
    let (s4, peer4) = accept(&p, l4);
    assert_eq!(&peer4[..2], &AF_INET.to_le_bytes(), "the IPv4 client reached the AF_INET listener");
    let c6 = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c6, &in6(LO6, 24670)), 0);
    let (s6, peer6) = accept(&p, l6);
    assert_eq!(&peer6[8..24], &LO6, "the ::1 client reached the IPv6 listener");
    // ::1 beside 127.0.0.1, both listening.
    let a = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, a, &in6(LO6, 24671)), 0);
    assert_eq!(p.call(sys::listen, [a, 4, 0, 0, 0, 0]), 0);
    let b = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, b, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24671 })), 0);
    assert_eq!(p.call(sys::listen, [b, 4, 0, 0, 0, 0]), 0);
    // An AF_INET program's 127.0.6.1 (the kernel's stand-in for ::1) does not reach an ::1 listener.
    let l = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, l, &in6(LO6, 24672)), 0);
    assert_eq!(p.call(sys::listen, [l, 4, 0, 0, 0, 0]), 0);
    let x = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, x, &encode_in(SockAddr { ip: Ipv4([127, 0, 6, 1]), port: 24672 })), -ECONNREFUSED);
    for fd in [l6, l4, c4, s4, c6, s6, a, b, l, x] {
        p.close(fd);
    }
    // The port is free on both sides again.
    let d = p.socket(AF_INET6, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, d, &in6(ANY6, 24670)), 0);
    assert_eq!(p.call(sys::listen, [d, 4, 0, 0, 0, 0]), 0);
    p.close(d);
}

#[test]
fn udp_ports_per_side() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 48, 1000);
    let u6 = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.setopt_int(u6, SOL_IPV6, IPV6_V6ONLY, 1), 0);
    assert_eq!(bind(&p, u6, &in6(ANY6, 24673)), 0);
    let u4 = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, u4, &encode_in(SockAddr { ip: Ipv4::ANY, port: 24673 })), 0, "IPv6-only beside AF_INET");
    let s = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(s, b"six", &in6(LO6, 24673)), 3);
    assert_eq!(p.sendto(s, b"four", &in6(mapped([127, 0, 0, 1]), 24673)), 4);
    assert_eq!(p.recvfrom(u6, 8).1, b"six");
    assert_eq!(p.recvfrom(u4, 8).1, b"four");
    assert_eq!(p.recv(u6, 8, MSG_DONTWAIT).0, -EAGAIN);
    // An AF_INET sender to 127.0.6.1 is IPv4: it reaches the AF_INET socket, not the IPv6-only one.
    let s4 = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(s4, b"x", &encode_in(SockAddr { ip: Ipv4([127, 0, 6, 1]), port: 24673 })), 1);
    assert_eq!(p.recvfrom(u4, 8).1, b"x");
    assert_eq!(p.recv(u6, 8, MSG_DONTWAIT).0, -EAGAIN);
    // A dual-stack :: holds both sides.
    let d = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, d, &in6(ANY6, 24674)), 0);
    let o = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.setopt_int(o, SOL_IPV6, IPV6_V6ONLY, 1), 0);
    assert_eq!(bind(&p, o, &in6(ANY6, 24674)), -EADDRINUSE);
    let w = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, w, &encode_in(SockAddr { ip: Ipv4::ANY, port: 24674 })), -EADDRINUSE);
    for fd in [u6, u4, s, s4, d, o, w] {
        p.close(fd);
    }
    let again = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, again, &in6(ANY6, 24674)), 0, "close gives both sides back");
    p.close(again);
}

#[test]
fn multicast_bind_like_linux() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 49, 1000);
    let mut grp = [0u8; 16];
    grp[0] = 0xff;
    grp[1] = 2;
    grp[15] = 0xfb;
    let u = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, u, &in6(grp, 24675)), -EINVAL, "a link-scope group needs an interface");
    let mut scoped = in6(grp, 24675);
    scoped[24] = 1;
    assert_eq!(bind(&p, u, &scoped), 0);
    assert_eq!(p.getname(u, false).unwrap(), scoped, "getsockname shows the group and its interface");
    assert_eq!(p.getopt_int(u, SOL_IPV6, IPV6_V6ONLY), 1);
    let v = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    grp[1] = 5;
    assert_eq!(bind(&p, v, &in6(grp, 24676)), 0, "a site-scope group needs none");
    assert_eq!(p.getname(v, false).unwrap(), in6(grp, 24676));
    // The group socket takes no unicast to ::1.
    let s = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(s, b"x", &in6(LO6, 24676)), 1);
    assert_eq!(p.recv(v, 8, MSG_DONTWAIT).0, -EAGAIN);
    for fd in [u, v, s] {
        p.close(fd);
    }
}

#[test]
fn receive_ancillary_data_like_linux() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 50, 1000);
    let r = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, r, &in6(ANY6, 24677)), 0);
    for name in [49, 51, 66] {
        assert_eq!(p.setopt_int(r, SOL_IPV6, name, 1), 0);
    }
    let s6 = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(s6, b"six", &in6(LO6, 24677)), 3);
    let (n, _, ctl, _) = p.recvmsg(r, 8, 128, 0);
    assert_eq!(n, 3);
    let mut info6 = LO6.to_vec();
    info6.extend_from_slice(&1u32.to_le_bytes());
    assert_eq!(cmsgs(&ctl), vec![(SOL_IPV6, 50, info6), (SOL_IPV6, 52, 64i32.to_le_bytes().to_vec()), (SOL_IPV6, 67, 0i32.to_le_bytes().to_vec())]);
    // An IPv4 datagram on the same socket: IPV6_PKTINFO (v4-mapped), then the SOL_IP ones it asked for.
    assert_eq!(p.setopt_int(r, SOL_IP, 8, 1), 0);
    let s4 = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(p.sendto(s4, b"four", &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24677 })), 4);
    let (_, _, ctl, _) = p.recvmsg(r, 8, 128, 0);
    let mut info = mapped([127, 0, 0, 1]).to_vec();
    info.extend_from_slice(&1u32.to_le_bytes());
    let mut pi4 = 1u32.to_le_bytes().to_vec();
    pi4.extend_from_slice(&[127, 0, 0, 1, 127, 0, 0, 1]);
    assert_eq!(cmsgs(&ctl), vec![(SOL_IPV6, 50, info), (SOL_IP, 8, pi4.clone())]);
    // AF_INET: IP_PKTINFO, IP_TTL (an int), IP_TOS (a byte).
    let a = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, a, &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24678 })), 0);
    for name in [8, 12, 13] {
        assert_eq!(p.setopt_int(a, SOL_IP, name, 1), 0);
    }
    assert_eq!(p.sendto(s4, b"x", &encode_in(SockAddr { ip: Ipv4::LOCALHOST, port: 24678 })), 1);
    let (_, _, ctl, _) = p.recvmsg(a, 8, 128, MSG_PEEK);
    assert_eq!(cmsgs(&ctl), vec![(SOL_IP, 8, pi4), (SOL_IP, 2, 64i32.to_le_bytes().to_vec()), (SOL_IP, 1, vec![0])]);
    let (_, _, ctl, fl) = p.recvmsg(a, 8, 0, 0);
    assert!(ctl.is_empty() && fl & MSG_CTRUNC != 0, "no room: MSG_CTRUNC");
    for fd in [r, s6, s4, a] {
        p.close(fd);
    }
}

#[test]
fn addrform_gives_the_ipv6_side_back() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 51, 1000);
    let u = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, u, &in6(ANY6, 24679)), 0);
    assert_eq!(connect(&p, u, &in6(mapped([127, 0, 0, 1]), 24660)), 0);
    assert_eq!(p.setopt_int(u, SOL_IPV6, 1, AF_INET as i32), 0);
    let o = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(p.setopt_int(o, SOL_IPV6, IPV6_V6ONLY, 1), 0);
    assert_eq!(bind(&p, o, &in6(ANY6, 24679)), 0, "the AF_INET socket now holds the IPv4 side only");
    for fd in [u, o] {
        p.close(fd);
    }
}

#[test]
fn short_values_and_bad_pointers() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 52, 1000);
    let u = p.socket(AF_INET6, SOCK_DGRAM) as u64;
    assert_eq!(setopt(&p, u, SOL_IPV6, 16, &[1, 0]), -EINVAL, "IPV6_UNICAST_HOPS below an int");
    assert_eq!(setopt(&p, u, SOL_IPV6, 67, &[1]), -EINVAL, "IPV6_TCLASS below an int");
    let bad = 0xffff_8000_0000_0000u64;
    assert_eq!(p.call(sys::setsockopt, [u, SOL_IPV6 as u64, 16, bad, 4, 0]), -EFAULT);
    assert_eq!(p.call(sys::bind, [u, bad, 28, 0, 0, 0]), -EFAULT);
    assert_eq!(p.call(sys::connect, [u, bad, 28, 0, 0, 0]), -EFAULT);
    p.close(u);
}
