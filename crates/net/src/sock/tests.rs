//! Host tests of the socket layer through its syscalls (`sys`) on a fake program memory: the Linux behaviour
//! each case pins down is named in the assertion text.

use super::abi::*;
use super::sys;
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::Mutex as StdMutex;
use uapi::errno::*;
use vfs::sys::{Sys, UserMem};
use vfs::{Cred, Errno, FdTable, MountNs};

pub(crate) const BASE: u64 = 0x1000_0000;
const SIZE: usize = 1 << 20;

/// A program's memory: one block at `BASE`; everything else faults.
pub(crate) struct Mem(pub StdMutex<Vec<u8>>);

impl UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let m = self.0.lock().unwrap();
        let Some(off) = addr.checked_sub(BASE) else { return false };
        let off = off as usize;
        if off.checked_add(buf.len()).is_none_or(|e| e > m.len()) {
            return false;
        }
        buf.copy_from_slice(&m[off..off + buf.len()]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut m = self.0.lock().unwrap();
        let Some(off) = addr.checked_sub(BASE) else { return false };
        let off = off as usize;
        if off.checked_add(data.len()).is_none_or(|e| e > m.len()) {
            return false;
        }
        m[off..off + data.len()].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let mut v = Vec::new();
        for i in 0..max as u64 {
            let mut b = [0u8];
            if !self.read(addr + i, &mut b) {
                return Err(EFAULT);
            }
            if b[0] == 0 {
                return Ok(v);
            }
            v.push(b[0]);
        }
        Err(ENAMETOOLONG)
    }
}

/// One fake program: memory, fd table, the shared mount view, and a bump allocator in its memory.
pub(crate) struct Prog {
    pub mem: Mem,
    pub fds: FdTable,
    pub ns: Arc<MountNs>,
    pub cred: Cred,
    pub pid: u32,
    next: StdMutex<u64>,
}

impl Prog {
    pub fn new(ns: &Arc<MountNs>, pid: u32, uid: u32) -> Prog {
        Prog { mem: Mem(StdMutex::new(alloc::vec![0; SIZE])), fds: FdTable::new(), ns: ns.clone(), cred: Cred { uid, gid: uid }, pid, next: StdMutex::new(BASE) }
    }
    pub fn sys(&self) -> Sys<'_> {
        Sys { mem: &self.mem, fds: &self.fds, ns: &self.ns, cred: self.cred, subject: policy::Subject { program: policy::ProgramId(1), pid: self.pid, uid: self.cred.uid } }
    }
    pub fn call(&self, f: vfs::sys::SysFn, a: [u64; 6]) -> i64 {
        f(&self.sys(), a)
    }
    /// Puts `data` into the program's memory; returns its address (wraps around at the end of the block).
    pub fn put(&self, data: &[u8]) -> u64 {
        let mut n = self.next.lock().unwrap();
        if *n + data.len() as u64 + 16 > BASE + SIZE as u64 {
            *n = BASE;
        }
        let at = *n;
        *n += (data.len() as u64 + 15) & !15;
        assert!(self.mem.write(at, data));
        at
    }
    pub fn buf(&self, n: usize) -> u64 {
        self.put(&alloc::vec![0u8; n])
    }
    pub fn get(&self, at: u64, n: usize) -> Vec<u8> {
        let mut v = alloc::vec![0u8; n];
        assert!(self.mem.read(at, &mut v));
        v
    }
    pub fn u32_at(&self, at: u64) -> u32 {
        u32::from_le_bytes(self.get(at, 4).try_into().unwrap())
    }

    pub fn socket(&self, fam: u16, ty: u32) -> i64 {
        self.call(sys::socket, [fam as u64, ty as u64, 0, 0, 0, 0])
    }
    pub fn pair(&self, ty: u32) -> (u64, u64) {
        let sv = self.buf(8);
        assert_eq!(self.call(sys::socketpair, [AF_UNIX as u64, ty as u64, 0, sv, 0, 0]), 0, "socketpair");
        (self.u32_at(sv) as u64, self.u32_at(sv + 4) as u64)
    }
    pub fn send(&self, fd: u64, data: &[u8], flags: u32) -> i64 {
        let at = self.put(data);
        self.call(sys::sendto, [fd, at, data.len() as u64, flags as u64, 0, 0])
    }
    pub fn sendto(&self, fd: u64, data: &[u8], to: &[u8]) -> i64 {
        let at = self.put(data);
        let a = self.put(to);
        self.call(sys::sendto, [fd, at, data.len() as u64, 0, a, to.len() as u64])
    }
    /// recv into a fresh buffer of `n`: (result, bytes).
    pub fn recv(&self, fd: u64, n: usize, flags: u32) -> (i64, Vec<u8>) {
        let b = self.buf(n);
        let r = self.call(sys::recvfrom, [fd, b, n as u64, flags as u64, 0, 0]);
        (r, if r > 0 { self.get(b, (r as usize).min(n)) } else { Vec::new() })
    }
    pub fn recvfrom(&self, fd: u64, n: usize) -> (i64, Vec<u8>, Vec<u8>) {
        let b = self.buf(n);
        let a = self.buf(128);
        let l = self.put(&128u32.to_le_bytes());
        let r = self.call(sys::recvfrom, [fd, b, n as u64, 0, a, l]);
        let alen = self.u32_at(l) as usize;
        (r, if r > 0 { self.get(b, (r as usize).min(n)) } else { Vec::new() }, self.get(a, alen.min(128)))
    }
    pub fn getname(&self, fd: u64, peer: bool) -> Result<Vec<u8>, i64> {
        let a = self.buf(128);
        let l = self.put(&128u32.to_le_bytes());
        let r = self.call(if peer { sys::getpeername } else { sys::getsockname }, [fd, a, l, 0, 0, 0]);
        if r < 0 {
            return Err(r);
        }
        Ok(self.get(a, self.u32_at(l) as usize))
    }
    pub fn setopt_int(&self, fd: u64, level: u32, name: u32, v: i32) -> i64 {
        let a = self.put(&v.to_le_bytes());
        self.call(sys::setsockopt, [fd, level as u64, name as u64, a, 4, 0])
    }
    pub fn getopt(&self, fd: u64, level: u32, name: u32, len: u32) -> Result<Vec<u8>, i64> {
        let a = self.buf(len as usize);
        let l = self.put(&len.to_le_bytes());
        let r = self.call(sys::getsockopt, [fd, level as u64, name as u64, a, l, 0]);
        if r < 0 {
            return Err(r);
        }
        Ok(self.get(a, self.u32_at(l) as usize))
    }
    pub fn getopt_int(&self, fd: u64, level: u32, name: u32) -> i64 {
        match self.getopt(fd, level, name, 4) {
            Ok(v) => i32::from_le_bytes(v[..4].try_into().unwrap()) as i64,
            Err(e) => e,
        }
    }
    pub fn close(&self, fd: u64) -> i64 {
        self.fds.close(fd as usize).map(|_| 0).unwrap_or_else(|e| -e)
    }
    pub fn events(&self, fd: u64) -> u32 {
        self.fds.get(fd as usize).unwrap().file.events().0
    }
    pub fn set_nonblock(&self, fd: u64) {
        let f = self.fds.get(fd as usize).unwrap();
        f.set_status_flags(f.status_flags() | vfs::abi::O_NONBLOCK);
    }

    /// sendmsg with one data segment and control data: the result.
    pub fn sendmsg(&self, fd: u64, data: &[u8], ctl: &[u8], to: Option<&[u8]>, flags: u32) -> i64 {
        let d = self.put(data);
        let iov = self.put(&[d.to_le_bytes(), (data.len() as u64).to_le_bytes()].concat());
        let c = if ctl.is_empty() { 0 } else { self.put(ctl) };
        let (n, nl) = match to {
            Some(t) => (self.put(t), t.len() as u64),
            None => (0, 0),
        };
        let mut h = Vec::new();
        h.extend_from_slice(&n.to_le_bytes());
        h.extend_from_slice(&nl.to_le_bytes());
        h.extend_from_slice(&iov.to_le_bytes());
        h.extend_from_slice(&1u64.to_le_bytes());
        h.extend_from_slice(&c.to_le_bytes());
        h.extend_from_slice(&(ctl.len() as u64).to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        let m = self.put(&h);
        self.call(sys::sendmsg, [fd, m, flags as u64, 0, 0, 0])
    }

    /// recvmsg with one buffer of `n` and `ctl_len` bytes of control space: (result, data, control, flags).
    pub fn recvmsg(&self, fd: u64, n: usize, ctl_len: usize, flags: u32) -> (i64, Vec<u8>, Vec<u8>, u32) {
        let d = self.buf(n);
        let iov = self.put(&[d.to_le_bytes(), (n as u64).to_le_bytes()].concat());
        let c = if ctl_len == 0 { 0 } else { self.buf(ctl_len) };
        let mut h = Vec::new();
        h.extend_from_slice(&0u64.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        h.extend_from_slice(&iov.to_le_bytes());
        h.extend_from_slice(&1u64.to_le_bytes());
        h.extend_from_slice(&c.to_le_bytes());
        h.extend_from_slice(&(ctl_len as u64).to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        let m = self.put(&h);
        let r = self.call(sys::recvmsg, [fd, m, flags as u64, 0, 0, 0]);
        let used = u64::from_le_bytes(self.get(m + 40, 8).try_into().unwrap()) as usize;
        let fl = self.u32_at(m + 48);
        (r, if r > 0 { self.get(d, (r as usize).min(n)) } else { Vec::new() }, if used > 0 { self.get(c, used) } else { Vec::new() }, fl)
    }
}

pub(crate) fn sun(path: &[u8]) -> Vec<u8> {
    let mut v = AF_UNIX.to_le_bytes().to_vec();
    v.extend_from_slice(path);
    v
}

pub(crate) fn rights(fds: &[i32]) -> Vec<u8> {
    let data: Vec<u8> = fds.iter().flat_map(|f| f.to_le_bytes()).collect();
    let mut v = (cmsg_len(data.len()) as u64).to_le_bytes().to_vec();
    v.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    v.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
    v.extend_from_slice(&data);
    v.resize(cmsg_space(data.len()), 0);
    v
}

/// NW-fl: runs `f` (a call on `fd`) in another thread and returns once that call holds `fd`'s open file
/// (Linux `fdget`; bounded wait, no sleep decides it): from then on what the test does to the fd reaches the
/// call. The short pause after that only makes "already asleep" the usual case; no result depends on it.
pub(crate) fn in_call<R: Send + 'static>(p: &Arc<Prog>, fd: u64, f: impl FnOnce(&Prog) -> R + Send + 'static) -> std::thread::JoinHandle<R> {
    let file = p.fds.get(fd as usize).expect("open fd");
    let base = Arc::strong_count(&file);
    let q = p.clone();
    let t = std::thread::spawn(move || f(&q));
    let t0 = std::time::Instant::now();
    while Arc::strong_count(&file) <= base {
        assert!(t0.elapsed() < std::time::Duration::from_secs(20), "the call never took fd {fd}");
        std::thread::yield_now();
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
    t
}

pub(crate) fn ns() -> Arc<MountNs> {
    klog::set_serial(false);
    let ns = Arc::new(MountNs::new_empty());
    let d = ns.mkdir_p("/tmp").unwrap();
    d.inode.meta.set_perm(0o1777);
    ns
}

use vfs::Events as E;

#[test]
fn creation_errors_like_linux() {
    let ns = ns();
    let p = Prog::new(&ns, 10, 1000);
    assert_eq!(p.socket(AF_UNIX, 99), -EINVAL, "type above SOCK_MAX");
    assert_eq!(p.call(sys::socket, [AF_UNIX as u64, 1 | 0x100, 0, 0, 0, 0]), -EINVAL, "unknown flag bit");
    assert_eq!(p.socket(AF_UNIX, 4), -ESOCKTNOSUPPORT, "SOCK_RDM");
    assert_eq!(p.call(sys::socket, [AF_UNIX as u64, 1, 7, 0, 0, 0]), -EPROTONOSUPPORT);
    let v6 = p.socket(AF_INET6, SOCK_STREAM);
    assert!(v6 >= 0, "AF_INET6 since NW-v6");
    p.close(v6 as u64);
    assert_eq!(p.socket(AF_INET6, SOCK_SEQPACKET), -ESOCKTNOSUPPORT);
    assert_eq!(p.socket(46, SOCK_STREAM), -EAFNOSUPPORT, "beyond NPROTO");
    let fd = p.socket(AF_UNIX, SOCK_RAW);
    assert!(fd >= 0);
    assert_eq!(p.getopt_int(fd as u64, SOL_SOCKET, SO_TYPE), SOCK_DGRAM as i64, "AF_UNIX SOCK_RAW is a datagram socket");
    assert_eq!(p.getopt_int(fd as u64, SOL_SOCKET, SO_DOMAIN), AF_UNIX as i64);
    let c = p.call(sys::socket, [AF_UNIX as u64, (SOCK_STREAM as u64) | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, 0, 0, 0]);
    assert!(p.fds.cloexec(c as usize).unwrap());
    assert!(p.fds.get(c as usize).unwrap().nonblocking());
    let st = p.fds.get(c as usize).unwrap().stat();
    assert_eq!(st.mode, vfs::abi::S_IFSOCK | 0o777);
    assert_eq!(p.call(sys::bind, [1234, 0, 0, 0, 0, 0]), -EBADF);
    ns.create_file("/tmp/plain", 0o644, Vec::new()).unwrap();
    let file = vfs::Ctx::kernel(&ns).open(None, b"/tmp/plain", vfs::abi::O_RDONLY, 0).unwrap();
    let ffd = p.fds.install(file).unwrap();
    assert_eq!(p.call(sys::listen, [ffd as u64, 1, 0, 0, 0, 0]), -ENOTSOCK);
}

#[test]
fn stream_pair_data_shutdown_and_close() {
    let ns = ns();
    let p = Prog::new(&ns, 11, 1000);
    let (a, b) = p.pair(SOCK_STREAM);
    assert_eq!(p.events(a) & (E::OUT | E::IN), E::OUT, "fresh pair: writable, nothing to read");
    assert_eq!(p.send(a, b"hello", 0), 5);
    assert_eq!(p.send(a, b" world", 0), 6);
    assert!(p.events(b) & E::IN != 0);
    assert_eq!(p.recv(b, 3, MSG_PEEK), (3, b"hel".to_vec()), "peek");
    assert_eq!(p.recv(b, 100, 0), (11, b"hello world".to_vec()), "stream reads glue the two writes");
    p.set_nonblock(b);
    assert_eq!(p.recv(b, 10, 0).0, -EAGAIN);
    assert_eq!(p.getname(a, true).unwrap(), AF_UNIX.to_le_bytes(), "unbound peer: the family alone");
    let cred = p.getopt(a, SOL_SOCKET, SO_PEERCRED, 12).unwrap();
    assert_eq!(cred, super::Ucred { pid: 11, uid: 1000, gid: 1000 }.bytes(), "socketpair: the creator's credentials");
    assert_eq!(p.call(sys::shutdown, [a, SHUT_WR, 0, 0, 0, 0]), 0);
    assert_eq!(p.recv(b, 10, 0).0, 0, "peer's SHUT_WR is EOF here");
    assert!(p.events(b) & E::RDHUP != 0);
    assert_eq!(p.send(a, b"x", MSG_NOSIGNAL), -EPIPE, "write after SHUT_WR");
    assert_eq!(p.call(sys::shutdown, [a, 3, 0, 0, 0, 0]), -EINVAL);
    assert_eq!(p.send(b, b"late", 0), 4, "the other direction still works");
    assert_eq!(p.recv(a, 10, 0), (4, b"late".to_vec()));
    assert_eq!(p.close(a), 0);
    assert_eq!(p.recv(b, 10, 0).0, 0, "closed peer: EOF");
    assert_eq!(p.events(b) & E::HUP, E::HUP);
    assert_eq!(p.send(b, b"x", MSG_NOSIGNAL), -EPIPE);
}

#[test]
fn unread_data_at_close_is_a_reset() {
    let ns = ns();
    let p = Prog::new(&ns, 12, 1000);
    let (a, b) = p.pair(SOCK_STREAM);
    assert_eq!(p.send(b, b"never read", 0), 10);
    p.close(a);
    assert_eq!(p.recv(b, 10, MSG_DONTWAIT).0, -ECONNRESET, "close with unread data: ECONNRESET once");
    assert_eq!(p.recv(b, 10, MSG_DONTWAIT).0, 0, "then EOF");
}

#[test]
fn seqpacket_and_dgram_keep_boundaries() {
    let ns = ns();
    let p = Prog::new(&ns, 13, 1000);
    for ty in [SOCK_SEQPACKET, SOCK_DGRAM] {
        let (a, b) = p.pair(ty);
        assert_eq!(p.send(a, b"one", 0), 3);
        assert_eq!(p.send(a, b"second", 0), 6);
        let (r, d, _, fl) = p.recvmsg(b, 2, 0, 0);
        assert_eq!((r, d.as_slice()), (2, &b"on"[..]), "a short buffer cuts the message");
        assert!(fl & MSG_TRUNC != 0, "with MSG_TRUNC in msg_flags");
        assert_eq!(p.recv(b, 1, MSG_TRUNC).0, 6, "MSG_TRUNC returns the real length");
        assert_eq!(p.recv(b, 1, MSG_DONTWAIT).0, -EAGAIN);
        assert_eq!(p.send(a, b"", 0), 0, "empty datagram");
        assert_eq!(p.recv(b, 10, MSG_DONTWAIT).0, 0);
        p.close(a);
        let r = p.recv(b, 1, MSG_DONTWAIT).0;
        if ty == SOCK_SEQPACKET {
            assert_eq!(r, 0, "seqpacket: a closed peer is EOF even non-blocking");
            assert_eq!(p.send(b, b"x", 0), -EPIPE, "seqpacket to a closed peer: EPIPE");
        } else {
            assert_eq!(r, -EAGAIN, "datagram: EAGAIN non-blocking");
        }
    }
}

#[test]
fn abstract_names_and_datagram_addresses() {
    let ns = ns();
    let p = Prog::new(&ns, 14, 1000);
    let srv = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    let cli = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    let name = sun(b"\0bits-test-dgram");
    let a = p.put(&name);
    assert_eq!(p.call(sys::bind, [srv, a, name.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::bind, [srv, a, name.len() as u64, 0, 0, 0]), -EINVAL, "bound twice");
    let other = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    assert_eq!(p.call(sys::bind, [other, a, name.len() as u64, 0, 0, 0]), -EADDRINUSE);
    let st = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(p.call(sys::bind, [st, a, name.len() as u64, 0, 0, 0]), 0, "abstract names are per socket type");
    assert_eq!(p.getname(srv, false).unwrap(), name);
    // Autobind: the family alone.
    let fam = p.put(&AF_UNIX.to_le_bytes());
    assert_eq!(p.call(sys::bind, [cli, fam, 2, 0, 0, 0]), 0);
    let auto = p.getname(cli, false).unwrap();
    assert_eq!(auto.len(), 8);
    assert_eq!(auto[2], 0);
    assert!(auto[3..].iter().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(p.sendto(cli, b"ping", &name), 4);
    let (r, d, from) = p.recvfrom(srv, 16);
    assert_eq!((r, d.as_slice()), (4, &b"ping"[..]));
    assert_eq!(from, auto, "recvfrom names the sender");
    assert_eq!(p.sendto(cli, b"x", &sun(b"\0nobody")), -ECONNREFUSED);
    assert_eq!(p.send(cli, b"x", 0), -ENOTCONN, "unconnected without address");
    // A datagram socket connected to someone else takes nothing from us.
    let c2 = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    let an = p.put(&auto);
    assert_eq!(p.call(sys::connect, [srv, an, auto.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.sendto(c2, b"x", &name), -EPERM, "receiver connected elsewhere");
    assert_eq!(p.sendto(cli, b"ok", &name), 2, "its own peer may");
    // AF_UNSPEC disconnects.
    let unspec = p.put(&0u16.to_le_bytes());
    assert_eq!(p.call(sys::connect, [srv, unspec, 2, 0, 0, 0]), 0);
    assert_eq!(p.getname(srv, true), Err(-ENOTCONN));
    assert_eq!(p.sendto(c2, b"x", &name), 1);
}

#[test]
fn path_names_listen_connect_accept() {
    let ns = ns();
    let p = Prog::new(&ns, 15, 1000);
    let srv = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let path = b"/tmp/wayland-0\0";
    let name = sun(path);
    let a = p.put(&name);
    assert_eq!(p.call(sys::listen, [srv, 5, 0, 0, 0, 0]), -EINVAL, "listen unbound");
    assert_eq!(p.call(sys::bind, [srv, a, name.len() as u64, 0, 0, 0]), 0);
    let at = ns.lookup("/tmp/wayland-0").expect("the socket file exists");
    assert_eq!(at.inode.meta.mode(), vfs::abi::S_IFSOCK | 0o755, "mode 0777 minus the umask 022");
    let again = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(p.call(sys::bind, [again, a, name.len() as u64, 0, 0, 0]), -EADDRINUSE, "the path exists");
    let cli = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(p.call(sys::connect, [cli, a, name.len() as u64, 0, 0, 0]), -ECONNREFUSED, "not listening");
    assert_eq!(p.call(sys::listen, [srv, 0, 0, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::connect, [cli, a, name.len() as u64, 0, 0, 0]), 0);
    // Backlog 0 holds one connection (full means queue length > backlog): the next is EAGAIN non-blocking.
    let c2 = p.call(sys::socket, [AF_UNIX as u64, (SOCK_STREAM as u64) | SOCK_NONBLOCK, 0, 0, 0, 0]) as u64;
    assert_eq!(p.call(sys::connect, [c2, a, name.len() as u64, 0, 0, 0]), -EAGAIN);
    assert_eq!(p.send(cli, b"before accept", 0), 13, "data before accept waits in the embryo");
    let pa = p.buf(128);
    let pl = p.put(&128u32.to_le_bytes());
    let s = p.call(sys::accept4, [srv, pa, pl, SOCK_CLOEXEC, 0, 0]);
    assert!(s >= 0);
    let s = s as u64;
    assert!(p.fds.cloexec(s as usize).unwrap());
    assert_eq!(p.u32_at(pl), 2, "the client is unbound");
    assert_eq!(p.call(sys::connect, [cli, a, name.len() as u64, 0, 0, 0]), -EISCONN, "after the target checks, our own state");
    assert_eq!(p.recv(s, 64, 0), (13, b"before accept".to_vec()));
    assert_eq!(p.getname(s, false).unwrap(), name, "accepted socket carries the listener's name");
    assert_eq!(p.getname(cli, true).unwrap(), name);
    let cred = p.getopt(s, SOL_SOCKET, SO_PEERCRED, 12).unwrap();
    assert_eq!(&cred[0..4], &15u32.to_le_bytes());
    assert_eq!(p.getopt_int(srv, SOL_SOCKET, SO_ACCEPTCONN), 1);
    p.set_nonblock(srv);
    assert_eq!(p.call(sys::accept, [srv, 0, 0, 0, 0, 0]), -EAGAIN);
    let dg = p.socket(AF_UNIX, SOCK_DGRAM) as u64;
    assert_eq!(p.call(sys::connect, [dg, a, name.len() as u64, 0, 0, 0]), -EPROTOTYPE, "a stream socket's path from a datagram socket");
    // The file stays after close; connecting then is refused.
    p.close(srv);
    let late = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(p.call(sys::connect, [late, a, name.len() as u64, 0, 0, 0]), -ECONNREFUSED);
    let missing = sun(b"/tmp/nothing-here\0");
    let m = p.put(&missing);
    assert_eq!(p.call(sys::connect, [late, m, missing.len() as u64, 0, 0, 0]), -ENOENT);
}

#[test]
fn scm_rights_pass_files_and_truncate() {
    let ns = ns();
    let p = Prog::new(&ns, 16, 1000);
    let q = Prog::new(&ns, 17, 1000);
    let (a, b) = p.pair(SOCK_STREAM);
    // Hand b to the other program (as a fork would).
    let bf = p.fds.get(b as usize).unwrap();
    let qb = q.fds.install(bf).unwrap() as u64;
    p.close(b);
    ns.create_file("/tmp/payload", 0o644, b"memfd-like content".to_vec()).unwrap();
    let file = vfs::Ctx::kernel(&ns).open(None, b"/tmp/payload", vfs::abi::O_RDONLY, 0).unwrap();
    let ffd = p.fds.install(file.clone()).unwrap() as i32;
    let before = Arc::strong_count(&file);
    assert_eq!(p.sendmsg(a, b"F", &rights(&[ffd, ffd]), None, 0), 1);
    p.close(ffd as u64);
    assert_eq!(Arc::strong_count(&file), before + 1, "two in flight, the sender's fd closed");
    let (r, d, ctl, fl) = q.recvmsg(qb, 8, cmsg_space(8), MSG_CMSG_CLOEXEC);
    assert_eq!((r, d.as_slice(), fl & MSG_CTRUNC), (1, &b"F"[..], 0));
    assert_eq!(ctl.len(), cmsg_space(8));
    assert_eq!(u64::from_le_bytes(ctl[0..8].try_into().unwrap()), cmsg_len(8) as u64);
    let n1 = u32::from_le_bytes(ctl[16..20].try_into().unwrap()) as usize;
    let n2 = u32::from_le_bytes(ctl[20..24].try_into().unwrap()) as usize;
    let got = q.fds.get(n1).unwrap();
    assert!(Arc::ptr_eq(&got, &file), "the same open file description arrives");
    assert!(q.fds.cloexec(n2).unwrap(), "MSG_CMSG_CLOEXEC");
    let mut buf = [0u8; 9];
    assert_eq!(got.read(&mut buf).unwrap(), 9);
    // Room for one fd only: one installed, the other closed, MSG_CTRUNC.
    let ffd = p.fds.install(file.clone()).unwrap() as i32;
    assert_eq!(p.sendmsg(a, b"G", &rights(&[ffd, ffd]), None, 0), 1);
    let n_before = q.fds.count();
    let (r, _, ctl, fl) = q.recvmsg(qb, 8, cmsg_len(4), 0);
    assert_eq!(r, 1);
    assert!(fl & MSG_CTRUNC != 0);
    assert_eq!(u64::from_le_bytes(ctl[0..8].try_into().unwrap()), cmsg_len(4) as u64);
    assert_eq!(q.fds.count(), n_before + 1);
    // No control buffer at all: MSG_CTRUNC, files closed.
    assert_eq!(p.sendmsg(a, b"H", &rights(&[ffd]), None, 0), 1);
    let (r, d, _, fl) = q.recvmsg(qb, 8, 0, 0);
    assert!(fl & MSG_CTRUNC != 0, "{r} {d:?} {fl:#x}");
    // Closing the receiver with files in its queue closes them ("closing in flight").
    let held = Arc::strong_count(&file);
    assert_eq!(p.sendmsg(a, b"I", &rights(&[ffd]), None, 0), 1);
    assert_eq!(Arc::strong_count(&file), held + 1);
    q.close(qb);
    assert_eq!(Arc::strong_count(&file), held, "purged with the receiver");
    assert_eq!(p.sendmsg(a, b"J", &rights(&[99]), None, 0), -EBADF);
}

#[test]
fn scm_credentials_and_passcred() {
    let ns = ns();
    let p = Prog::new(&ns, 18, 1000);
    let (a, b) = p.pair(SOCK_DGRAM);
    assert_eq!(p.setopt_int(b, SOL_SOCKET, SO_PASSCRED, 1), 0);
    assert_eq!(p.getopt_int(b, SOL_SOCKET, SO_PASSCRED), 1);
    assert_eq!(p.send(a, b"c", 0), 1);
    let (r, _, ctl, _) = p.recvmsg(b, 4, cmsg_space(12), 0);
    assert_eq!(r, 1);
    assert_eq!(ctl.len(), cmsg_space(12));
    assert_eq!(&ctl[8..16], &[1, 0, 0, 0, 2, 0, 0, 0], "SOL_SOCKET, SCM_CREDENTIALS");
    assert_eq!(&ctl[16..28], &super::Ucred { pid: 18, uid: 1000, gid: 1000 }.bytes());
    let mut creds = (cmsg_len(12) as u64).to_le_bytes().to_vec();
    creds.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    creds.extend_from_slice(&SCM_CREDENTIALS.to_le_bytes());
    creds.extend_from_slice(&super::Ucred { pid: 18, uid: 0, gid: 1000 }.bytes());
    creds.resize(cmsg_space(12), 0);
    assert_eq!(p.sendmsg(a, b"x", &creds, None, 0), -EPERM, "another uid without root");
    // A receiver without SO_PASSCRED gets no credentials.
    let (c, d) = p.pair(SOCK_DGRAM);
    p.send(c, b"y", 0);
    let (_, _, ctl, fl) = p.recvmsg(d, 4, 64, 0);
    assert!(ctl.is_empty() && fl & MSG_CTRUNC == 0);
}

#[test]
fn cycles_in_flight_are_collected() {
    let ns = ns();
    let p = Prog::new(&ns, 19, 1000);
    let (a, b) = p.pair(SOCK_STREAM);
    let wa = Arc::downgrade(&p.fds.get(a as usize).unwrap());
    let wb = Arc::downgrade(&p.fds.get(b as usize).unwrap());
    // Each end travels over itself: after closing both fds nothing reaches them but their own queues.
    assert_eq!(p.sendmsg(a, b"1", &rights(&[b as i32]), None, 0), 1);
    assert_eq!(p.sendmsg(b, b"2", &rights(&[a as i32]), None, 0), 1);
    p.close(a);
    p.close(b);
    assert!(wa.strong_count() > 0 && wb.strong_count() > 0, "kept alive by the cycle");
    // Any unix socket's release runs the collector (Linux's trigger).
    let (x, y) = p.pair(SOCK_DGRAM);
    p.close(x);
    p.close(y);
    // Another test's collection may be running on another thread; it makes one more pass for us.
    for _ in 0..100 {
        if wa.strong_count() == 0 && wb.strong_count() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(wa.strong_count() == 0 && wb.strong_count() == 0, "collected");
}

#[test]
fn blocking_read_is_woken_by_another_thread() {
    let ns = ns();
    let p = Arc::new(Prog::new(&ns, 20, 1000));
    let (a, b) = p.pair(SOCK_STREAM);
    let t = in_call(&p, b, move |q| q.recv(b, 16, 0));
    assert_eq!(p.send(a, b"wake", 0), 4);
    assert_eq!(t.join().unwrap(), (4, b"wake".to_vec()));
    // A full sender (SO_SNDBUF small) waits until the reader takes data.
    p.setopt_int(a, SOL_SOCKET, SO_SNDBUF, 4096);
    let big = alloc::vec![7u8; 64 * 1024];
    let q = p.clone();
    let w = std::thread::spawn(move || q.send(a, &big, 0));
    let mut got = 0usize;
    while got < 64 * 1024 {
        let (r, _) = p.recv(b, 16 * 1024, 0);
        assert!(r > 0);
        got += r as usize;
    }
    assert_eq!(w.join().unwrap(), 64 * 1024);
    p.set_nonblock(a);
    let mut sent = 0;
    loop {
        let r = p.send(a, &[1u8; 1024], 0);
        if r < 0 {
            assert_eq!(r, -EAGAIN, "full and non-blocking");
            break;
        }
        sent += r;
    }
    assert!(sent > 0);
    assert_eq!(p.events(a) & E::OUT, 0, "not writable while full");
}

#[test]
fn options_like_linux() {
    let ns = ns();
    let p = Prog::new(&ns, 21, 1000);
    let fd = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(p.getopt_int(fd, SOL_SOCKET, SO_SNDBUF), WMEM_DEFAULT as i64);
    assert_eq!(p.setopt_int(fd, SOL_SOCKET, SO_SNDBUF, 1000), 0);
    assert_eq!(p.getopt_int(fd, SOL_SOCKET, SO_SNDBUF), SOCK_MIN_SNDBUF as i64, "doubled, at least SOCK_MIN_SNDBUF");
    assert_eq!(p.setopt_int(fd, SOL_SOCKET, SO_RCVBUF, 100_000), 0);
    assert_eq!(p.getopt_int(fd, SOL_SOCKET, SO_RCVBUF), 200_000);
    assert_eq!(p.setopt_int(fd, SOL_SOCKET, SO_RCVBUF, -1), 0);
    assert_eq!(p.getopt_int(fd, SOL_SOCKET, SO_RCVBUF), (RMEM_MAX * 2) as i64, "negative is huge, clamped to rmem_max");
    assert_eq!(p.setopt_int(fd, SOL_SOCKET, SO_TYPE, 1), -ENOPROTOOPT);
    assert_eq!(p.setopt_int(fd, SOL_SOCKET, 999, 1), -ENOPROTOOPT);
    assert_eq!(p.setopt_int(fd, SOL_TCP, TCP_NODELAY, 1), -EOPNOTSUPP, "AF_UNIX has no other levels");
    let a = p.put(&1i32.to_le_bytes());
    assert_eq!(p.call(sys::setsockopt, [fd, SOL_SOCKET as u64, SO_REUSEADDR as u64, a, 2, 0]), -EINVAL, "optlen below an int");
    assert_eq!(p.call(sys::setsockopt, [fd, SOL_SOCKET as u64, SO_REUSEADDR as u64, a, u32::MAX as u64, 0]), -EINVAL);
    let tv = p.put(&[1i64.to_le_bytes(), 1500i64.to_le_bytes()].concat());
    assert_eq!(p.call(sys::setsockopt, [fd, SOL_SOCKET as u64, SO_RCVTIMEO_NEW as u64, tv, 16, 0]), 0);
    let got = p.getopt(fd, SOL_SOCKET, SO_RCVTIMEO_NEW, 16).unwrap();
    assert_eq!(got, [1i64.to_le_bytes(), 2000i64.to_le_bytes()].concat(), "microseconds rounded up to the 1 ms tick");
    let bad = p.put(&[0i64.to_le_bytes(), 1_000_000i64.to_le_bytes()].concat());
    assert_eq!(p.call(sys::setsockopt, [fd, SOL_SOCKET as u64, SO_RCVTIMEO_NEW as u64, bad, 16, 0]), -EDOM);
    assert_eq!(p.getopt(fd, SOL_SOCKET, SO_TYPE, 2).unwrap(), 1u16.to_le_bytes(), "a short buffer gets the start");
    assert_eq!(p.getopt_int(fd, SOL_SOCKET, 999), -ENOPROTOOPT);
    assert_eq!(p.getopt(fd, SOL_SOCKET, SO_PEERCRED, 12).unwrap(), super::Ucred { pid: 0, uid: u32::MAX, gid: u32::MAX }.bytes());
}

#[test]
fn recv_timeout_gives_eagain() {
    let ns = ns();
    let p = Prog::new(&ns, 22, 1000);
    let (_a, b) = p.pair(SOCK_DGRAM);
    let tv = p.put(&[0i64.to_le_bytes(), 30_000i64.to_le_bytes()].concat());
    assert_eq!(p.call(sys::setsockopt, [b, SOL_SOCKET as u64, SO_RCVTIMEO_NEW as u64, tv, 16, 0]), 0);
    let t = std::time::Instant::now();
    assert_eq!(p.recv(b, 4, 0).0, -EAGAIN);
    assert!(t.elapsed() >= std::time::Duration::from_millis(25));
}

#[test]
fn writev_is_one_datagram() {
    let ns = ns();
    let p = Prog::new(&ns, 23, 1000);
    let (a, b) = p.pair(SOCK_DGRAM);
    let x = p.put(b"abc");
    let y = p.put(b"defg");
    let iov = p.put(&[x.to_le_bytes(), 3u64.to_le_bytes(), y.to_le_bytes(), 4u64.to_le_bytes()].concat());
    assert_eq!(p.call(sys::writev, [a, iov, 2, 0, 0, 0]), 7);
    assert_eq!(p.recv(b, 16, 0), (7, b"abcdefg".to_vec()), "one datagram");
    let big = alloc::vec![1u8; 100_000];
    let at = p.put(&big);
    assert_eq!(p.call(sys::write, [a, at, big.len() as u64, 0, 0, 0]), 100_000, "write(2) of 100 KB: one datagram");
    assert_eq!(p.recv(b, 200_000, 0).0, 100_000);
    let r1 = p.buf(2);
    let r2 = p.buf(8);
    p.send(a, b"xyzuvw", 0);
    let riov = p.put(&[r1.to_le_bytes(), 2u64.to_le_bytes(), r2.to_le_bytes(), 8u64.to_le_bytes()].concat());
    assert_eq!(p.call(sys::readv, [b, riov, 2, 0, 0, 0]), 6);
    assert_eq!(p.get(r1, 2), b"xy");
    assert_eq!(p.get(r2, 4), b"zuvw");
}

#[test]
fn peek_after_a_partial_read() {
    let ns = ns();
    let p = Prog::new(&ns, 24, 1000);
    let (a, b) = p.pair(SOCK_STREAM);
    p.send(a, b"abcdef", 0);
    p.send(a, b"gh", 0);
    assert_eq!(p.recv(b, 2, 0), (2, b"ab".to_vec()));
    assert_eq!(p.recv(b, 8, MSG_PEEK), (6, b"cdefgh".to_vec()), "peek starts after what was read, across writes");
    assert_eq!(p.recv(b, 8, 0), (6, b"cdefgh".to_vec()));
}

#[test]
fn a_long_chain_in_flight_closes_without_recursion() {
    // Each socket's only reference is the next one's queue (send b_k over pair k+1, close pair k's fds). Closing
    // the last fd releases the whole chain; by recursion that would need a stack frame per link (R2, R8).
    let ns = ns();
    let p = Arc::new(Prog::new(&ns, 40, 0));
    const LINKS: usize = 3000;
    let (mut pa, mut pb) = p.pair(SOCK_DGRAM);
    let first = Arc::downgrade(&p.fds.get(pb as usize).unwrap());
    for _ in 0..LINKS {
        let (a, b) = p.pair(SOCK_DGRAM);
        assert_eq!(p.sendmsg(a, b"x", &rights(&[pb as i32]), None, 0), 1);
        p.close(pa);
        p.close(pb);
        (pa, pb) = (a, b);
    }
    assert!(first.strong_count() > 0, "the chain holds its first link");
    // The closing thread gets a stack far smaller than the chain would need by recursion.
    let q = p.clone();
    let t = std::thread::Builder::new().stack_size(128 * 1024).spawn(move || {
        q.close(pa);
        q.close(pb);
    });
    t.unwrap().join().expect("closing the chain's head on a 128 KiB stack");
    assert_eq!(first.strong_count(), 0, "the whole chain went with its head");
}

#[test]
fn shutdown_and_close_against_blocked_calls() {
    let ns = ns();
    let p = Arc::new(Prog::new(&ns, 41, 1000));
    // A blocked recv ends with EOF when another thread shuts the socket's receive side (`unix_shutdown`).
    let (_a, b) = p.pair(SOCK_STREAM);
    let t = in_call(&p, b, move |q| q.recv(b, 16, 0).0);
    assert_eq!(p.call(sys::shutdown, [b, SHUT_RD, 0, 0, 0, 0]), 0);
    assert_eq!(t.join().unwrap(), 0, "recv after SHUT_RD: EOF");
    // close() of the fd does not end a call already blocked on it (the call holds the open file, as Linux's
    // fdget): the data sent afterwards arrives.
    let (c, d) = p.pair(SOCK_STREAM);
    let t = in_call(&p, d, move |q| q.recv(d, 16, 0));
    assert_eq!(p.close(d), 0);
    assert_eq!(p.send(c, b"late", 0), 4);
    assert_eq!(t.join().unwrap(), (4, b"late".to_vec()), "the blocked recv still gets the data");
    // A blocked accept ends with EINVAL on shutdown(SHUT_RDWR) of the listener (`unix_accept` after RCV_SHUTDOWN).
    let srv = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let name = sun(b"\0nw5-blocked-accept");
    let na = p.put(&name);
    assert_eq!(p.call(sys::bind, [srv, na, name.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::listen, [srv, 0, 0, 0, 0, 0]), 0);
    let t = in_call(&p, srv, move |q| q.call(sys::accept, [srv, 0, 0, 0, 0, 0]));
    assert_eq!(p.call(sys::shutdown, [srv, SHUT_RDWR, 0, 0, 0, 0]), 0);
    assert_eq!(t.join().unwrap(), -EINVAL);
    // A connect blocked on a full backlog is refused when the listener goes (`unix_wait_for_peer`, then the
    // lookup finds nobody): ECONNREFUSED. The socket can connect again afterwards.
    let srv = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let name = sun(b"\0nw5-full-backlog");
    let na = p.put(&name);
    assert_eq!(p.call(sys::bind, [srv, na, name.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::listen, [srv, 0, 0, 0, 0, 0]), 0);
    let first = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(p.call(sys::connect, [first, na, name.len() as u64, 0, 0, 0]), 0, "fills backlog 0");
    let waiting = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let nl = name.len() as u64;
    let t = in_call(&p, waiting, move |q| q.call(sys::connect, [waiting, na, nl, 0, 0, 0]));
    assert_eq!(p.close(srv), 0);
    assert_eq!(t.join().unwrap(), -ECONNREFUSED);
    let again = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    let name2 = sun(b"\0nw5-full-backlog-2");
    let n2 = p.put(&name2);
    assert_eq!(p.call(sys::bind, [again, n2, name2.len() as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::listen, [again, 1, 0, 0, 0, 0]), 0);
    assert_eq!(p.call(sys::connect, [waiting, n2, name2.len() as u64, 0, 0, 0]), 0, "the failed connect left no claim behind");
}

#[test]
fn ioctls_and_more_options_like_linux() {
    let ns = ns();
    let p = Prog::new(&ns, 42, 1000);
    let (a, b) = p.pair(SOCK_STREAM);
    let ioctl = |fd: u64, cmd: u64| -> Result<i32, i64> {
        let f = p.fds.get(fd as usize).unwrap();
        f.file.ioctl(cmd, 0).map(|v| i32::from_le_bytes(v[..4].try_into().unwrap()))
    };
    assert_eq!(ioctl(a, SIOCATMARK), Ok(0), "no out-of-band data: not at the mark");
    let (da, _db) = p.pair(SOCK_DGRAM);
    assert_eq!(ioctl(da, SIOCATMARK), Err(EOPNOTSUPP), "streams only (CachyOS)");
    assert_eq!(ioctl(a, SIOCGPGRP), Ok(0), "no owner");
    assert_eq!(ioctl(a, SIOCGSTAMP_OLD), Err(ENOTTY), "AF_UNIX has no gettstamp");
    assert_eq!(ioctl(a, 0x8915), Err(ENOTTY), "SIOCGIFADDR on AF_UNIX");
    // SO_TIMESTAMP*: the flags as __sock_set_timestamps leaves them.
    assert_eq!(p.setopt_int(b, SOL_SOCKET, SO_TIMESTAMPNS_OLD, 1), 0);
    assert_eq!(p.getopt_int(b, SOL_SOCKET, SO_TIMESTAMPNS_OLD), 1);
    assert_eq!(p.getopt_int(b, SOL_SOCKET, SO_TIMESTAMP_OLD), 0, "ns wins over plain");
    assert_eq!(p.setopt_int(b, SOL_SOCKET, SO_TIMESTAMP_OLD, 1), 0);
    assert_eq!((p.getopt_int(b, SOL_SOCKET, SO_TIMESTAMP_OLD), p.getopt_int(b, SOL_SOCKET, SO_TIMESTAMPNS_OLD)), (1, 0));
    assert_eq!(p.setopt_int(b, SOL_SOCKET, SO_TIMESTAMP_OLD, 0), 0);
    assert_eq!(p.getopt_int(b, SOL_SOCKET, SO_TIMESTAMP_OLD), 0);
    assert_eq!(p.setopt_int(b, SOL_SOCKET, SO_BUSY_POLL, -1), -EINVAL);
    assert_eq!(p.setopt_int(b, SOL_SOCKET, SO_BUSY_POLL, 50), 0);
    assert_eq!(p.getopt_int(b, SOL_SOCKET, SO_BUSY_POLL), 50);
    assert_eq!(p.getopt_int(b, SOL_SOCKET, SO_INCOMING_CPU), -1);
    assert_eq!(p.getopt(b, SOL_SOCKET, SO_BINDTODEVICE, 16), Ok(Vec::new()), "not bound: length 0");
    let empty = p.put(&[0u8; 4]);
    assert_eq!(p.call(sys::setsockopt, [b, SOL_SOCKET as u64, SO_BINDTODEVICE as u64, empty, 0, 0]), 0, "length 0 unbinds");
    let eth = p.put(b"nope9\0");
    assert_eq!(p.call(sys::setsockopt, [b, SOL_SOCKET as u64, SO_BINDTODEVICE as u64, eth, 6, 0]), -ENODEV, "no such interface");
    assert_eq!(p.call(sys::setsockopt, [b, SOL_SOCKET as u64, SO_BINDTODEVICE as u64, 8, 5, 0]), -EFAULT);
    assert_eq!(p.getopt(b, SOL_SOCKET, SO_PEERGROUPS, 64), Ok(Vec::new()), "a peer without supplementary groups");
    let lone = p.socket(AF_UNIX, SOCK_STREAM) as u64;
    assert_eq!(p.getopt(lone, SOL_SOCKET, SO_PEERGROUPS, 64), Err(-ENODATA));
}

/// NW-7 fixer: a READY waiter looks at its connection only when its change counter moved, or once per slice (B4);
/// a "ready" answer is never reused (a wait that ended ready is asked again and must still say ready).
#[test]
fn quiet_skips_looks_until_the_counter_moves() {
    use core::cell::Cell;
    let q = super::Quiet::new();
    let looks = Cell::new(0);
    let look = |r: bool| {
        looks.set(looks.get() + 1);
        r
    };
    assert!(q.look(None, || look(true)) && q.look(None, || look(true)), "no counter (loopback): always look");
    assert_eq!(looks.get(), 2);
    assert!(!q.look(Some(5), || look(false)), "the first look always happens");
    assert!(!q.look(Some(5), || look(true)), "nothing changed: no look");
    assert_eq!(looks.get(), 3);
    assert!(q.look(Some(6), || look(true)), "the counter moved: look");
    assert!(q.look(Some(6), || look(true)), "a ready answer is asked again, not reused as not-ready");
    assert_eq!(looks.get(), 5);
}

/// WY-1a CR1 (Smithay's display epoll inside calloop's): a stream socket in an inner epoll wakes the outer epoll
/// on every message, also after the inner one was harvested by its own epoll_wait and the socket drained. Before
/// INT-W4 the socket named no `PollQueue`, so only a sleeper on the inner epoll ever looked at it again.
#[test]
fn a_unix_stream_in_a_nested_epoll_wakes_the_outer_one_every_time() {
    use vfs::epoll::{new_epoll, EPOLL_CTL_ADD};
    let ns = ns();
    let p = Prog::new(&ns, 11, 1000);
    let (s0, s1) = p.pair(SOCK_STREAM);
    p.set_nonblock(s0);
    let (inner, outer) = (new_epoll(), new_epoll());
    let f0 = p.fds.get(s0 as usize).unwrap();
    vfs::epoll::Epoll::ctl(&outer, EPOLL_CTL_ADD, &inner, 50, (E::IN, 9), false).unwrap();
    vfs::epoll::Epoll::ctl(&inner, EPOLL_CTL_ADD, &f0, s0 as i32, (E::IN, 1), false).unwrap();
    let wait = |ep: &alloc::sync::Arc<vfs::OpenFile>| {
        let mut v: Vec<(u32, u64)> = Vec::new();
        ep.file.as_epoll().unwrap().wait(&mut v, 8, Some(0), &|| false).unwrap();
        v
    };
    for round in 0..4 {
        assert_eq!(p.send(s1, b"m", 0), 1);
        assert_eq!(wait(&outer), vec![(E::IN, 9)], "round {round}: the outer epoll");
        assert_eq!(wait(&inner), vec![(E::IN, 1)], "round {round}: the inner epoll");
        assert_eq!(p.recv(s0, 8, 0).0, 1);
        assert!(wait(&inner).is_empty(), "round {round}: drained");
        assert!(wait(&outer).is_empty(), "round {round}: outer after the drain");
    }
}
