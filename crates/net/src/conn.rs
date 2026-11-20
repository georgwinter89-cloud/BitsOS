//! What a program's TCP socket needs of a connection beyond `C: KSocket`'s `TcpStream` (NW-5): calls that never
//! wait, peeking, half-close and readiness. The smoltcp streams and the 127.0.0.1 loopback streams implement it;
//! accept queues hold connections as `Conn`, kernel users still see `TcpStream`.

use crate::{NetError, SockAddr, TcpStream};
use alloc::sync::Arc;

/// The record of a connection a program opened, still to be written (`Conn::on_established`). Written once per
/// connection whoever gets there first: this, or the connection's end.
pub struct Opened {
    pub(crate) conn: Arc<ksync::Once<policy::ConnHandle>>,
    pub(crate) bytes: Arc<policy::ConnCounters>,
    pub(crate) who: policy::Subject,
    pub(crate) local_port: u16,
    pub(crate) peer: SockAddr,
}

impl Opened {
    /// With no spin lock held: the connection view's entry and its event record.
    pub fn write(self) {
        let Opened { conn, bytes, who, local_port, peer } = self;
        conn.get_or_init(|| policy::open_counted(who, policy::NetProto::Tcp, false, local_port, policy::IpAddr::V4(peer.ip.0), peer.port, bytes));
    }
}

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
    /// TCP_QUICKACK: false is Linux's pingpong mode (delayed ACKs), true leaves it (NW-10 fixer).
    fn set_quickack(&self, _on: bool) {}
    /// The program owning it (its connection records name that program, B3).
    fn set_owner(&self, _who: policy::Subject) {}
    /// The socket layer saw the handshake finish (the connection record is written then, not inside `state`,
    /// which wait conditions call). Called under the socket's spin lock: the record, which allocates, comes back
    /// to be written once that lock is gone (`Opened::write`).
    fn on_established(&self) -> Option<Opened> {
        None
    }
    /// A listener's card took this connection out of its round: the connection record of an accepted
    /// connection is written here, after the card's lock, before the accept queue has it.
    fn on_accepted(&self) {}
    /// NW-7: a counter that moves with every change `state` could show (smoltcp's wakes, a reset). A waiter on
    /// the shared `inet::READY` skips the look (and the card's lock) while it stands still. None: always look.
    fn change_seq(&self) -> Option<u64> {
        None
    }
}
