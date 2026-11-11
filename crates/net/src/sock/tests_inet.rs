//! Host tests of AF_INET program sockets on a stack without cards: TCP and UDP over 127.0.0.1.

use super::abi::*;
use super::addr::encode_in;
use super::sys;
use super::tests::{Prog, in_call, ns};
use crate::{Ipv4, SockAddr};
use uapi::errno::*;
use vfs::Events as E;

fn stack() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| super::inet::set_stack(crate::stack::Stack::new_static(super::now, 7, false)));
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

fn port_of(name: &[u8]) -> u16 {
    u16::from_be_bytes([name[2], name[3]])
}

#[test]
fn tcp_over_loopback() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 30, 1000);
    let srv = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(p.events(srv) & (E::HUP | E::OUT), E::HUP | E::OUT, "a fresh TCP socket: HUP and OUT");
    assert_eq!(bind(&p, srv, &any(24601)), 0);
    assert_eq!(p.getname(srv, false).unwrap(), any(24601));
    let other = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, other, &any(24601)), -EADDRINUSE);
    assert_eq!(bind(&p, other, &lo(80)), -EACCES, "privileged port for uid 1000");
    assert_eq!(bind(&p, other, &encode_in(SockAddr { ip: Ipv4([8, 8, 8, 8]), port: 1 })), -EADDRNOTAVAIL);
    let mut short = any(1);
    short.truncate(15);
    assert_eq!(bind(&p, other, &short), -EINVAL);
    assert_eq!(p.call(sys::listen, [srv, 8, 0, 0, 0, 0]), 0);
    assert_eq!(p.getopt_int(srv, SOL_SOCKET, SO_ACCEPTCONN), 1);
    let cli = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, cli, &lo(24601)), 0);
    assert_eq!(connect(&p, cli, &lo(24601)), -EISCONN);
    assert!(p.events(srv) & E::IN != 0, "a connection to accept");
    let pa = p.buf(16);
    let pl = p.put(&16u32.to_le_bytes());
    let s = p.call(sys::accept4, [srv, pa, pl, SOCK_NONBLOCK, 0, 0]);
    assert!(s >= 0, "accept {s}");
    let s = s as u64;
    let cname = p.getname(cli, false).unwrap();
    assert_eq!(p.get(pa, 16), cname, "accept names the client");
    assert_eq!(p.getname(cli, true).unwrap(), lo(24601));
    assert_eq!(p.send(cli, b"GET / HTTP/1.0\r\n\r\n", 0), 18);
    assert_eq!(p.recv(s, 4, MSG_PEEK), (4, b"GET ".to_vec()));
    assert_eq!(p.recv(s, 64, 0).0, 18);
    assert_eq!(p.recv(s, 64, 0).0, -EAGAIN, "accept4 SOCK_NONBLOCK");
    assert_eq!(p.call(sys::shutdown, [cli, SHUT_WR, 0, 0, 0, 0]), 0);
    assert_eq!(p.recv(s, 64, 0).0, 0, "FIN: EOF");
    assert!(p.events(s) & E::RDHUP != 0);
    assert_eq!(p.send(s, b"answer", 0), 6, "the other direction still works");
    assert_eq!(p.recv(cli, 64, 0), (6, b"answer".to_vec()));
    assert_eq!(p.send(cli, b"x", MSG_NOSIGNAL), -EPIPE, "after SHUT_WR");
    assert_eq!(p.getopt_int(cli, SOL_SOCKET, SO_TYPE), SOCK_STREAM as i64);
    assert_eq!(p.getopt_int(cli, SOL_SOCKET, SO_PROTOCOL), IPPROTO_TCP as i64);
    assert_eq!(p.setopt_int(cli, SOL_TCP, TCP_NODELAY, 1), 0);
    assert_eq!(p.getopt_int(cli, SOL_TCP, TCP_NODELAY), 1);
    assert_eq!(p.getopt_int(cli, SOL_TCP, 9999), -ENOPROTOOPT);
    assert_eq!(p.getopt_int(cli, SOL_IP, 2), 64, "IP_TTL");
    // Closing with unread data resets the peer: ECONNRESET once, then EOF, and writes fail.
    let c2 = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, c2, &lo(24601)), 0);
    let s2 = p.call(sys::accept, [srv, 0, 0, 0, 0, 0]) as u64;
    assert_eq!(p.send(c2, b"unread", 0), 6);
    p.close(s2);
    assert_eq!(p.events(c2) & (E::IN | E::HUP | E::ERR | E::RDHUP), E::IN | E::HUP | E::ERR | E::RDHUP);
    assert_eq!(p.recv(c2, 8, MSG_DONTWAIT).0, -ECONNRESET);
    assert_eq!(p.recv(c2, 8, MSG_DONTWAIT).0, 0);
    assert_eq!(p.send(c2, b"x", MSG_NOSIGNAL), -EPIPE);
    p.close(c2);
    p.close(s);
    p.close(cli);
    p.close(srv);
    // The port is free again.
    let again = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, again, &any(24601)), 0);
    p.close(again);
}

#[test]
fn tcp_nonblocking_connect_refused_and_reuse() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 31, 1000);
    let c = p.call(sys::socket, [AF_INET as u64, (SOCK_STREAM as u64) | SOCK_NONBLOCK, 0, 0, 0, 0]) as u64;
    assert_eq!(connect(&p, c, &lo(24602)), -EINPROGRESS, "non-blocking connect");
    assert_eq!(p.events(c) & (E::ERR | E::HUP | E::OUT), E::ERR | E::HUP | E::OUT, "refused: POLLERR|POLLHUP|POLLOUT");
    assert_eq!(p.events(c) & (E::IN | E::RDHUP), E::IN | E::RDHUP, "tcp_done shut both directions");
    assert_eq!(p.getopt_int(c, SOL_SOCKET, SO_ERROR), ECONNREFUSED);
    assert_eq!(p.getopt_int(c, SOL_SOCKET, SO_ERROR), 0, "SO_ERROR is cleared by reading it");
    assert_eq!(connect(&p, c, &lo(24602)), -ECONNABORTED, "connect() again after the error was read");
    let b = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, b, &lo(24602)), -ECONNREFUSED, "blocking: at once");
    assert_eq!(p.recv(b, 4, 0).0, -ENOTCONN);
    assert_eq!(p.send(b, b"x", MSG_NOSIGNAL), -EPIPE, "send on an unconnected TCP socket");
    // SO_REUSEADDR on both lets two unconnected sockets bind one port; a listener then is alone.
    let x = p.socket(AF_INET, SOCK_STREAM) as u64;
    let y = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(p.setopt_int(x, SOL_SOCKET, SO_REUSEADDR, 1), 0);
    assert_eq!(p.setopt_int(y, SOL_SOCKET, SO_REUSEADDR, 1), 0);
    assert_eq!(bind(&p, x, &any(24603)), 0);
    assert_eq!(bind(&p, y, &any(24603)), 0, "both SO_REUSEADDR");
    assert_eq!(p.call(sys::listen, [x, 4, 0, 0, 0, 0]), 0, "beside an idle SO_REUSEADDR socket");
    assert_eq!(p.call(sys::listen, [y, 4, 0, 0, 0, 0]), -EADDRINUSE, "not beside a listener");
    p.close(y);
    // Listen without bind picks a port.
    let z = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(p.call(sys::listen, [z, 1, 0, 0, 0, 0]), 0);
    assert_ne!(port_of(&p.getname(z, false).unwrap()), 0);
    // A non-blocking connect that works: EINPROGRESS, then writable, then connect() says 0, then EISCONN.
    let n = p.call(sys::socket, [AF_INET as u64, (SOCK_STREAM as u64) | SOCK_NONBLOCK, 0, 0, 0, 0]) as u64;
    assert_eq!(connect(&p, n, &lo(24603)), -EINPROGRESS);
    assert!(p.events(n) & E::OUT != 0);
    assert_eq!(connect(&p, n, &lo(24603)), 0);
    assert_eq!(connect(&p, n, &lo(24603)), -EISCONN);
    for fd in [c, b, x, z, n] {
        p.close(fd);
    }
}

#[test]
fn udp_over_loopback() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 32, 1000);
    let a = p.socket(AF_INET, SOCK_DGRAM) as u64;
    let b = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, a, &lo(24610)), 0);
    assert_eq!(p.events(a) & (E::IN | E::OUT), E::OUT);
    assert_eq!(p.send(b, b"x", 0), -EDESTADDRREQ);
    assert_eq!(p.sendto(b, b"hello", &lo(24610)), 5);
    let bname = p.getname(b, false).unwrap();
    assert_ne!(port_of(&bname), 0, "sendto binds an ephemeral port");
    let (r, d, from) = p.recvfrom(a, 3);
    assert_eq!((r, d.as_slice()), (3, &b"hel"[..]), "truncated to the buffer");
    assert_eq!(port_of(&from), port_of(&bname));
    assert_eq!(&from[4..8], &[127, 0, 0, 1]);
    assert_eq!(p.recv(a, 8, MSG_DONTWAIT).0, -EAGAIN, "the rest of a datagram is gone");
    p.sendto(b, b"twelve bytes", &lo(24610));
    assert_eq!(p.recv(a, 6, MSG_PEEK), (6, b"twelve".to_vec()), "a peek leaves the datagram queued");
    let (r, _, _, fl) = p.recvmsg(a, 4, 0, 0);
    assert_eq!(r, 4);
    assert!(fl & MSG_TRUNC != 0);
    // A connected socket takes its peer's datagrams only; a refusal arrives as its next error.
    let c = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(connect(&p, c, &lo(24611)), 0);
    assert_eq!(p.send(c, b"x", 0), 1);
    assert_eq!(p.send(c, b"x", 0), -ECONNREFUSED, "nobody on 24611: ICMP unreachable reported");
    assert_eq!(p.call(sys::shutdown, [b, SHUT_RDWR, 0, 0, 0, 0]), -ENOTCONN, "unconnected UDP");
    for fd in [a, b, c] {
        p.close(fd);
    }
    let again = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, again, &lo(24610)), 0, "the port came back");
    p.close(again);
}

#[test]
fn inet_creation_errors() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 33, 1000);
    assert_eq!(p.call(sys::socket, [AF_INET as u64, SOCK_STREAM as u64, 17, 0, 0, 0]), -EPROTONOSUPPORT);
    assert_eq!(p.call(sys::socket, [AF_INET as u64, SOCK_STREAM as u64, 300, 0, 0, 0]), -EINVAL);
    assert_eq!(p.socket(AF_INET, SOCK_SEQPACKET), -ESOCKTNOSUPPORT);
    assert_eq!(p.socket(AF_INET, SOCK_RAW), -EPERM);
    let sv = p.buf(8);
    assert_eq!(p.call(sys::socketpair, [AF_INET as u64, SOCK_STREAM as u64, 0, sv, 0, 0]), -EOPNOTSUPP);
    assert_eq!(p.fds.count(), 0, "socketpair gave its fds back");
}

#[test]
fn tcp_shutdown_against_blocked_calls() {
    stack();
    let ns = ns();
    let p = std::sync::Arc::new(Prog::new(&ns, 32, 1000));
    let srv = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, srv, &lo(24611)), 0);
    assert_eq!(p.call(sys::listen, [srv, 4, 0, 0, 0, 0]), 0);
    let cli = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, cli, &lo(24611)), 0);
    let s = p.call(sys::accept, [srv, 0, 0, 0, 0, 0]);
    assert!(s >= 0);
    let s = s as u64;
    // A blocked recv ends with EOF on shutdown(SHUT_RD) from another thread (`inet_shutdown`, `tcp_recvmsg`).
    let t = in_call(&p, s, move |q| q.recv(s, 16, 0).0);
    assert_eq!(p.call(sys::shutdown, [s, SHUT_RD, 0, 0, 0, 0]), 0);
    assert_eq!(t.join().unwrap(), 0);
    // A blocked accept ends with EINVAL when the listener is shut down (`inet_shutdown` disconnects a listener).
    let t = in_call(&p, srv, move |q| q.call(sys::accept, [srv, 0, 0, 0, 0, 0]));
    assert_eq!(p.call(sys::shutdown, [srv, SHUT_RDWR, 0, 0, 0, 0]), 0);
    assert_eq!(t.join().unwrap(), -EINVAL);
    // ioctls: SIOCATMARK is 0 on TCP (no urgent data), ENOTTY on UDP; SIOCGSTAMP has no stamp: ENOENT.
    let ioctl = |fd: u64, cmd: u64| -> Result<i32, i64> {
        let f = p.fds.get(fd as usize).unwrap();
        f.file.ioctl(cmd, 0).map(|v| i32::from_le_bytes(v[..4].try_into().unwrap()))
    };
    assert_eq!(ioctl(cli, SIOCATMARK), Ok(0));
    let u = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(ioctl(u, SIOCATMARK), Err(ENOTTY));
    assert_eq!(ioctl(u, SIOCGSTAMP_OLD), Err(ENOENT));
    assert_eq!(p.getopt(u, SOL_SOCKET, SO_PEERGROUPS, 64), Err(-ENODATA));
    for fd in [srv, cli, s, u] {
        p.close(fd);
    }
}

/// NW-10: poll, select and epoll subscribe to AF_INET sockets' queue (`inet::READY`), so a sleeper wakes with the
/// data instead of at the next 10 ms look (BusyBox wget stalled ~9 ms each time its buffer ran dry). An epoll
/// sleeping with a long timeout returns the socket once data arrives.
#[test]
fn inet_sockets_wake_poll_and_epoll() {
    use vfs::epoll::{new_epoll, EPOLL_CTL_ADD};
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 31, 1000);
    let srv = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, srv, &any(24659)), 0);
    assert_eq!(p.call(sys::listen, [srv, 8, 0, 0, 0, 0]), 0);
    let cli = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(connect(&p, cli, &lo(24659)), 0);
    let s = p.call(sys::accept4, [srv, 0, 0, SOCK_NONBLOCK, 0, 0]);
    assert!(s >= 0, "accept {s}");
    let f = p.fds.get(s as usize).unwrap();
    let mut named = 0;
    f.file.poll_wait(&mut |q| {
        assert!(core::ptr::eq(q, &super::inet::READY), "an AF_INET socket names inet::READY");
        named += 1;
    });
    assert_eq!(named, 1, "exactly one queue");
    let ep = new_epoll();
    vfs::epoll::Epoll::ctl(&ep, EPOLL_CTL_ADD, &f, s as i32, (E::IN, 7), false).unwrap();
    let sleeper = {
        let ep = ep.clone();
        std::thread::spawn(move || {
            let mut v: Vec<(u32, u64)> = Vec::new();
            ep.file.as_epoll().unwrap().wait(&mut v, 8, Some(10_000_000_000), &|| false).unwrap();
            v
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(p.send(cli, b"data", 0), 4);
    let got = sleeper.join().unwrap();
    assert_eq!(got.len(), 1, "woken with the socket: {got:?}");
    assert!(got[0].0 & E::IN != 0 && got[0].1 == 7, "{got:?}");
    assert_eq!(p.recv(s as u64, 8, 0).0, 4);
    p.close(s as u64);
    p.close(cli);
    p.close(srv);
}

/// An epoll item for `fd` and a thread waiting on it for 20 s. AF_INET sockets name `inet::READY` and the epoll
/// never looks again by itself, so the thread comes back with an event only when READY woke it.
fn sleeper(p: &Prog, fd: u64, mask: u32) -> std::thread::JoinHandle<Vec<(u32, u64)>> {
    use vfs::epoll::{new_epoll, EPOLL_CTL_ADD};
    let f = p.fds.get(fd as usize).unwrap();
    let mut named = 0;
    f.file.poll_wait(&mut |q| {
        assert!(core::ptr::eq(q, &super::inet::READY));
        named += 1;
    });
    assert_eq!(named, 1, "an AF_INET socket names inet::READY (no 10 ms looks)");
    let ep = new_epoll();
    vfs::epoll::Epoll::ctl(&ep, EPOLL_CTL_ADD, &f, fd as i32, (mask, fd), false).unwrap();
    std::thread::spawn(move || {
        let mut v: Vec<(u32, u64)> = Vec::new();
        let end = vfs::poll::now() + 20_000_000_000;
        ep.file.as_epoll().unwrap().wait(&mut v, 8, Some(end), &|| false).unwrap();
        v
    })
}

fn woken(h: std::thread::JoinHandle<Vec<(u32, u64)>>, want: u32, what: &str) {
    let got = h.join().unwrap();
    assert!(got.len() == 1 && got[0].0 & want == want, "{what}: {got:?} (want {want:#x})");
}

/// NW-10 fixer: epoll on AF_INET sockets wakes on every change, not only on data: a non-blocking connect that
/// completes (EPOLLOUT), the peer's FIN (EPOLLIN|EPOLLRDHUP), a reset (EPOLLERR|EPOLLHUP), a UDP datagram, and
/// room to send again (EPOLLOUT).
#[test]
fn inet_epoll_wakes_on_every_change() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 33, 1000);
    let srv = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(bind(&p, srv, &any(24690)), 0);
    assert_eq!(p.call(sys::listen, [srv, 8, 0, 0, 0, 0]), 0);
    let pair = || {
        let c = p.socket(AF_INET, SOCK_STREAM) as u64;
        assert_eq!(connect(&p, c, &lo(24690)), 0);
        let s = p.call(sys::accept4, [srv, 0, 0, SOCK_NONBLOCK, 0, 0]);
        assert!(s >= 0, "accept {s}");
        (c, s as u64)
    };
    // A non-blocking connect (it may be done before the item is added: then the first look finds it).
    let n = p.call(sys::socket, [AF_INET as u64, (SOCK_STREAM as u64) | SOCK_NONBLOCK, 0, 0, 0, 0]) as u64;
    assert_eq!(connect(&p, n, &lo(24690)), -EINPROGRESS);
    woken(sleeper(&p, n, E::OUT), E::OUT, "connect done");
    let ns_ = p.call(sys::accept4, [srv, 0, 0, SOCK_NONBLOCK, 0, 0]);
    assert!(ns_ >= 0);
    // The peer's FIN.
    let (c, s) = pair();
    assert_eq!(p.events(s) & (E::IN | E::RDHUP), 0);
    let h = sleeper(&p, s, E::IN | E::RDHUP);
    assert_eq!(p.call(sys::shutdown, [c, SHUT_WR, 0, 0, 0, 0]), 0);
    woken(h, E::IN | E::RDHUP, "FIN");
    // A reset: SO_LINGER 0 s, then close.
    let (c2, s2) = pair();
    assert_eq!(p.events(s2) & (E::ERR | E::HUP), 0);
    let h = sleeper(&p, s2, E::IN);
    let linger = p.put(&[1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(p.call(sys::setsockopt, [c2, SOL_SOCKET as u64, SO_LINGER as u64, linger, 8, 0]), 0);
    p.close(c2);
    woken(h, E::ERR | E::HUP, "reset");
    // A UDP datagram.
    let u = p.socket(AF_INET, SOCK_DGRAM) as u64;
    let v = p.socket(AF_INET, SOCK_DGRAM) as u64;
    assert_eq!(bind(&p, u, &lo(24691)), 0);
    assert_eq!(p.events(u) & E::IN, 0);
    let h = sleeper(&p, u, E::IN);
    assert_eq!(p.sendto(v, b"dgram", &lo(24691)), 5);
    woken(h, E::IN, "datagram");
    // Room to send again after the peer read.
    let (c3, s3) = pair();
    p.set_nonblock(c3);
    let chunk = alloc::vec![3u8; 64 * 1024];
    let mut idle = 0;
    while idle < 5 {
        if p.send(c3, &chunk, 0) == -EAGAIN {
            idle += 1;
            std::thread::sleep(std::time::Duration::from_millis(5));
        } else {
            idle = 0;
        }
    }
    assert_eq!(p.events(c3) & E::OUT, 0, "the send side is full");
    let h = sleeper(&p, c3, E::OUT);
    while p.recv(s3, 64 * 1024, MSG_DONTWAIT).0 > 0 {}
    woken(h, E::OUT, "room to send");
    for fd in [n, ns_ as u64, c, s, s2, u, v, c3, s3, srv] {
        p.close(fd);
    }
}

/// NW-10 fixer: TCP_QUICKACK reads back !pingpong as on Linux (1 on a fresh socket, `do_tcp_getsockopt`
/// linux-6.6/net/ipv4/tcp.c:4023); 0 and 1 switch it, and listen() clears it.
#[test]
fn tcp_quickack_reads_back_as_linux() {
    stack();
    let ns = ns();
    let p = Prog::new(&ns, 34, 1000);
    let t = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(p.getopt_int(t, SOL_TCP, TCP_QUICKACK), 1, "fresh: not in pingpong mode");
    assert_eq!(p.setopt_int(t, SOL_TCP, TCP_QUICKACK, 0), 0);
    assert_eq!(p.getopt_int(t, SOL_TCP, TCP_QUICKACK), 0);
    assert_eq!(p.setopt_int(t, SOL_TCP, TCP_QUICKACK, 2), 0);
    assert_eq!(p.getopt_int(t, SOL_TCP, TCP_QUICKACK), 1, "any non-zero value leaves pingpong");
    assert_eq!(p.setopt_int(t, SOL_TCP, TCP_QUICKACK, 0), 0);
    assert_eq!(bind(&p, t, &any(24692)), 0);
    assert_eq!(p.call(sys::listen, [t, 2, 0, 0, 0, 0]), 0);
    assert_eq!(p.getopt_int(t, SOL_TCP, TCP_QUICKACK), 1, "listen clears the ACK state");
    let c = p.socket(AF_INET, SOCK_STREAM) as u64;
    assert_eq!(p.setopt_int(c, SOL_TCP, TCP_QUICKACK, 0), 0);
    assert_eq!(connect(&p, c, &lo(24692)), 0);
    assert_eq!(p.getopt_int(c, SOL_TCP, TCP_QUICKACK), 0, "kept over connect");
    let a = p.call(sys::accept4, [t, 0, 0, 0, 0, 0]);
    assert!(a >= 0);
    assert_eq!(p.getopt_int(a as u64, SOL_TCP, TCP_QUICKACK), 1, "an accepted socket starts fresh");
    for fd in [t, c, a as u64] {
        p.close(fd);
    }
}
