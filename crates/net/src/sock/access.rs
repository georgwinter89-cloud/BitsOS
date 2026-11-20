//! DB2: the access log's view of inet sockets (vfs::access): connect, listen and accept with the address, and per
//! connection the bytes it moved, recorded when its last fd goes. AF_UNIX and netlink are this machine's own talk
//! and stay out of it.

use super::Caller;
use super::inet::InetSock;
use uapi::errno::*;
use vfs::Errno;
use vfs::access::{self, Kind, NetAddr};

fn proto(i: &InetSock) -> u8 {
    i.protocol() as u8
}

/// connect(2) of an inet socket: an attempt that is under way (EINPROGRESS) counts.
pub fn connect(i: &InetSock, raw: &[u8], c: &Caller, nonblock: bool) -> Result<i64, Errno> {
    let r = i.connect(raw, c, nonblock);
    if matches!(r, Ok(()) | Err(EINPROGRESS))
        && let Some(a) = NetAddr::from_sockaddr(proto(i), raw)
    {
        access::net(Kind::Connect, c.pid, c.uid, a);
        i.acc.set_peer(c.pid, c.uid, a);
    }
    r.map(|_| 0)
}

/// listen(2): the local address it waits on.
pub fn listen(i: &InetSock, backlog: usize, c: &Caller) -> Result<i64, Errno> {
    i.listen(backlog, c)?;
    if access::on()
        && let Ok(name) = i.getname(false)
        && let Some(a) = NetAddr::from_sockaddr(proto(i), &name)
    {
        access::net(Kind::Listen, c.pid, c.uid, a);
    }
    Ok(0)
}

/// accept(2) gave connection `n` from `peer`.
pub fn accepted(n: &InetSock, peer: &[u8], c: &Caller) {
    if let Some(a) = NetAddr::from_sockaddr(proto(n), peer) {
        access::net(Kind::Accept, c.pid, c.uid, a);
        n.acc.set_peer(c.pid, c.uid, a);
    }
}
