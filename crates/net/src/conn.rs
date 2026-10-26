//! What a program's TCP socket needs of a connection beyond `C: KSocket`'s `TcpStream` (NW-5): calls that never
//! wait, peeking, half-close and readiness. The smoltcp streams and the 127.0.0.1 loopback streams implement it;
//! accept queues hold connections as `Conn`, kernel users still see `TcpStream`.

use crate::{NetError, TcpStream};

/// A connection's state as a socket layer sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnState {
    /// The handshake is under way.
    pub connecting: bool,
    /// Bytes wait to be read.
    pub readable: bool,
    /// The peer finished sending (FIN): reads end after the queued bytes.
    pub eof: bool,
    /// A send would queue something now.
    pub writable: bool,
    /// The connection failed or was reset (reported once by the socket layer).
    pub error: Option<NetError>,
    /// Nothing more can be sent (closed, or our FIN is out).
    pub closed: bool,
}

pub trait Conn: TcpStream {
    /// Without waiting: Ok(Some(n > 0)) bytes, Ok(Some(0)) end of stream, Ok(None) nothing yet. `peek` leaves the
    /// bytes queued.
    fn try_recv(&self, buf: &mut [u8], peek: bool) -> Result<Option<usize>, NetError>;
    /// Drops the first `n` queued bytes without copying them (after a peek whose copy to the program worked:
    /// a failed copy leaves the bytes queued, as `tcp_recvmsg`). Returns how many went.
    fn discard(&self, n: usize) -> Result<usize, NetError>;
    /// Without waiting: queues what fits (Ok(0) = no room now). Err(Closed) after `finish`.
    fn try_send(&self, buf: &[u8]) -> Result<usize, NetError>;
    fn state(&self) -> ConnState;
    /// shutdown(SHUT_WR): FIN after the queued bytes.
    fn finish(&self);
    /// Bytes waiting to be read (SIOCINQ).
    fn recv_queue(&self) -> usize;
    /// Bytes queued and not yet acknowledged (SIOCOUTQ).
    fn send_queue(&self) -> usize;
    fn set_nodelay(&self, _on: bool) {}
    fn set_keepalive(&self, _on: bool) {}
    /// The program owning it (its connection records name that program, B3).
    fn set_owner(&self, _who: policy::Subject) {}
    /// The socket layer saw the handshake finish (the connection record is written then, not inside `state`,
    /// which wait conditions call).
    fn on_established(&self) {}
    /// NW-7: a counter that moves with every change `state` could show (smoltcp's wakes, a reset). A waiter on
    /// the shared `inet::READY` skips the look (and the card's lock) while it stands still. None: always look.
    fn change_seq(&self) -> Option<u64> {
        None
    }
}
