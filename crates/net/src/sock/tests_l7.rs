//! L7-NET host tests: where Linux 7.3.1 answers differently from Linux 6.6 in the socket layer, each case the
//! way linux-7.3.1 answers (plan/spec/net.md §10.3): the receive options by family (SO_PASSCRED, SO_PASSSEC,
//! SO_PASSPIDFD, SO_PASSRIGHTS, AF_UNIX's SO_INQ and SO_RIGHTS_NOTRUNC), and rtnetlink's order of checks for
//! RTM_NEWLINK and RTM_NEWADDR in a namespace.

use super::abi::*;
use super::nlmsg::*;
use super::tests::{Prog, ns, rights, sun};
use super::tests_netns::{ack, drain, enter, ifinfo, ns_root, req, view};
use super::sys;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

fn int_opt(p: &Prog, fd: u64, name: u32, v: i32) -> i64 {
    p.setopt_int(fd, SOL_SOCKET, name, v)
}

fn opt_len(p: &Prog, fd: u64, name: u32, v: &[u8]) -> i64 {
    let a = p.put(v);
    p.call(sys::setsockopt, [fd, SOL_SOCKET as u64, name as u64, a, v.len() as u64, 0])
}

#[test]
fn receive_options_belong_to_their_families() {
    let fs = ns();
    let p = Prog::new(&fs, 71, 1000);
    let udp = p.socket(AF_INET, SOCK_DGRAM) as u64;
    for name in [SO_PASSCRED, SO_PASSSEC, SO_PASSPIDFD, SO_PASSRIGHTS] {
        assert_eq!(int_opt(&p, udp, name, 1), -EOPNOTSUPP, "setsockopt {name} on AF_INET (sock.c:1560-1586)");
        assert_eq!(p.getopt_int(udp, SOL_SOCKET, name), -EOPNOTSUPP, "getsockopt {name} on AF_INET (sock.c:1882-1900)");
    }
    assert_eq!(int_opt(&p, udp, SO_INQ, 1), -ENOPROTOOPT, "SO_INQ is AF_UNIX's");
    assert_eq!(int_opt(&p, udp, SO_RIGHTS_NOTRUNC, 1), -ENOPROTOOPT);

    let nl = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    assert_eq!(int_opt(&p, nl, SO_PASSCRED, 1), 0, "netlink may pass credentials");
    assert_eq!(int_opt(&p, nl, SO_PASSSEC, 1), 0);
    assert_eq!(p.getopt_int(nl, SOL_SOCKET, SO_PASSSEC), 1);
    assert_eq!(int_opt(&p, nl, SO_PASSPIDFD, 1), -EOPNOTSUPP, "pidfds are AF_UNIX only");
    assert_eq!(p.getopt_int(nl, SOL_SOCKET, SO_PASSRIGHTS), -EOPNOTSUPP);

    let (a, _b) = p.pair(SOCK_DGRAM);
    assert_eq!(p.getopt_int(a, SOL_SOCKET, SO_PASSRIGHTS), 1, "a new AF_UNIX socket takes files");
    assert_eq!(int_opt(&p, a, SO_PASSRIGHTS, 0), 0);
    assert_eq!(p.getopt_int(a, SOL_SOCKET, SO_PASSRIGHTS), 0);
    assert_eq!(int_opt(&p, a, SO_PASSPIDFD, 5), 0, "any non-zero int turns it on");
    assert_eq!(p.getopt_int(a, SOL_SOCKET, SO_PASSPIDFD), 1);
    assert_eq!(int_opt(&p, a, SO_PASSSEC, 1), 0);
    assert_eq!(p.getopt_int(a, SOL_SOCKET, SO_PASSSEC), 1);

    // `unix_setsockopt`: exactly an int, 0 or 1, SO_INQ for streams only; getsockopt does not know them.
    assert_eq!(int_opt(&p, a, SO_INQ, 1), -ENOPROTOOPT, "SO_INQ on a datagram socket");
    assert_eq!(int_opt(&p, a, SO_RIGHTS_NOTRUNC, 1), 0);
    assert_eq!(int_opt(&p, a, SO_RIGHTS_NOTRUNC, 2), -EINVAL);
    assert_eq!(p.getopt_int(a, SOL_SOCKET, SO_RIGHTS_NOTRUNC), -ENOPROTOOPT);
    let (s, _t) = p.pair(SOCK_STREAM);
    assert_eq!(int_opt(&p, s, SO_INQ, 1), 0);
    assert_eq!(int_opt(&p, s, SO_INQ, -1), -EINVAL);
    assert_eq!(opt_len(&p, s, SO_INQ, &[1, 0, 0, 0, 0, 0, 0, 0]), -EINVAL, "optlen must be sizeof(int)");
    assert_eq!(opt_len(&p, s, SO_INQ, &[1, 0]), -EINVAL);
    assert_eq!(p.getopt_int(s, SOL_SOCKET, SO_INQ), -ENOPROTOOPT);
    // The length check comes before the type check (af_unix.c:944 before :953).
    assert_eq!(opt_len(&p, a, SO_INQ, &[1, 0]), -EINVAL);
}

#[test]
fn a_receiver_without_passrights_refuses_files() {
    let fs = ns();
    let p = Prog::new(&fs, 72, 1000);
    let file = p.socket(AF_INET, SOCK_DGRAM) as i32;
    for ty in [SOCK_DGRAM, SOCK_SEQPACKET, SOCK_STREAM] {
        let (a, b) = p.pair(ty);
        assert_eq!(int_opt(&p, b, SO_PASSRIGHTS, 0), 0);
        assert_eq!(p.sendmsg(a, b"F", &rights(&[file]), None, 0), -EPERM, "type {ty}: af_unix.c:2245/:2485");
        assert_eq!(p.sendmsg(a, b"x", &[], None, 0), 1, "data without files still goes");
        assert_eq!(int_opt(&p, b, SO_PASSRIGHTS, 1), 0);
        assert_eq!(p.sendmsg(a, b"F", &rights(&[file]), None, 0), 1);
    }
    // A connection takes its listener's receive options when it is made (af_unix.c:1750, :1755); SO_INQ stays.
    let name = sun(b"\0l7-passrights");
    let l = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let at = p.put(&name);
    assert_eq!(p.call(sys::bind, [l, at, name.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::listen, [l, 4, 0, 0, 0, 0]), 0);
    for (opt, v) in [(SO_PASSRIGHTS, 0), (SO_PASSCRED, 1), (SO_PASSPIDFD, 1), (SO_RIGHTS_NOTRUNC, 1), (SO_INQ, 1)] {
        assert_eq!(int_opt(&p, l, opt, v), 0);
    }
    let c = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let at = p.put(&name);
    assert_eq!(p.call(sys::connect, [c, at, name.len() as u64, 0, 0, 0]), 0);
    assert_eq!(int_opt(&p, l, SO_PASSCRED, 0), 0, "a change after the connect does not reach the connection");
    assert_eq!(p.sendmsg(c, b"F", &rights(&[file]), None, 0), -EPERM, "the embryo refuses files already");
    let s = p.call(sys::accept4, [l, 0, 0, 0, 0, 0]) as u64;
    assert_eq!(p.getopt_int(s, SOL_SOCKET, SO_PASSRIGHTS), 0);
    assert_eq!(p.getopt_int(s, SOL_SOCKET, SO_PASSCRED), 1);
    assert_eq!(p.getopt_int(s, SOL_SOCKET, SO_PASSPIDFD), 1);
    assert_eq!(p.send(c, b"abc", 0), 3);
    let (r, _, ctl, _) = p.recvmsg(s, 8, 256, 0);
    assert_eq!(r, 3);
    assert!(!ctl_types(&ctl).contains(&SCM_INQ), "SO_INQ is not inherited");
}

/// The (level, type, data) of every control message.
fn ctl_msgs(ctl: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
    super::cmsg::headers(ctl).unwrap().into_iter().map(|(l, t, d)| (l, t, d.to_vec())).collect()
}

fn ctl_types(ctl: &[u8]) -> Vec<u32> {
    ctl_msgs(ctl).into_iter().map(|(_, t, _)| t).collect()
}

#[test]
fn so_inq_reports_what_is_left() {
    let fs = ns();
    let p = Prog::new(&fs, 73, 1000);
    let (a, b) = p.pair(SOCK_STREAM);
    assert_eq!(int_opt(&p, b, SO_INQ, 1), 0);
    assert_eq!(p.send(a, b"hello", 0), 5);
    assert_eq!(p.send(a, b"world", 0), 5);
    let (r, d, ctl, fl) = p.recvmsg(b, 3, 64, 0);
    assert_eq!((r, d.as_slice()), (3, &b"hel"[..]));
    assert_eq!(ctl_msgs(&ctl), alloc::vec![(SOL_SOCKET, SCM_INQ, 7u32.to_le_bytes().to_vec())], "SCM_INQ after the read");
    assert_eq!(fl & MSG_CTRUNC, 0);
    let (r, _, _, fl) = p.recvmsg(b, 3, 0, 0);
    assert_eq!(r, 3);
    assert_ne!(fl & MSG_CTRUNC, 0, "no room for SCM_INQ: put_cmsg says MSG_CTRUNC");
    let (r, _, ctl, _) = p.recvmsg(b, 64, 64, MSG_PEEK);
    assert_eq!(r, 4);
    assert_eq!(ctl_msgs(&ctl)[0].2, 4u32.to_le_bytes().to_vec(), "a peek leaves the bytes queued");
}

/// Which process runs as pid 74 now: a new number after the old one "ended and its pid was reused".
static PID74: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// The test's proc: only pid 74 exists; the held process is its incarnation, and its pidfd an eventfd whose
/// count names that incarnation.
fn pin_test(pid: u32) -> Option<super::PinnedPid> {
    (pid == 74).then(|| Arc::new(PID74.load(core::sync::atomic::Ordering::SeqCst)) as super::PinnedPid)
}

fn open_test(p: &super::PinnedPid) -> Result<Arc<vfs::OpenFile>, vfs::Errno> {
    p.downcast_ref::<u32>().map(|n| vfs::eventfd::new_eventfd(*n, 0)).ok_or(ESRCH)
}

static PIDFD: super::PidfdHook = super::PidfdHook { pin: pin_test, open: open_test };

/// The incarnation an SCM_PIDFD fd names (the test hook's eventfd count), or the errno it stands for.
fn pidfd_names(p: &Prog, ctl: &[u8]) -> i64 {
    let m = ctl_msgs(ctl);
    let (_, _, d) = m.iter().find(|(l, t, _)| (*l, *t) == (SOL_SOCKET, SCM_PIDFD)).expect("SCM_PIDFD");
    let fd = i32::from_le_bytes(d[..4].try_into().unwrap());
    if fd < 0 {
        return fd as i64;
    }
    let f = p.fds.get(fd as usize).expect("installed");
    let mut b = [0u8; 8];
    assert_eq!(vfs::File::read_at(&*f.file, 0, &mut b), Ok(8));
    u64::from_le_bytes(b) as i64
}

#[test]
fn so_passpidfd_hands_over_the_sender() {
    super::set_pidfd_hook(&PIDFD);
    let fs = ns();
    let p = Prog::new(&fs, 74, 1000);
    let (a, b) = p.pair(SOCK_DGRAM);
    assert_eq!(int_opt(&p, b, SO_PASSPIDFD, 1), 0);
    assert_eq!(p.send(a, b"x", 0), 1);
    let (r, _, ctl, fl) = p.recvmsg(b, 4, 64, 0);
    assert_eq!((r, fl & MSG_CTRUNC), (1, 0));
    let m = ctl_msgs(&ctl);
    assert_eq!(m.len(), 1, "SCM_PIDFD alone: SO_PASSPIDFD gives no SCM_CREDENTIALS");
    assert_eq!((m[0].0, m[0].1), (SOL_SOCKET, SCM_PIDFD));
    let fd = i32::from_le_bytes(m[0].2[..4].try_into().unwrap());
    assert!(fd >= 0 && p.fds.get(fd as usize).is_ok(), "an fd for the sender");
    assert!(p.fds.cloexec(fd as usize).unwrap_or(false), "pidfd_prepare: always close-on-exec");
    // Too little room for an int: MSG_CTRUNC, no fd; no control buffer at all: MSG_CTRUNC as well.
    assert_eq!(p.send(a, b"y", 0), 1);
    let (_, _, ctl, fl) = p.recvmsg(b, 4, cmsg_len(4) - 1, 0);
    assert!(ctl.is_empty() && fl & MSG_CTRUNC != 0);
    assert_eq!(p.send(a, b"z", 0), 1);
    let (_, _, _, fl) = p.recvmsg(b, 4, 0, 0);
    assert_ne!(fl & MSG_CTRUNC, 0);
    // Both SO_PASSCRED and SO_PASSPIDFD: credentials first (`scm_recv_unix`).
    assert_eq!(int_opt(&p, b, SO_PASSCRED, 1), 0);
    assert_eq!(p.send(a, b"w", 0), 1);
    let (_, _, ctl, _) = p.recvmsg(b, 4, 128, 0);
    assert_eq!(ctl_types(&ctl), alloc::vec![SCM_CREDENTIALS, SCM_PIDFD]);
    assert_eq!(pidfd_names(&p, &ctl), 1);
    // The sender ends and its pid goes to another process before the receive: the pidfd still names the sender
    // (the message held it from the send on, PIDFD_STALE), never the new owner of the number.
    for ty in [SOCK_DGRAM, SOCK_STREAM] {
        let (s1, r1) = p.pair(ty);
        assert_eq!(int_opt(&p, r1, SO_PASSPIDFD, 1), 0);
        assert_eq!(p.send(s1, b"old", 0), 3);
        PID74.store(2, core::sync::atomic::Ordering::SeqCst);
        let (_, _, ctl, _) = p.recvmsg(r1, 8, 64, 0);
        assert_eq!(pidfd_names(&p, &ctl), 1, "type {ty}: the sender, not pid 74's new process");
        assert_eq!(p.send(s1, b"new", 0), 3);
        let (_, _, ctl, _) = p.recvmsg(r1, 8, 64, 0);
        assert_eq!(pidfd_names(&p, &ctl), 2, "type {ty}: a message of the new process names it");
        PID74.store(1, core::sync::atomic::Ordering::SeqCst);
    }
    // A stream read never joins messages of two processes with the same pid number (`unix_skb_scm_eq`).
    let (s1, r1) = p.pair(SOCK_STREAM);
    assert_eq!(int_opt(&p, r1, SO_PASSPIDFD, 1), 0);
    assert_eq!(p.send(s1, b"ab", 0), 2);
    PID74.store(2, core::sync::atomic::Ordering::SeqCst);
    assert_eq!(p.send(s1, b"cd", 0), 2);
    PID74.store(1, core::sync::atomic::Ordering::SeqCst);
    let (r, _, ctl, _) = p.recvmsg(r1, 8, 64, 0);
    assert_eq!((r, pidfd_names(&p, &ctl)), (2, 1));
    let (r, _, ctl, _) = p.recvmsg(r1, 8, 64, 0);
    assert_eq!((r, pidfd_names(&p, &ctl)), (2, 2));
    // A sender that cannot be held (no such process for the hook): ESRCH in place of the fd.
    let q = Prog::new(&fs, 75, 1000);
    let (s2, r2) = q.pair(SOCK_DGRAM);
    assert_eq!(int_opt(&q, r2, SO_PASSPIDFD, 1), 0);
    assert_eq!(q.send(s2, b"x", 0), 1);
    let (_, _, ctl, _) = q.recvmsg(r2, 8, 64, 0);
    assert_eq!(pidfd_names(&q, &ctl), -(ESRCH as i64));
    // SO_PASSSEC: no label without a security module, but a missing control buffer is MSG_CTRUNC.
    let (c, d) = p.pair(SOCK_DGRAM);
    assert_eq!(int_opt(&p, d, SO_PASSSEC, 1), 0);
    assert_eq!(p.send(c, b"s", 0), 1);
    let (_, _, ctl, fl) = p.recvmsg(d, 4, 64, 0);
    assert!(ctl.is_empty() && fl & MSG_CTRUNC == 0);
    assert_eq!(p.send(c, b"s", 0), 1);
    let (_, _, _, fl) = p.recvmsg(d, 4, 0, 0);
    assert_ne!(fl & MSG_CTRUNC, 0, "scm_has_secdata");
}

#[test]
fn msg_no_shared_frags_is_internal() {
    assert_ne!(MSG_INTERNAL_SENDMSG_FLAGS & 0x8_0000, 0, "include/linux/socket.h:358 (Linux 7)");
}

// ---- rtnetlink in a namespace ------------------------------------------------------------------------------

const RTM_NEWLINK: u16 = 16;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_CACHEINFO: u16 = 6;
const IFA_RT_PRIORITY: u16 = 9;
const IFA_PROTO: u16 = 11;

fn ask(p: &Prog, n: u64, m: &[u8]) -> i32 {
    p.send(n, m, 0);
    ack(&drain(p, n))
}

#[test]
fn newlink_checks_as_linux_7() {
    let fs = ns();
    let p = Prog::new(&fs, 75, 1000);
    let net = view(0xF000_0071);
    enter(&net, Some(ns_root(75, true)));
    let n = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    let f = NLM_F_REQUEST | NLM_F_ACK;
    assert_eq!(ask(&p, n, &req(RTM_NEWLINK, f, 1, &ifinfo(-1, 0, 0))), -(EINVAL as i32), "ifindex can't be negative");
    assert_eq!(ask(&p, n, &req(RTM_NEWLINK, f, 2, &ifinfo(7, 0, 0))), -(ENODEV as i32));
    assert_eq!(ask(&p, n, &req(RTM_NEWLINK, f | NLM_F_CREATE, 3, &ifinfo(7, 0, 0))), -(EOPNOTSUPP as i32), "Unknown device type");
    assert_eq!(ask(&p, n, &req(RTM_NEWLINK, f | NLM_F_CREATE, 4, &ifinfo(0, 0, 0))), -(EOPNOTSUPP as i32));
    assert_eq!(ask(&p, n, &req(RTM_NEWLINK, f, 5, &ifinfo(0, 0, 0))), -(ENODEV as i32));
    assert_eq!(ask(&p, n, &req(RTM_NEWLINK, f | NLM_F_CREATE | NLM_F_EXCL, 6, &ifinfo(1, 0, 0))), -(EEXIST as i32), "lo exists");
    assert_eq!(ask(&p, n, &req(RTM_SETLINK, f, 7, &ifinfo(-1, 0, 0))), -(EINVAL as i32), "SETLINK: no index, no name");
    let mut named = Msg::new(RTM_SETLINK, f, 8, 0, &ifinfo(-1, 0, 0));
    named.str(3, b"lo");
    named.set_flags(f);
    assert_eq!(ask(&p, n, &named.done()), 0, "SETLINK finds lo by name past a negative index");
    // `ip link add NAME type KIND` sends IFLA_LINKINFO: the index, the lookup and NLM_F_EXCL come before the kind.
    let ce = f | NLM_F_CREATE | NLM_F_EXCL;
    let kinded = |ty: u16, flags: u16, seq: u32, index: i32, name: Option<&[u8]>, data: bool| {
        let mut m = Msg::new(ty, flags, seq, 0, &ifinfo(index, 0, 0));
        if let Some(nm) = name {
            m.str(3, nm);
        }
        m.attr(18, &linkinfo(data));
        m.set_flags(flags);
        m.done()
    };
    assert_eq!(ask(&p, n, &kinded(RTM_NEWLINK, ce, 9, -1, None, false)), -(EINVAL as i32), "negative index first");
    assert_eq!(ask(&p, n, &kinded(RTM_NEWLINK, ce, 10, 1, None, true)), -(EEXIST as i32), "lo exists, data or not");
    assert_eq!(ask(&p, n, &kinded(RTM_NEWLINK, ce, 11, 0, Some(b"lo"), false)), -(EEXIST as i32));
    assert_eq!(ask(&p, n, &kinded(RTM_NEWLINK, ce, 12, 0, Some(b"nope"), false)), -(EOPNOTSUPP as i32), "Unknown device type");
    assert_eq!(ask(&p, n, &kinded(RTM_NEWLINK, f, 13, 0, Some(b"nope"), true)), -(ENODEV as i32));
    assert_eq!(ask(&p, n, &kinded(RTM_NEWLINK, f, 14, 1, None, true)), -(EOPNOTSUPP as i32), "lo has no link ops");
    assert_eq!(ask(&p, n, &kinded(RTM_NEWLINK, f, 15, 1, None, false)), 0, "a kind alone changes nothing");
    assert_eq!(ask(&p, n, &kinded(RTM_SETLINK, f, 16, 1, None, true)), 0, "rtnl_setlink ignores IFLA_LINKINFO");
    // Attributes V2 cannot apply are refused where `do_setlink` would apply them: a missing device says so first.
    let mut m = Msg::new(RTM_NEWLINK, f, 17, 0, &ifinfo(7, 0, 0));
    m.attr(1, &[2, 0, 0, 0, 0, 1]);
    m.set_flags(f);
    assert_eq!(ask(&p, n, &m.done()), -(ENODEV as i32), "IFLA_ADDRESS on a device that is not there");
    super::tests_netns::enter(&None, None);
}

/// IFLA_LINKINFO { IFLA_INFO_KIND "nokind", and an empty IFLA_INFO_DATA when `data` }.
fn linkinfo(data: bool) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&11u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(b"nokind\0\0");
    if data {
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
    }
    v
}

/// An RTM_NEWADDR request for `local`/`prefix` on interface `index`, with extra attributes.
fn newaddr(seq: u32, flags: u16, index: u32, local: [u8; 4], prefix: u8, extra: &[(u16, &[u8])]) -> Vec<u8> {
    let mut fixed = [0u8; 8];
    fixed[0] = 2;
    fixed[1] = prefix;
    fixed[4..8].copy_from_slice(&index.to_le_bytes());
    let f = NLM_F_REQUEST | NLM_F_ACK | flags;
    let mut m = Msg::new(RTM_NEWADDR, f, seq, 0, &fixed);
    m.attr(IFA_LOCAL, &local).attr(IFA_ADDRESS, &local);
    for (t, d) in extra {
        m.attr(*t, d);
    }
    m.set_flags(f);
    m.done()
}

fn cacheinfo(preferred: u32, valid: u32) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0..4].copy_from_slice(&preferred.to_le_bytes());
    c[4..8].copy_from_slice(&valid.to_le_bytes());
    c
}

/// (IFA_PROTO, IFA_RT_PRIORITY) of an address message, None where absent.
fn proto_metric(body: &[u8]) -> (Option<u8>, Option<u32>) {
    let a = attrs(&body[8..]);
    let get = |t: u16| a.iter().find(|(x, _)| *x == t).map(|(_, d)| d.to_vec());
    (get(IFA_PROTO).map(|d| d[0]), get(IFA_RT_PRIORITY).map(|d| u32::from_le_bytes(d[..4].try_into().unwrap())))
}

#[test]
fn newaddr_checks_in_linux_7_order_and_replace_updates() {
    let fs = ns();
    let p = Prog::new(&fs, 76, 1000);
    let net = view(0xF000_0072);
    enter(&net, Some(ns_root(76, true)));
    let n = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    let member = p.socket(AF_NETLINK, SOCK_RAW) as u64;
    assert_eq!(p.setopt_int(member, super::netlink::SOL_NETLINK, 1, 5), 0, "RTNLGRP_IPV4_IFADDR");
    let ip = [192, 0, 2, 1];
    // `inet_validate_rtm` before the device: a bad lifetime on an unknown index is EINVAL, not ENODEV.
    assert_eq!(ask(&p, n, &newaddr(1, 0, 9, ip, 24, &[(IFA_CACHEINFO, &cacheinfo(0, 0))])), -(EINVAL as i32));
    assert_eq!(ask(&p, n, &newaddr(2, 0, 9, ip, 33, &[])), -(EINVAL as i32), "prefix before device");
    // 0.0.0.0 is accepted before the device is looked up.
    assert_eq!(ask(&p, n, &newaddr(3, 0, 9, [0; 4], 24, &[])), 0);
    assert_eq!(ask(&p, n, &newaddr(4, 0, 9, ip, 24, &[])), -(ENODEV as i32));
    // IFA_PROTO is kept and shown; a replace takes the new protocol and metric and is announced each time.
    assert_eq!(ask(&p, n, &newaddr(5, NLM_F_CREATE, 1, ip, 24, &[(IFA_PROTO, &[4])])), 0);
    let heard: Vec<_> = drain(&p, member).into_iter().filter(|(h, _)| h.ty == RTM_NEWADDR).collect();
    assert_eq!(heard.len(), 1);
    assert_eq!((heard[0].0.seq, proto_metric(&heard[0].1)), (5, (Some(4), None)), "__inet_insert_ifa: rtmsg_ifa with nlh");
    assert_eq!(ask(&p, n, &newaddr(6, NLM_F_CREATE, 1, ip, 24, &[])), -(EEXIST as i32));
    assert_eq!(ask(&p, n, &newaddr(7, NLM_F_REPLACE, 1, ip, 24, &[(IFA_PROTO, &[7]), (IFA_RT_PRIORITY, &50u32.to_le_bytes())])), 0);
    let heard: Vec<_> = drain(&p, member).into_iter().filter(|(h, _)| h.ty == RTM_NEWADDR).collect();
    assert_eq!(heard.len(), 1, "one RTM_NEWADDR for the replace");
    assert_eq!((heard[0].0.seq, proto_metric(&heard[0].1)), (7, (Some(7), Some(50))), "rtmsg_ifa with the request's seq");
    assert_eq!(ask(&p, n, &newaddr(8, NLM_F_REPLACE, 1, ip, 24, &[(IFA_PROTO, &[7]), (IFA_RT_PRIORITY, &50u32.to_le_bytes())])), 0);
    let heard: Vec<_> = drain(&p, member).into_iter().filter(|(h, _)| h.ty == RTM_NEWADDR).collect();
    assert_eq!(heard.len(), 1, "a replace that changes nothing is announced as well");
    // Without IFA_PROTO the replace clears it (`ifa->ifa_proto = new_proto`).
    assert_eq!(ask(&p, n, &newaddr(9, NLM_F_REPLACE, 1, ip, 24, &[])), 0);
    let mut ga = alloc::vec![2u8];
    ga.resize(8, 0);
    p.send(n, &req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 10, &ga), 0);
    let dump: Vec<_> = drain(&p, n).into_iter().filter(|(h, _)| h.ty == RTM_NEWADDR).collect();
    assert_eq!(dump.len(), 1);
    assert_eq!(proto_metric(&dump[0].1), (None, None));
    // A finite lifetime: EEXIST still wins for a plain add; where Linux would set it, V2 says EOPNOTSUPP.
    assert_eq!(ask(&p, n, &newaddr(11, NLM_F_CREATE, 1, ip, 24, &[(IFA_CACHEINFO, &cacheinfo(10, 20))])), -(EEXIST as i32));
    assert_eq!(ask(&p, n, &newaddr(12, NLM_F_REPLACE, 1, ip, 24, &[(IFA_CACHEINFO, &cacheinfo(10, 20))])), -(EOPNOTSUPP as i32));
    assert_eq!(ask(&p, n, &newaddr(13, NLM_F_CREATE, 1, [192, 0, 2, 9], 24, &[(IFA_CACHEINFO, &cacheinfo(10, 20))])), -(EOPNOTSUPP as i32));
    super::tests_netns::enter(&None, None);
}

// ---- the unix collector's triggers (row 51: same results as before) ---------------------------------------

/// Releases unix sockets until both are gone (other tests' collections may be running on other threads).
fn collected(p: &Prog, w: &[alloc::sync::Weak<vfs::OpenFile>]) -> bool {
    for _ in 0..100 {
        if w.iter().all(|w| w.strong_count() == 0) {
            return true;
        }
        let (x, y) = p.pair(SOCK_DGRAM);
        p.close(x);
        p.close(y);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[test]
fn a_live_cycle_stays_and_goes_once_its_fd_closes() {
    let fs = ns();
    let p = Prog::new(&fs, 77, 1000);
    let (a, b) = p.pair(SOCK_DGRAM);
    let wb = Arc::downgrade(&p.fds.get(b as usize).unwrap());
    // b travels in its own queue; its fd stays open: alive.
    assert_eq!(p.sendmsg(a, b"1", &rights(&[b as i32]), None, 0), 1);
    assert!(super::unix_gc::may_be_cyclic(), "a socket queued at itself may make a cycle");
    for _ in 0..3 {
        let (x, y) = p.pair(SOCK_DGRAM);
        p.close(x);
        p.close(y);
    }
    super::unix_gc::collect();
    assert!(wb.strong_count() > 0 && p.fds.get(b as usize).is_ok(), "a cycle with an open fd is alive");
    p.close(b);
    assert!(collected(&p, &[wb]), "closing the fd leaves garbage the next release collects");
    p.close(a);
}

#[test]
fn a_cycle_closed_by_a_later_send_is_collected() {
    let fs = ns();
    let p = Prog::new(&fs, 78, 1000);
    let (a1, a2) = p.pair(SOCK_DGRAM);
    let (b1, b2) = p.pair(SOCK_DGRAM);
    let wa = Arc::downgrade(&p.fds.get(a2 as usize).unwrap());
    let wb = Arc::downgrade(&p.fds.get(b2 as usize).unwrap());
    // b2 waits in a2's queue first (a2 is not in flight yet), then a2 goes into b2's queue: the cycle closes on
    // the second send, whose receiver is in flight.
    assert_eq!(p.sendmsg(a1, b"1", &rights(&[b2 as i32]), None, 0), 1);
    assert_eq!(p.sendmsg(b1, b"2", &rights(&[a2 as i32]), None, 0), 1);
    for fd in [a1, a2, b1, b2] {
        p.close(fd);
    }
    assert!(collected(&p, &[wa, wb]));
}

#[test]
fn an_embryo_cycle_is_collected() {
    let fs = ns();
    let p = Prog::new(&fs, 79, 1000);
    let name = sun(b"\0l7-gc-embryo");
    let l = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let at = p.put(&name);
    assert_eq!(p.call(sys::bind, [l, at, name.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::listen, [l, 4, 0, 0, 0, 0]), 0);
    let c = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let at = p.put(&name);
    assert_eq!(p.call(sys::connect, [c, at, name.len() as u64, 0, 0, 0]), 0);
    let wl = Arc::downgrade(&p.fds.get(l as usize).unwrap());
    // The listener travels to the embryo nobody accepted: it holds the embryo, the embryo's queue holds it.
    assert_eq!(p.sendmsg(c, b"L", &rights(&[l as i32]), None, 0), 1);
    p.close(l);
    p.close(c);
    assert!(collected(&p, &[wl]));
}

/// Sends `n` messages of SCM_MAX_FD copies of socket `x` from `a` (each copy one file in flight).
fn flood(p: &Prog, a: u64, x: u64, n: usize) {
    let msg = rights(&[x as i32; SCM_MAX_FD]);
    for _ in 0..n {
        assert_eq!(p.sendmsg(a, b"f", &msg, None, 0), 1);
    }
}

#[test]
fn a_drained_burst_never_holds_off_the_send_trigger() {
    let fs = ns();
    let p = Prog::new(&fs, 80, 4321);
    p.fds.set_limit(1 << 16);
    let (x, y) = p.pair(SOCK_DGRAM);
    // A burst of 13 * 253 files in flight past SANE_USER (2024), then all gone again (the receiver closed).
    let (a, b) = p.pair(SOCK_STREAM);
    flood(&p, a, x, 13);
    p.close(a);
    p.close(b);
    // The same user later builds a cycle (b2 travels in its own queue, its fd open) and has 2024 files in
    // flight again: its next send of sockets runs a collection (`unix_schedule_gc(user)`).
    let (a2, b2) = p.pair(SOCK_STREAM);
    assert_eq!(p.sendmsg(a2, b"c", &rights(&[b2 as i32]), None, 0), 1);
    let passes = super::unix_gc::runs();
    flood(&p, a2, x, 8);
    let before = super::unix_gc::send_runs();
    flood(&p, a2, x, 1);
    // A pass of another test since the cycle was made also resets the rate limit (fewer than TRIGGER_STEP
    // files since it): then that pass was the collection. Alone, only the send can have run one.
    assert!(
        super::unix_gc::send_runs() > before || super::unix_gc::runs() > passes,
        "a send past SANE_USER with a cycle in the graph collects"
    );
    let wb = Arc::downgrade(&p.fds.get(b2 as usize).unwrap());
    for fd in [a2, b2, x, y] {
        p.close(fd);
    }
    assert!(collected(&p, &[wb]));
}
