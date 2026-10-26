//! The UDP peers of one socket in the connection view (PM-2). UDP has no connection, so each peer a socket
//! sends to or hears from is one entry (Opened when the socket sent first, Accepted when the peer did), counting
//! the bytes of each datagram; all end when the socket goes (the net stack holds a `UdpPeers` per socket).
//!
//! Anyone can send a datagram from any address, so a socket keeps at most `udp_peer_keep` peers of its own (a
//! setting); datagrams of further peers count in one entry "(more peers)" at 0.0.0.0:0. That bounds the memory
//! and the events at the socket's close. DNS answers name connections only when they come from a peer this
//! socket sent to on port 53 (what a resolver checks too: glibc's res_send takes answers only from the servers
//! it asked), so another host on the LAN cannot label a program's connections.

use crate::view::{open, ConnHandle};
use crate::{dns, IpAddr, NetProto, Subject};
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::SpinLock;

static KEEP: AtomicUsize = AtomicUsize::new(64);

/// Peers each UDP socket shows on their own (the rest share one entry). Applies to peers seen from now on.
pub fn set_udp_peer_keep(n: usize) {
    KEEP.store(n.max(1), Ordering::Relaxed);
}

pub fn udp_peer_keep() -> usize {
    KEEP.load(Ordering::Relaxed)
}

struct Peers {
    map: BTreeMap<([u8; 17], u16), ConnHandle>,
    more: Option<ConnHandle>,
}

pub struct UdpPeers {
    owner: Subject,
    local_port: u16,
    peers: SpinLock<Peers>,
}

impl UdpPeers {
    pub const fn new(owner: Subject, local_port: u16) -> UdpPeers {
        UdpPeers { owner, local_port, peers: SpinLock::named("policy.udp", Peers { map: BTreeMap::new(), more: None }) }
    }

    /// A datagram of `len` bytes went to `to:port`.
    pub fn sent(&self, to: IpAddr, port: u16, len: usize) {
        self.note(to, port, false, len);
    }

    /// A datagram of `len` bytes came from `from:port`; `data` is its payload (None for a peek: nothing is
    /// counted twice). An answer from a DNS server this socket asked names the program's connections.
    pub fn received(&self, from: IpAddr, port: u16, len: usize, data: Option<&[u8]>) {
        let asked = self.note(from, port, true, len);
        if let Some(d) = data.filter(|_| port == 53 && asked) {
            dns::note_dns_answer(&self.owner, d);
        }
    }

    /// Peers with an entry of their own.
    pub fn peers(&self) -> usize {
        self.peers.lock().map.len()
    }

    /// Counts the datagram; true if this socket has sent to the peer.
    fn note(&self, peer: IpAddr, port: u16, incoming: bool, len: usize) -> bool {
        let add = |h: &ConnHandle| if incoming { h.add_in(len as u64) } else { h.add_out(len as u64) };
        let key = (dns::ip_key(peer), port);
        let mut p = self.peers.lock();
        if let Some(h) = p.map.get(&key) {
            add(h);
            return h.bytes().1 > 0;
        }
        if p.map.len() < udp_peer_keep() {
            let h = open(self.owner, NetProto::Udp, incoming, self.local_port, peer, port);
            add(&h);
            let sent = h.bytes().1 > 0;
            p.map.insert(key, h);
            return sent;
        }
        let (owner, local_port) = (self.owner, self.local_port);
        let more = p.more.get_or_insert_with(|| {
            klog::event!(Info, "pm", "udp.more_peers", program = owner.program.0, pid = owner.pid, local_port = local_port,
                keep = udp_peer_keep(); "further peers of this socket share one entry");
            let h = open(owner, NetProto::Udp, incoming, local_port, IpAddr::V4([0; 4]), 0);
            h.label("(more peers)");
            h
        });
        add(more);
        false
    }
}
