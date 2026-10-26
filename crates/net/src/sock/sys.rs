//! The socket syscalls (linux-6.6/net/socket.c, net/core/sock.c `sk_setsockopt`/`sk_getsockopt`,
//! net/core/scm.c `put_cmsg`/`scm_detach_fds`) on `vfs::sys::Sys`: the caller's memory, fd table, mount view and
//! credentials. Every function takes the six raw arguments and returns the value or -errno, with Linux's
//! argument checks in Linux's order; `crates/syscall/src/socket.rs` registers them.

use super::abi::*;
use super::cmsg;
use super::inet::{self, InetSock};
use super::netlink::{self, NetlinkSock};
use super::unix::{SendArgs, UType, UnixSock};
use super::{Caller, Kind, Received, SockFile, Source, flag};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use uapi::errno::*;
use vfs::sys::{Args, Sys};
use vfs::{Errno, OpenFile, OpenFlags};

fn ret(r: Result<i64, Errno>) -> i64 {
    r.unwrap_or_else(|e| -e)
}

fn fd_of(a: u64) -> Result<usize, Errno> {
    let fd = a as u32 as i32;
    if fd < 0 { Err(EBADF) } else { Ok(fd as usize) }
}

/// `access_ok` of x86-64: no wrap, ends in the lower half.
fn access_ok(buf: u64, count: u64) -> bool {
    buf.checked_add(count).is_some_and(|end| (end as i64) >= 0)
}

fn caller(s: &Sys<'_>) -> Caller {
    Caller { pid: s.subject.pid, uid: s.cred.uid, gid: s.cred.gid, subject: s.subject }
}

/// `sockfd_lookup_light`: EBADF (no fd, or an O_PATH one), ENOTSOCK.
fn sock(s: &Sys<'_>, fd: u64) -> Result<(Arc<OpenFile>, Arc<SockFile>), Errno> {
    let f = s.fds.get(fd_of(fd)?)?;
    if f.is_path_only() {
        return Err(EBADF);
    }
    let k = SockFile::of(&f.file).ok_or(ENOTSOCK)?;
    Ok((f, k))
}

fn read_bytes(s: &Sys<'_>, addr: u64, n: usize) -> Result<Vec<u8>, Errno> {
    let mut v = super::buffer(n)?;
    if n > 0 && !s.mem.read(addr, &mut v) {
        return Err(EFAULT);
    }
    Ok(v)
}

fn read_u32(s: &Sys<'_>, addr: u64) -> Result<u32, Errno> {
    let mut b = [0u8; 4];
    if !s.mem.read(addr, &mut b) {
        return Err(EFAULT);
    }
    Ok(u32::from_le_bytes(b))
}

fn read_u64(s: &Sys<'_>, addr: u64) -> Result<u64, Errno> {
    let mut b = [0u8; 8];
    if !s.mem.read(addr, &mut b) {
        return Err(EFAULT);
    }
    Ok(u64::from_le_bytes(b))
}

fn put(s: &Sys<'_>, addr: u64, data: &[u8]) -> Result<(), Errno> {
    if data.is_empty() || s.mem.write(addr, data) { Ok(()) } else { Err(EFAULT) }
}

/// `move_addr_to_kernel`.
fn addr_in(s: &Sys<'_>, uaddr: u64, len: u64) -> Result<Vec<u8>, Errno> {
    let len = len as u32 as i32;
    if len < 0 || len as usize > SOCKADDR_STORAGE {
        return Err(EINVAL);
    }
    read_bytes(s, uaddr, len as usize)
}

/// `move_addr_to_user`: at most the caller's length is copied; the full length goes back.
fn addr_out(s: &Sys<'_>, k: &[u8], uaddr: u64, ulen: u64) -> Result<(), Errno> {
    let len = read_u32(s, ulen)? as i32;
    if len < 0 {
        return Err(EINVAL);
    }
    let n = (len as usize).min(k.len());
    put(s, uaddr, &k[..n])?;
    put(s, ulen, &(k.len() as u32).to_le_bytes())
}

fn open_file(f: Arc<SockFile>, flags: u64) -> Arc<OpenFile> {
    OpenFile::new(f, OpenFlags { read: true, write: true, nonblock: flags & SOCK_NONBLOCK != 0, ..Default::default() })
}

/// Installs a new socket file (O_RDWR, O_NONBLOCK as asked) at the lowest free fd.
fn install(s: &Sys<'_>, f: Arc<SockFile>, flags: u64) -> Result<usize, Errno> {
    s.fds.install_flags(open_file(f, flags), 0, flags & SOCK_CLOEXEC != 0)
}

/// `__sock_create` with the families BitsOS has: AF_UNIX, AF_INET, AF_INET6 (::1 and v4-mapped peers, NW-v6) and
/// AF_NETLINK (NETLINK_ROUTE, NW-6).
fn create(family: u64, ty: u32, protocol: u64, c: &Caller) -> Result<Kind, Errno> {
    let family = family as u32 as i32;
    if family < 0 || family as u64 >= NPROTO {
        return Err(EAFNOSUPPORT);
    }
    if ty >= SOCK_MAX {
        return Err(EINVAL);
    }
    match family as u16 {
        AF_UNIX => {
            let p = protocol as u32 as i32;
            if p != 0 && p != AF_UNIX as i32 {
                return Err(EPROTONOSUPPORT);
            }
            let t = UType::from_linux(ty).ok_or(ESOCKTNOSUPPORT)?;
            Ok(Kind::Unix(UnixSock::new(t)))
        }
        AF_INET => Ok(Kind::Inet(InetSock::create(ty, protocol as u32 as i32, c)?)),
        AF_INET6 => Ok(Kind::Inet(InetSock::create6(ty, protocol as u32 as i32, c)?)),
        AF_NETLINK => Ok(Kind::Netlink(NetlinkSock::create(ty, protocol as u32 as i32, c)?)),
        _ => Err(EAFNOSUPPORT),
    }
}

/// socket(2).
pub fn socket(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let ty = a[1] as u32 as u64;
        if ty & !SOCK_TYPE_MASK & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
            return Err(EINVAL);
        }
        let kind = create(a[0], (ty & SOCK_TYPE_MASK) as u32, a[2], &caller(s))?;
        let f = SockFile::new(kind)?;
        install(s, f, ty).map(|fd| fd as i64)
    })())
}

/// A placeholder for a reserved fd number (socketpair takes both numbers before it makes the sockets).
struct Reserved;

impl vfs::File for Reserved {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn mode(&self) -> u32 {
        0
    }
}

/// socketpair(2): two fds first (EMFILE), their numbers to the program (EFAULT), then the sockets.
pub fn socketpair(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let ty = a[1] as u32 as u64;
        let flags = ty & !SOCK_TYPE_MASK;
        if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
            return Err(EINVAL);
        }
        let ph = || OpenFile::new(Arc::new(Reserved), OpenFlags::default());
        let fd1 = s.fds.install_flags(ph(), 0, false)?;
        let fd2 = match s.fds.install_flags(ph(), 0, false) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = s.fds.close(fd1);
                return Err(e);
            }
        };
        let undo = |e: Errno| {
            let _ = s.fds.close(fd1);
            let _ = s.fds.close(fd2);
            Err(e)
        };
        if !s.mem.write(a[3], &(fd1 as u32).to_le_bytes()) || !s.mem.write(a[3].wrapping_add(4), &(fd2 as u32).to_le_bytes()) {
            return undo(EFAULT);
        }
        let c = caller(s);
        let kind = match create(a[0], (ty & SOCK_TYPE_MASK) as u32, a[2], &c) {
            Ok(k) => k,
            Err(e) => return undo(e),
        };
        let Kind::Unix(u) = kind else { return undo(EOPNOTSUPP) };
        let (x, y) = UnixSock::pair(u.ty, &c);
        u.release(false);
        drop(u);
        let (fx, fy) = match (SockFile::new(Kind::Unix(x)), SockFile::new(Kind::Unix(y))) {
            (Ok(fx), Ok(fy)) => (fx, fy),
            _ => return undo(ENOMEM),
        };
        let cloexec = flags & SOCK_CLOEXEC != 0;
        s.fds.install_at_flags(fd1, open_file(fx, flags), cloexec)?;
        s.fds.install_at_flags(fd2, open_file(fy, flags), cloexec)?;
        Ok(0)
    })())
}

/// bind(2).
pub fn bind(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let (_, k) = sock(s, a[0])?;
        let raw = addr_in(s, a[1], a[2])?;
        match &k.kind {
            Kind::Unix(u) => u.bind(&raw, &s.ctx()).map(|_| 0),
            Kind::Inet(i) => i.bind(&raw, &caller(s)).map(|_| 0),
            Kind::Netlink(n) => n.bind(&raw, &caller(s)).map(|_| 0),
        }
    })())
}

/// listen(2): the backlog is clamped to net.core.somaxconn (a negative one too: Linux compares unsigned).
pub fn listen(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let (_, k) = sock(s, a[0])?;
        let backlog = (a[1] as u32).min(SOMAXCONN) as usize;
        match &k.kind {
            Kind::Unix(u) => u.listen(backlog, &caller(s)).map(|_| 0),
            Kind::Inet(i) => i.listen(backlog, &caller(s)).map(|_| 0),
            // `sock_no_listen`.
            Kind::Netlink(_) => Err(EOPNOTSUPP),
        }
    })())
}

fn do_accept(s: &Sys<'_>, a: Args, flags: u64) -> Result<i64, Errno> {
    let f = s.fds.get(fd_of(a[0])?)?;
    if f.is_path_only() {
        return Err(EBADF);
    }
    if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(EINVAL);
    }
    // The fd number is taken before the wait on Linux: EMFILE first.
    if !s.fds.has_free() {
        return Err(EMFILE);
    }
    let k = SockFile::of(&f.file).ok_or(ENOTSOCK)?;
    let nonblock = f.nonblocking();
    let (kind, peer) = match &k.kind {
        Kind::Unix(u) => {
            let n = u.accept(nonblock)?;
            let peer = if a[1] != 0 { Some(n.getname(true).map_err(|_| ECONNABORTED)?) } else { None };
            (Kind::Unix(n), peer)
        }
        Kind::Inet(i) => {
            let (n, peer) = i.accept(nonblock, &caller(s))?;
            (Kind::Inet(n), (a[1] != 0).then_some(peer))
        }
        // `sock_no_accept`.
        Kind::Netlink(_) => return Err(EOPNOTSUPP),
    };
    let nf = SockFile::new(kind)?;
    if let Some(p) = peer {
        // A fault here loses the connection (Linux releases the new file too).
        addr_out(s, &p, a[1], a[2])?;
    }
    install(s, nf, flags).map(|fd| fd as i64)
}

pub fn accept(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_accept(s, a, 0))
}

pub fn accept4(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_accept(s, a, a[3] as u32 as u64))
}

/// connect(2).
pub fn connect(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.fds.get(fd_of(a[0])?)?;
        if f.is_path_only() {
            return Err(EBADF);
        }
        let raw = addr_in(s, a[1], a[2])?;
        let k = SockFile::of(&f.file).ok_or(ENOTSOCK)?;
        let nonblock = f.nonblocking();
        match &k.kind {
            Kind::Unix(u) => u.connect(&raw, Some(&s.ctx()), &caller(s), nonblock).map(|_| 0),
            Kind::Inet(i) => i.connect(&raw, &caller(s), nonblock).map(|_| 0),
            Kind::Netlink(n) => n.connect(&raw, &caller(s)).map(|_| 0),
        }
    })())
}

fn getname(s: &Sys<'_>, a: Args, peer: bool) -> i64 {
    ret((|| {
        let (_, k) = sock(s, a[0])?;
        let name = match &k.kind {
            Kind::Unix(u) => u.getname(peer)?,
            Kind::Inet(i) => i.getname(peer)?,
            Kind::Netlink(n) => n.getname(peer),
        };
        addr_out(s, &name, a[1], a[2]).map(|_| 0)
    })())
}

pub fn getsockname(s: &Sys<'_>, a: Args) -> i64 {
    getname(s, a, false)
}

pub fn getpeername(s: &Sys<'_>, a: Args) -> i64 {
    getname(s, a, true)
}

/// shutdown(2).
pub fn shutdown(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let (_, k) = sock(s, a[0])?;
        // `sock_no_shutdown`, whatever `how` is.
        if k.netlink().is_some() {
            return Err(EOPNOTSUPP);
        }
        let how = a[1] as u32 as i32;
        if how < 0 {
            return Err(EINVAL);
        }
        match &k.kind {
            Kind::Unix(u) => u.shutdown(how as u64).map(|_| 0),
            Kind::Inet(i) => i.shutdown(how as u64).map(|_| 0),
            Kind::Netlink(_) => Err(EOPNOTSUPP),
        }
    })())
}

// ---- sending and receiving ------------------------------------------------------------------------------

/// The bytes of a send, read from the program's memory segment after segment.
struct UserSource<'a, 'b> {
    s: &'a Sys<'b>,
    segs: Vec<(u64, u64)>,
    seg: usize,
    off: u64,
}

impl Source for UserSource<'_, '_> {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), Errno> {
        let mut done = 0usize;
        while done < buf.len() {
            let (base, len) = *self.segs.get(self.seg).ok_or(EFAULT)?;
            if self.off >= len {
                self.seg += 1;
                self.off = 0;
                continue;
            }
            let n = ((len - self.off) as usize).min(buf.len() - done);
            if !self.s.mem.read(base + self.off, &mut buf[done..done + n]) {
                return Err(EFAULT);
            }
            done += n;
            self.off += n as u64;
        }
        Ok(())
    }
}

/// Writes received bytes into the program's segments in order.
struct UserSink<'a, 'b> {
    s: &'a Sys<'b>,
    segs: Vec<(u64, u64)>,
    seg: usize,
    off: u64,
}

impl UserSink<'_, '_> {
    fn put(&mut self, mut d: &[u8]) -> Result<(), Errno> {
        while !d.is_empty() {
            let (base, len) = *self.segs.get(self.seg).ok_or(EFAULT)?;
            if self.off >= len {
                self.seg += 1;
                self.off = 0;
                continue;
            }
            let n = ((len - self.off) as usize).min(d.len());
            if !self.s.mem.write(base + self.off, &d[..n]) {
                return Err(EFAULT);
            }
            d = &d[n..];
            self.off += n as u64;
        }
        Ok(())
    }
}

/// `import_iovec`: EFAULT for the array, EINVAL for a negative length, EFAULT for a segment that fails
/// `access_ok`; the total is clipped to MAX_RW_COUNT.
fn iovecs(s: &Sys<'_>, iov: u64, cnt: u64) -> Result<Vec<(u64, u64)>, Errno> {
    if cnt > UIO_MAXIOV {
        return Err(EINVAL);
    }
    let raw = read_bytes(s, iov, cnt as usize * 16)?;
    let mut out = Vec::new();
    out.try_reserve_exact(cnt as usize).map_err(|_| ENOMEM)?;
    for c in raw.chunks_exact(16) {
        let base = u64::from_le_bytes(c[0..8].try_into().unwrap_or([0; 8]));
        let len = u64::from_le_bytes(c[8..16].try_into().unwrap_or([0; 8]));
        if (len as i64) < 0 {
            return Err(EINVAL);
        }
        out.push((base, len));
    }
    let mut total = 0u64;
    for (base, len) in out.iter_mut() {
        if !access_ok(*base, *len) {
            return Err(EFAULT);
        }
        *len = (*len).min(MAX_RW_COUNT - total);
        total += *len;
    }
    Ok(out)
}

fn total(segs: &[(u64, u64)]) -> usize {
    segs.iter().map(|(_, l)| *l as usize).sum()
}

/// One send on a socket (sendto, sendmsg, sendmmsg's entries).
fn send_on(s: &Sys<'_>, f: &OpenFile, k: &SockFile, segs: Vec<(u64, u64)>, flags: u32, to: Option<&[u8]>, ctl: &[u8]) -> Result<usize, Errno> {
    let len = total(&segs);
    let flags = flags & !MSG_INTERNAL_SENDMSG_FLAGS;
    let nonblock = f.nonblocking() || flags & MSG_DONTWAIT != 0;
    let mut src = UserSource { s, segs, seg: 0, off: 0 };
    match &k.kind {
        Kind::Unix(u) => {
            let c = caller(s);
            let parsed = if ctl.is_empty() { cmsg::SendCtl::default() } else { cmsg::parse_unix(ctl, &c, |n| s.fds.get(n as usize).ok())? };
            let args = SendArgs { flags, nonblock, to, files: parsed.files, creds: parsed.creds, caller: c, nofile: s.fds.limit() };
            u.send(&mut src, len, args, Some(&s.ctx()))
        }
        Kind::Inet(i) => {
            if !ctl.is_empty() {
                cmsg::check_inet(ctl)?;
            }
            i.send(&mut src, len, flags, nonblock, to)
        }
        Kind::Netlink(n) => {
            if flags & MSG_OOB != 0 {
                return Err(EOPNOTSUPP);
            }
            if len == 0 {
                return Err(ENODATA);
            }
            let c = caller(s);
            // `scm_send`: the same checks as AF_UNIX; files passed to netlink are dropped, credentials travel.
            let parsed = if ctl.is_empty() { cmsg::SendCtl::default() } else { cmsg::parse_unix(ctl, &c, |n| s.fds.get(n as usize).ok())? };
            n.send(&mut src, len, flags, nonblock, to, &c, parsed.creds)
        }
    }
}

/// sendto(2) (and send).
pub fn sendto(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let len = a[2].min(MAX_RW_COUNT);
        if !access_ok(a[1], len) {
            return Err(EFAULT);
        }
        let (f, k) = sock(s, a[0])?;
        let to = if a[4] != 0 { Some(addr_in(s, a[4], a[5])?) } else { None };
        send_on(s, &f, &k, alloc::vec![(a[1], len)], a[3] as u32, to.as_deref(), &[]).map(|n| n as i64)
    })())
}

/// The user `struct msghdr` (56 bytes).
struct MsgHdr {
    name: u64,
    namelen: i32,
    iov: u64,
    iovlen: u64,
    control: u64,
    controllen: u64,
}

fn msghdr(s: &Sys<'_>, at: u64) -> Result<MsgHdr, Errno> {
    let b = read_bytes(s, at, MSGHDR)?;
    let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]));
    let mut m = MsgHdr { name: q(0), namelen: q(8) as u32 as i32, iov: q(16), iovlen: q(24), control: q(32), controllen: q(40) };
    // `__copy_msghdr`: no name means no length; a negative length is EINVAL; more than sockaddr_storage is cut.
    if m.name == 0 {
        m.namelen = 0;
    }
    if m.namelen < 0 {
        return Err(EINVAL);
    }
    m.namelen = m.namelen.min(SOCKADDR_STORAGE as i32);
    Ok(m)
}

fn sendmsg_one(s: &Sys<'_>, f: &OpenFile, k: &SockFile, at: u64, flags: u32) -> Result<(usize, bool), Errno> {
    let m = msghdr(s, at)?;
    let to = if m.name != 0 && m.namelen > 0 { Some(read_bytes(s, m.name, m.namelen as usize)?) } else { None };
    if m.iovlen > UIO_MAXIOV {
        return Err(EMSGSIZE);
    }
    let segs = iovecs(s, m.iov, m.iovlen)?;
    // `sock_kmalloc` takes at most net.core.optmem_max (CachyOS 131072) for the control data: ENOBUFS above.
    if m.controllen > i32::MAX as u64 || m.controllen >= OPTMEM_MAX {
        return Err(ENOBUFS);
    }
    let ctl = if m.controllen > 0 { read_bytes(s, m.control, m.controllen as usize)? } else { Vec::new() };
    let want = total(&segs);
    let n = send_on(s, f, k, segs, flags, to.as_deref(), &ctl)?;
    Ok((n, n < want))
}

/// sendmsg(2).
pub fn sendmsg(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let flags = a[2] as u32;
        if flags & MSG_CMSG_COMPAT != 0 {
            return Err(EINVAL);
        }
        let (f, k) = sock(s, a[0])?;
        sendmsg_one(s, &f, &k, a[1], flags).map(|(n, _)| n as i64)
    })())
}

/// sendmmsg(2): an error after the first message ends the batch with the count so far.
pub fn sendmmsg(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let oflags = a[3] as u32;
        if oflags & MSG_CMSG_COMPAT != 0 {
            return Err(EINVAL);
        }
        let vlen = (a[2] as u32 as u64).min(UIO_MAXIOV);
        let (f, k) = sock(s, a[0])?;
        let mut sent = 0u64;
        let mut err = 0i64;
        while sent < vlen {
            let flags = if sent == vlen - 1 { oflags } else { oflags | MSG_BATCH };
            let entry = a[1].wrapping_add(sent * MMSGHDR as u64);
            match sendmsg_one(s, &f, &k, entry, flags) {
                Ok((n, partial)) => {
                    if put(s, entry + MSGHDR as u64, &(n as u32).to_le_bytes()).is_err() {
                        err = EFAULT;
                        break;
                    }
                    sent += 1;
                    if partial {
                        break;
                    }
                }
                Err(e) => {
                    err = e;
                    break;
                }
            }
        }
        if sent > 0 || err == 0 { Ok(sent as i64) } else { Err(err) }
    })())
}

/// Receives on a socket into `segs`; `flags` as the program gave them.
fn recv_on(s: &Sys<'_>, f: &OpenFile, k: &SockFile, segs: Vec<(u64, u64)>, flags: u32, want_from: bool) -> Result<Received, Errno> {
    let size = total(&segs);
    let nonblock = f.nonblocking() || flags & MSG_DONTWAIT != 0;
    let mut sink = UserSink { s, segs, seg: 0, off: 0 };
    let mut put = |d: &[u8]| sink.put(d);
    match &k.kind {
        Kind::Unix(u) => u.recv(size, flags, nonblock, &mut put, want_from),
        Kind::Inet(i) => i.recv(size, flags, nonblock, &mut put),
        Kind::Netlink(n) => n.recv(size, flags, nonblock, &mut put),
    }
}

/// recvfrom(2) (and recv).
pub fn recvfrom(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let len = a[2].min(MAX_RW_COUNT);
        if !access_ok(a[1], len) {
            return Err(EFAULT);
        }
        let (f, k) = sock(s, a[0])?;
        let r = recv_on(s, &f, &k, alloc::vec![(a[1], len)], a[3] as u32, a[4] != 0)?;
        if a[4] != 0 {
            addr_out(s, r.from.as_deref().unwrap_or(&[]), a[4], a[5])?;
        }
        Ok(r.n as i64)
    })())
}

/// The control buffer of a recvmsg (`put_cmsg`, `scm_detach_fds`).
struct CtlOut {
    at: u64,
    left: u64,
    used: u64,
    flags: u32,
}

impl CtlOut {
    fn put(&mut self, s: &Sys<'_>, level: u32, ty: u32, data: &[u8]) {
        if self.at == 0 || self.left < CMSGHDR as u64 {
            self.flags |= MSG_CTRUNC;
            return;
        }
        let mut cmlen = cmsg_len(data.len()) as u64;
        if self.left < cmlen {
            self.flags |= MSG_CTRUNC;
            cmlen = self.left;
        }
        let mut b = Vec::with_capacity(cmlen as usize);
        b.extend_from_slice(&cmlen.to_le_bytes());
        b.extend_from_slice(&level.to_le_bytes());
        b.extend_from_slice(&ty.to_le_bytes());
        b.extend_from_slice(&data[..cmlen as usize - CMSGHDR]);
        // Linux ignores put_cmsg's EFAULT here.
        if put(s, self.at, &b).is_err() {
            return;
        }
        self.advance(cmsg_space(data.len()) as u64);
    }

    fn advance(&mut self, n: u64) {
        let n = n.min(self.left);
        self.at += n;
        self.left -= n;
        self.used += n;
    }

    /// `scm_detach_fds`: as many fds as fit (installed, numbers written), the rest closed with MSG_CTRUNC.
    fn rights(&mut self, s: &Sys<'_>, files: Vec<Arc<OpenFile>>, cloexec: bool) {
        let count = files.len();
        let fdmax = if self.left <= CMSGHDR as u64 { 0 } else { ((self.left - CMSGHDR as u64) / 4) as usize }.min(count);
        let mut i = 0usize;
        for f in files.into_iter().take(fdmax) {
            let Ok(fd) = s.fds.install_flags(f, 0, cloexec) else { break };
            if put(s, self.at + CMSGHDR as u64 + i as u64 * 4, &(fd as u32).to_le_bytes()).is_err() {
                let _ = s.fds.close(fd);
                break;
            }
            i += 1;
        }
        if i > 0 {
            let cmlen = cmsg_len(i * 4) as u64;
            let mut h = Vec::with_capacity(CMSGHDR);
            h.extend_from_slice(&cmlen.to_le_bytes());
            h.extend_from_slice(&SOL_SOCKET.to_le_bytes());
            h.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
            if put(s, self.at, &h).is_ok() {
                self.advance(cmsg_space(i * 4) as u64);
            }
        }
        if i < count || (count > 0 && fdmax == 0) {
            self.flags |= MSG_CTRUNC;
        }
    }
}

fn recvmsg_one(s: &Sys<'_>, f: &OpenFile, k: &SockFile, at: u64, flags: u32) -> Result<usize, Errno> {
    let m = msghdr(s, at)?;
    if m.iovlen > UIO_MAXIOV {
        return Err(EMSGSIZE);
    }
    let segs = iovecs(s, m.iov, m.iovlen)?;
    let r = recv_on(s, f, k, segs, flags, m.name != 0)?;
    if m.name != 0 {
        addr_out(s, r.from.as_deref().unwrap_or(&[]), m.name, at + 8)?;
    }
    let mut out = CtlOut { at: m.control, left: m.controllen, used: 0, flags: r.flags & (MSG_TRUNC | MSG_CTRUNC | MSG_EOR) };
    out.flags |= flags & MSG_CMSG_CLOEXEC;
    if m.control == 0 {
        if r.creds.is_some() || !r.fds.is_empty() || r.pktinfo.is_some() || !r.ctl.is_empty() {
            out.flags |= MSG_CTRUNC;
        }
        drop(r.fds);
    } else {
        // `netlink_cmsg_recv_pktinfo` comes before `scm_recv`.
        if let Some(g) = r.pktinfo {
            out.put(s, netlink::SOL_NETLINK, netlink::NETLINK_PKTINFO, &g.to_le_bytes());
        }
        // The protocol's own messages (`ip_cmsg_recv`, `ip6_datagram_recv_ctl`) come before `scm_recv` as well.
        for (level, ty, d) in &r.ctl {
            out.put(s, *level, *ty, d);
        }
        if let Some(c) = r.creds {
            out.put(s, SOL_SOCKET, SCM_CREDENTIALS, &c.bytes());
        }
        if !r.fds.is_empty() {
            out.rights(s, r.fds, flags & MSG_CMSG_CLOEXEC != 0);
        }
    }
    put(s, at + 48, &out.flags.to_le_bytes())?;
    put(s, at + 40, &out.used.to_le_bytes())?;
    Ok(r.n)
}

/// recvmsg(2).
pub fn recvmsg(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let flags = a[2] as u32;
        if flags & MSG_CMSG_COMPAT != 0 {
            return Err(EINVAL);
        }
        let (f, k) = sock(s, a[0])?;
        recvmsg_one(s, &f, &k, a[1], flags).map(|n| n as i64)
    })())
}

fn take_error(k: &SockFile) -> i64 {
    match &k.kind {
        Kind::Unix(u) => u.take_error(),
        Kind::Inet(i) => i.take_error(),
        Kind::Netlink(n) => n.take_error(),
    }
}

fn set_error(k: &SockFile, e: i64) {
    match &k.kind {
        Kind::Unix(u) => u.set_error(e),
        Kind::Inet(i) => i.set_error(e),
        Kind::Netlink(n) => n.set_error(e),
    }
}

/// recvmmsg(2): MSG_WAITFORONE makes the rest non-blocking; the timeout is looked at between messages only
/// (as Linux); an error after some messages is kept as the socket's pending error unless it is EAGAIN.
pub fn recvmmsg(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let mut flags = a[3] as u32;
        if flags & MSG_CMSG_COMPAT != 0 {
            return Err(EINVAL);
        }
        let mut end: Option<u64> = None;
        if a[4] != 0 {
            let sec = read_u64(s, a[4])? as i64;
            let nsec = read_u64(s, a[4] + 8)? as i64;
            if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
                return Err(EINVAL);
            }
            end = Some(super::now().saturating_add((sec as u64).saturating_mul(1_000_000_000)).saturating_add(nsec as u64));
        }
        let vlen = a[2] as u32 as u64;
        let (f, k) = sock(s, a[0])?;
        if flags & MSG_ERRQUEUE == 0 {
            let e = take_error(&k);
            if e != 0 {
                return Err(e);
            }
        }
        let mut got = 0u64;
        let mut err = 0i64;
        let mut timed_out = false;
        while got < vlen {
            let entry = a[1].wrapping_add(got * MMSGHDR as u64);
            match recvmsg_one(s, &f, &k, entry, flags & !MSG_WAITFORONE) {
                Ok(n) => {
                    if put(s, entry + MSGHDR as u64, &(n as u32).to_le_bytes()).is_err() {
                        err = EFAULT;
                        break;
                    }
                }
                Err(e) => {
                    err = e;
                    break;
                }
            }
            got += 1;
            if flags & MSG_WAITFORONE != 0 {
                flags |= MSG_DONTWAIT;
            }
            if let Some(e) = end
                && super::now() >= e
            {
                timed_out = true;
                break;
            }
        }
        if got == 0 && err != 0 {
            return Err(err);
        }
        if got > 0 && err != 0 && err != EAGAIN {
            set_error(&k, err);
        }
        if let Some(e) = end
            && got > 0
        {
            let left = if timed_out { 0 } else { e.saturating_sub(super::now()) };
            let mut ts = [0u8; 16];
            ts[0..8].copy_from_slice(&(left / 1_000_000_000).to_le_bytes());
            ts[8..16].copy_from_slice(&(left % 1_000_000_000).to_le_bytes());
            put(s, a[4], &ts)?;
        }
        Ok(got as i64)
    })())
}

// ---- options --------------------------------------------------------------------------------------------

/// A timeval option value (`sock_set_timeout`): stored in ns, 0 = no timeout, u64::MAX = do not wait at all
/// (a negative timeval).
fn set_timeout(slot: &core::sync::atomic::AtomicU64, val: &[u8]) -> Result<(), Errno> {
    if val.len() < 16 {
        return Err(EINVAL);
    }
    let sec = i64::from_le_bytes(val[0..8].try_into().unwrap_or([0; 8]));
    let usec = i64::from_le_bytes(val[8..16].try_into().unwrap_or([0; 8]));
    if !(0..1_000_000).contains(&usec) {
        return Err(EDOM);
    }
    let ns = if sec < 0 {
        u64::MAX
    } else if sec == 0 && usec == 0 {
        0
    } else {
        // Linux keeps jiffies: HZ 1000 (CachyOS) rounds the microseconds up to whole milliseconds.
        (sec as u64).saturating_mul(1_000_000_000).saturating_add((usec as u64).div_ceil(1000) * 1_000_000)
    };
    slot.store(ns, Ordering::Relaxed);
    Ok(())
}

fn get_timeout(slot: &core::sync::atomic::AtomicU64) -> Vec<u8> {
    let ns = slot.load(Ordering::Relaxed);
    let (sec, usec) = if ns == 0 || ns == u64::MAX { (0, 0) } else { (ns / 1_000_000_000, (ns % 1_000_000_000) / 1000) };
    let mut v = (sec as i64).to_le_bytes().to_vec();
    v.extend_from_slice(&(usec as i64).to_le_bytes());
    v
}

/// SOL_SOCKET options (`sk_setsockopt`); `val` holds at least 4 bytes.
fn set_sol_socket(k: &SockFile, c: &Caller, name: u32, val: &[u8]) -> Result<(), Errno> {
    let o = k.opts();
    let v = i32::from_le_bytes(val[0..4].try_into().unwrap_or([0; 4]));
    let on = v != 0;
    match name {
        SO_DEBUG if on && c.uid != 0 => return Err(EACCES),
        SO_DEBUG => o.set(flag::DEBUG, on),
        SO_REUSEADDR => o.set(flag::REUSEADDR, on),
        SO_REUSEPORT => o.set(flag::REUSEPORT, on),
        SO_TYPE | SO_PROTOCOL | SO_DOMAIN | SO_ERROR => return Err(ENOPROTOOPT),
        SO_DONTROUTE => o.set(flag::DONTROUTE, on),
        SO_BROADCAST => o.set(flag::BROADCAST, on),
        SO_SNDBUF | SO_SNDBUFFORCE => {
            let v = if name == SO_SNDBUF {
                (v as u32).min(WMEM_MAX)
            } else if c.uid != 0 {
                return Err(EPERM);
            } else {
                v.max(0) as u32
            };
            let v = v.min(i32::MAX as u32 / 2);
            o.sndbuf.store((v * 2).max(SOCK_MIN_SNDBUF), Ordering::Relaxed);
            if let Kind::Unix(u) = &k.kind {
                u.wake();
            }
        }
        SO_RCVBUF | SO_RCVBUFFORCE => {
            let v = if name == SO_RCVBUF {
                (v as u32).min(RMEM_MAX)
            } else if c.uid != 0 {
                return Err(EPERM);
            } else {
                v.max(0) as u32
            };
            let v = v.min(i32::MAX as u32 / 2);
            o.rcvbuf.store((v * 2).max(SOCK_MIN_RCVBUF), Ordering::Relaxed);
        }
        SO_KEEPALIVE => {
            o.set(flag::KEEPALIVE, on);
            if let Kind::Inet(i) = &k.kind {
                i.options_changed();
            }
        }
        SO_OOBINLINE => o.set(flag::OOBINLINE, on),
        SO_NO_CHECK => o.set(flag::NO_CHECK, on),
        SO_PRIORITY => {
            if !(0..=6).contains(&v) && c.uid != 0 {
                return Err(EPERM);
            }
            o.priority.store(v as u32, Ordering::Relaxed);
        }
        SO_LINGER => {
            if val.len() < 8 {
                return Err(EINVAL);
            }
            let secs = u32::from_le_bytes(val[4..8].try_into().unwrap_or([0; 4]));
            o.linger.store(if on { (1u64 << 63) | secs as u64 } else { 0 }, Ordering::Relaxed);
        }
        SO_BSDCOMPAT => {}
        SO_PASSCRED => o.set(flag::PASSCRED, on),
        SO_RCVLOWAT => {
            let v = if v < 0 { i32::MAX } else { v };
            o.rcvlowat.store((v as u32).max(1), Ordering::Relaxed);
        }
        SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => set_timeout(&o.rcvtimeo, val)?,
        SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => set_timeout(&o.sndtimeo, val)?,
        // `__sock_set_timestamps`: on sets RCVTSTAMP and chooses ns/new; off clears RCVTSTAMP and RCVTSTAMPNS.
        SO_TIMESTAMP_OLD | SO_TIMESTAMP_NEW | SO_TIMESTAMPNS_OLD | SO_TIMESTAMPNS_NEW => {
            if on {
                o.set(flag::TSTAMP_NEW, matches!(name, SO_TIMESTAMP_NEW | SO_TIMESTAMPNS_NEW));
                o.set(flag::RCVTSTAMPNS, matches!(name, SO_TIMESTAMPNS_OLD | SO_TIMESTAMPNS_NEW));
                o.set(flag::RCVTSTAMP, true);
            } else {
                o.set(flag::RCVTSTAMP, false);
                o.set(flag::RCVTSTAMPNS, false);
            }
        }
        // Turning timestamping off is what V2 has; asking for software/hardware stamps is not offered.
        SO_TIMESTAMPING_OLD | SO_TIMESTAMPING_NEW if v == 0 => {}
        SO_BUSY_POLL if v < 0 => return Err(EINVAL),
        SO_BUSY_POLL => o.busy_poll.store(v as u32, Ordering::Relaxed),
        SO_INCOMING_CPU => o.incoming_cpu.store(v, Ordering::Relaxed),
        SO_BINDTOIFINDEX => return bind_to_index(k, c.uid, v),
        _ => return Err(ENOPROTOOPT),
    }
    Ok(())
}

/// SO_BINDTODEVICE (`sock_setbindtodevice`, before the int-length check): at most IFNAMSIZ-1 bytes of a name;
/// an empty name (or length 0) unbinds; an unknown name is ENODEV; changing a binding needs CAP_NET_RAW
/// (`sock_bindtoindex_locked`).
fn set_bindtodevice(s: &Sys<'_>, k: &SockFile, optval: u64, optlen: usize) -> Result<(), Errno> {
    let n = optlen.min(IFNAMSIZ - 1);
    let name = read_bytes(s, optval, n)?;
    let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
    let index = if name.is_empty() { 0 } else { crate::ifview::by_name(name).ok_or(ENODEV)?.index };
    bind_to_index(k, s.cred.uid, index as i32)
}

/// `sock_bindtoindex_locked` (SO_BINDTODEVICE by name, SO_BINDTOIFINDEX by number): EPERM to change a binding
/// without CAP_NET_RAW, EINVAL for a negative index. An index that names no interface is taken as it is (Linux
/// does not look it up either).
fn bind_to_index(k: &SockFile, uid: u32, index: i32) -> Result<(), Errno> {
    let o = k.opts();
    if o.bound_dev.load(Ordering::Relaxed) != 0 && uid != 0 {
        return Err(EPERM);
    }
    if index < 0 {
        return Err(EINVAL);
    }
    o.bound_dev.store(index as u32, Ordering::Relaxed);
    Ok(())
}

/// getsockopt SO_BINDTODEVICE (`sock_getbindtodevice`): length 0 when not bound, EINVAL below IFNAMSIZ, else the
/// name with its NUL.
fn get_bindtodevice(k: &SockFile, len: usize) -> Result<Vec<u8>, Errno> {
    let index = k.opts().bound_dev.load(Ordering::Relaxed);
    if index == 0 {
        return Ok(Vec::new());
    }
    if len < IFNAMSIZ {
        return Err(EINVAL);
    }
    let mut v = crate::ifview::by_index(index).ok_or(ENODEV)?.name.into_bytes();
    v.push(0);
    Ok(v)
}

/// setsockopt(2).
pub fn setsockopt(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let optlen = a[4] as u32 as i32;
        if optlen < 0 {
            return Err(EINVAL);
        }
        let (_, k) = sock(s, a[0])?;
        let level = a[1] as u32;
        let name = a[2] as u32;
        // Option values are small: a longer optlen is read only as far as the largest option needs (a bound
        // against a program asking the kernel to copy gigabytes).
        let n = (optlen as usize).min(64);
        if level == SOL_SOCKET && name == SO_BINDTODEVICE {
            return set_bindtodevice(s, &k, a[3], optlen as usize).map(|_| 0);
        }
        if level == SOL_SOCKET {
            if optlen < 4 {
                return Err(EINVAL);
            }
            let first = read_bytes(s, a[3], 4)?;
            let val = read_bytes(s, a[3], n).unwrap_or(first);
            return set_sol_socket(&k, &caller(s), name, &val).map(|_| 0);
        }
        match &k.kind {
            Kind::Unix(_) => Err(EOPNOTSUPP),
            Kind::Inet(i) => {
                // SOL_IPV6 has struct values (ipv6_mreq, in6_pktinfo); everything else reads an int.
                let want = if level == inet::SOL_IPV6 { inet::opt6_len(name) } else { 4 };
                let val = read_bytes(s, a[3], n.min(want));
                i.setsockopt(level, name, optlen as usize, val)
                    .map(|_| 0)
            }
            // `netlink_setsockopt`: SOL_NETLINK only; a value shorter than an int is read as 0.
            Kind::Netlink(nl) => {
                if level != netlink::SOL_NETLINK {
                    return Err(ENOPROTOOPT);
                }
                let val = if optlen >= 4 { read_u32(s, a[3])? } else { 0 };
                nl.setsockopt(name, val, &caller(s)).map(|_| 0)
            }
        }
    })())
}

/// SOL_SOCKET values for getsockopt (`sk_getsockopt`).
fn get_sol_socket(k: &SockFile, name: u32) -> Result<Vec<u8>, Errno> {
    let o = k.opts();
    let int = |v: u32| v.to_le_bytes().to_vec();
    let b = |f: u32| int(o.has(f) as u32);
    let (family, ty, proto) = k.triple();
    Ok(match name {
        SO_DEBUG => b(flag::DEBUG),
        SO_DONTROUTE => b(flag::DONTROUTE),
        SO_BROADCAST => b(flag::BROADCAST),
        SO_SNDBUF => int(o.sndbuf.load(Ordering::Relaxed)),
        SO_RCVBUF => int(o.rcvbuf.load(Ordering::Relaxed)),
        SO_REUSEADDR => b(flag::REUSEADDR),
        SO_REUSEPORT => b(flag::REUSEPORT),
        SO_KEEPALIVE => b(flag::KEEPALIVE),
        SO_TYPE => int(ty),
        SO_PROTOCOL => int(proto),
        SO_DOMAIN => int(family as u32),
        SO_ERROR => int(take_error(k) as i32 as u32),
        SO_OOBINLINE => b(flag::OOBINLINE),
        SO_NO_CHECK => b(flag::NO_CHECK),
        SO_PRIORITY => int(o.priority.load(Ordering::Relaxed)),
        SO_LINGER => {
            let l = o.linger.load(Ordering::Relaxed);
            let mut v = int((l >> 63) as u32);
            v.extend_from_slice(&((l & 0xffff_ffff) as u32).to_le_bytes());
            v
        }
        SO_BSDCOMPAT => int(0),
        SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => get_timeout(&o.rcvtimeo),
        SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => get_timeout(&o.sndtimeo),
        SO_RCVLOWAT => int(o.rcvlowat.load(Ordering::Relaxed)),
        SO_SNDLOWAT => int(1),
        SO_PASSCRED => b(flag::PASSCRED),
        SO_PEERCRED => {
            let c = match &k.kind {
                Kind::Unix(u) => u.peercred(),
                _ => None,
            };
            c.unwrap_or(super::Ucred { pid: 0, uid: u32::MAX, gid: u32::MAX }).bytes().to_vec()
        }
        SO_ACCEPTCONN => int(match &k.kind {
            Kind::Unix(u) => u.is_listening(),
            Kind::Inet(i) => i.is_listening(),
            Kind::Netlink(_) => false,
        } as u32),
        SO_TIMESTAMP_OLD => int((o.has(flag::RCVTSTAMP) && !o.has(flag::TSTAMP_NEW) && !o.has(flag::RCVTSTAMPNS)) as u32),
        SO_TIMESTAMPNS_OLD => int((o.has(flag::RCVTSTAMPNS) && !o.has(flag::TSTAMP_NEW)) as u32),
        SO_TIMESTAMP_NEW => int((o.has(flag::RCVTSTAMP) && o.has(flag::TSTAMP_NEW)) as u32),
        SO_TIMESTAMPNS_NEW => int((o.has(flag::RCVTSTAMPNS) && o.has(flag::TSTAMP_NEW)) as u32),
        // `struct so_timestamping` { flags, bind_phc }: never set in V2.
        SO_TIMESTAMPING_OLD => alloc::vec![0u8; 8],
        SO_BUSY_POLL => int(o.busy_poll.load(Ordering::Relaxed)),
        SO_INCOMING_CPU => int(o.incoming_cpu.load(Ordering::Relaxed) as u32),
        SO_BINDTOIFINDEX => int(o.bound_dev.load(Ordering::Relaxed)),
        // The peer's supplementary groups (`sk_peer_cred`): V2's credentials have none, so a peer's list is empty;
        // no peer credentials (an unconnected or AF_INET socket) is ENODATA.
        SO_PEERGROUPS => match k.unix().and_then(|u| u.peercred()) {
            Some(_) => Vec::new(),
            None => return Err(ENODATA),
        },
        _ => return Err(ENOPROTOOPT),
    })
}

/// getsockopt(2).
pub fn getsockopt(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let (_, k) = sock(s, a[0])?;
        let level = a[1] as u32;
        let name = a[2] as u32;
        let len = read_u32(s, a[4])? as i32;
        if len < 0 {
            return Err(EINVAL);
        }
        let v = if level == SOL_SOCKET && name == SO_BINDTODEVICE {
            get_bindtodevice(&k, len as usize)?
        } else if level == SOL_SOCKET {
            get_sol_socket(&k, name)?
        } else {
            match &k.kind {
                Kind::Unix(_) => return Err(EOPNOTSUPP),
                Kind::Inet(i) => i.getsockopt(level, name)?,
                Kind::Netlink(nl) => {
                    if level != netlink::SOL_NETLINK {
                        return Err(ENOPROTOOPT);
                    }
                    let (v, outlen) = nl.getsockopt(name, len as usize)?;
                    put(s, a[3], &v)?;
                    return put(s, a[4], &outlen.to_le_bytes()).map(|_| 0);
                }
            }
        };
        let n = (len as usize).min(v.len());
        put(s, a[3], &v[..n])?;
        put(s, a[4], &(n as u32).to_le_bytes()).map(|_| 0)
    })())
}

// ---- read(2)/write(2)/readv(2)/writev(2) on a socket fd -------------------------------------------------

/// read(2) on a socket (`sock_read_iter`): one recvmsg without flags. For FS's read handler to call when the fd
/// is a socket (the generic path reads streams in 64 KiB pieces, which is fine for stream sockets only).
pub fn read(s: &Sys<'_>, a: Args) -> i64 {
    recvfrom(s, [a[0], a[1], a[2], 0, 0, 0])
}

/// write(2) on a socket (`sock_write_iter`): one sendmsg, so a datagram stays one datagram.
pub fn write(s: &Sys<'_>, a: Args) -> i64 {
    sendto(s, [a[0], a[1], a[2], 0, 0, 0])
}

/// readv(2) on a socket: one recvmsg over all segments.
pub fn readv(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let (f, k) = sock(s, a[0])?;
        let segs = iovecs(s, a[1], a[2])?;
        recv_on(s, &f, &k, segs, 0, false).map(|r| r.n as i64)
    })())
}

/// writev(2) on a socket: one sendmsg over all segments (one datagram on a datagram socket).
pub fn writev(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let (f, k) = sock(s, a[0])?;
        let segs = iovecs(s, a[1], a[2])?;
        send_on(s, &f, &k, segs, 0, None, &[]).map(|n| n as i64)
    })())
}

// ---- ioctl(2) on a socket fd: the interface commands --------------------------------------------------------

/// The interface ioctls (`super::devioctl`) for the ioctl syscall to ask before the file's own ioctl: Some(the
/// result) when `fd` is a socket and `cmd` one of them, None otherwise (not a socket, or the socket's own
/// command: the syscall goes on as before).
pub fn dev_ioctl(s: &Sys<'_>, a: Args) -> Option<i64> {
    let f = s.fds.get(fd_of(a[0]).ok()?).ok()?;
    if f.is_path_only() {
        return None;
    }
    let k = SockFile::of(&f.file)?;
    let asker = super::devioctl::Asker { mem: s.mem, uid: s.cred.uid };
    super::devioctl::ioctl(&asker, &k, a[1] as u32 as u64, a[2])
}
