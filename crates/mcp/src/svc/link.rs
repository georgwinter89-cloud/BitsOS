//! The byte streams between the registry and its services: `Link` (one service's connection) and `Listen` (the
//! socket services connect to). In the kernel both are AF_UNIX stream sockets of NW's `net::sock::unix`
//! (`UnixLink`, `UnixListen`, made by `bind_kernel`); host tests use std's Unix sockets (`crate::host`).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use net::sock::abi::{AF_UNIX, MSG_DONTWAIT, MSG_NOSIGNAL, SHUT_RDWR};
use net::sock::unix::{SendArgs, UType, UnixSock};
use net::sock::{Caller, SliceSource};
use uapi::errno::*;

/// Who is at the other end (SO_PEERCRED of the connection).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Peer {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// One service's connection.
pub trait Link: Send + Sync {
    /// Blocks until bytes arrive (their count), the peer closed (0) or `close` was called (0 or an error).
    fn read(&self, buf: &mut [u8]) -> Result<usize, i64>;
    /// Writes what fits now without waiting (0: no room yet); an error: the connection is broken.
    fn try_write(&self, data: &[u8]) -> Result<usize, i64>;
    /// Ends both directions; a blocked `read` returns.
    fn close(&self);
    fn peer(&self) -> Peer;
}

/// The socket services connect to.
pub trait Listen: Send + Sync {
    /// Blocks until a service connects; an error once `close` was called.
    fn accept(&self) -> Result<Box<dyn Link>, i64>;
    /// Stops listening (a blocked `accept` returns) and removes the socket's name.
    fn close(&self);
}

/// The kernel acting on its own sockets (SO_PEERCRED pid 0, root).
const KERNEL: Caller = Caller { pid: 0, uid: 0, gid: 0, subject: policy::Subject::KERNEL };
/// Linux's somaxconn: connections waiting for `accept` (a queue bound, not a count of services).
const BACKLOG: usize = 4096;
/// The socket file's mode, 0666: services started by init run as root, programs `run` starts as uid 1000; who
/// may register what is B3's (PM) to decide. Until then names are first come, first served, never a kernel
/// tool's, `settings*` only for uid 0, and every tool shows its owner, the service's pid and (uid != 0)
/// `verified:false` (`svc` module docs).
const SOCK_UMASK: u32 = 0o111;

pub struct UnixLink {
    sock: Arc<UnixSock>,
    peer: Peer,
}

impl Link for UnixLink {
    fn read(&self, buf: &mut [u8]) -> Result<usize, i64> {
        let mut at = 0;
        let size = buf.len();
        self.sock.recv(
            size,
            0,
            false,
            &mut |b| {
                let end = (at + b.len()).min(size);
                buf[at..end].copy_from_slice(&b[..end - at]);
                at = end;
                Ok(())
            },
            false,
        )?;
        Ok(at)
    }
    fn try_write(&self, data: &[u8]) -> Result<usize, i64> {
        let mut src = SliceSource { data, at: 0 };
        let a = SendArgs { flags: MSG_NOSIGNAL | MSG_DONTWAIT, nonblock: true, to: None, files: Vec::new(), creds: None, caller: KERNEL, nofile: usize::MAX };
        match self.sock.send(&mut src, data.len(), a, None) {
            Ok(n) => Ok(n),
            Err(e) if e == EAGAIN => Ok(0),
            Err(e) => Err(e),
        }
    }
    fn close(&self) {
        let _ = self.sock.shutdown(SHUT_RDWR);
    }
    fn peer(&self) -> Peer {
        self.peer
    }
}

impl Drop for UnixLink {
    fn drop(&mut self) {
        // The kernel holds no fd: this is the last `fput` (Linux `sock_release`).
        self.sock.release(false);
    }
}

pub struct UnixListen {
    sock: Arc<UnixSock>,
    path: String,
}

impl Listen for UnixListen {
    fn accept(&self) -> Result<Box<dyn Link>, i64> {
        let s = self.sock.accept(false)?;
        let c = s.peercred().unwrap_or_default();
        Ok(Box::new(UnixLink { sock: s, peer: Peer { pid: c.pid, uid: c.uid, gid: c.gid } }))
    }
    fn close(&self) {
        let _ = self.sock.shutdown(SHUT_RDWR);
        let ns = vfs::root_ns();
        if is_socket(&ns, &self.path) {
            let _ = vfs::Ctx::kernel(&ns).unlink(None, self.path.as_bytes());
        }
        self.sock.release(false);
    }
}

fn is_socket(ns: &vfs::MountNs, path: &str) -> bool {
    vfs::Ctx::kernel(ns).lookup(None, path.as_bytes(), false).is_ok_and(|p| p.dentry.inode.meta.mode() & uapi::S_IFMT == vfs::abi::S_IFSOCK)
}

/// `struct sockaddr_un` for a path.
fn sockaddr(path: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + path.len());
    v.extend_from_slice(&AF_UNIX.to_le_bytes());
    v.extend_from_slice(path.as_bytes());
    v.push(0);
    v
}

/// Listens on `path` in the root name space: makes its folder (0755), removes a stale socket of an earlier
/// registry, binds (mode 0666) and listens. A socket someone still listens on stays (EADDRINUSE, as Linux's
/// bind: linux-6.6/net/unix/af_unix.c unix_bind_bsd); stale = a probe connect is refused (ECONNREFUSED), the
/// usual test of a daemon's socket. Another kind of file there: bind's EADDRINUSE.
pub fn bind_kernel(path: &str) -> Result<UnixListen, i64> {
    let ns = vfs::root_ns();
    if let Some(dir) = path.rfind('/').map(|i| &path[..i]).filter(|d| !d.is_empty()) {
        ns.mkdir_p(dir)?;
    }
    if is_socket(&ns, path) {
        match connect_kernel(path) {
            Err(e) if e == ECONNREFUSED => vfs::Ctx::kernel(&ns).unlink(None, path.as_bytes())?,
            Ok(_probe) => {
                klog::event!(Warn, "mcp", "svc.socket_in_use", path = path; "another registry listens there; left alone");
                return Err(EADDRINUSE);
            }
            Err(e) => {
                klog::event!(Warn, "mcp", "svc.socket_in_use", path = path, errno = e; "the socket there answers; left alone");
                return Err(EADDRINUSE);
            }
        }
    }
    let sock = UnixSock::new(UType::Stream);
    let mut ctx = vfs::Ctx::kernel(&ns);
    ctx.umask = SOCK_UMASK;
    sock.bind(&sockaddr(path), &ctx)?;
    if let Err(e) = sock.listen(BACKLOG, &KERNEL) {
        sock.release(false);
        let _ = vfs::Ctx::kernel(&ns).unlink(None, path.as_bytes());
        return Err(e);
    }
    Ok(UnixListen { sock, path: String::from(path) })
}

/// A kernel-side client (ktests: a fake service as a kernel thread).
pub fn connect_kernel(path: &str) -> Result<UnixLink, i64> {
    let ns = vfs::root_ns();
    let sock = UnixSock::new(UType::Stream);
    let ctx = vfs::Ctx::kernel(&ns);
    if let Err(e) = sock.connect(&sockaddr(path), Some(&ctx), &KERNEL, false) {
        sock.release(false);
        return Err(e);
    }
    Ok(UnixLink { sock, peer: Peer::default() })
}
