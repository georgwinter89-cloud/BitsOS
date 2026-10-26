//! One smoltcp interface on one network card: the card adapter (smoltcp `Device` over `NetDevice`), the
//! interface with its socket set, DHCP, the listening sockets of the stack's listeners, and orphaned
//! connections that still close in the background. Everything runs under the card's lock, briefly: one poll
//! handles at most `RX_BUDGET` received frames (NAPI style), the rest comes with the next round.

use super::dhcp::{Action, DhcpState, Lease};
use super::phy::{Phy, TxState};
use super::{tcp, ListenShared, Stack};
use crate::accept::AcceptQueue;
use crate::{Ipv4, NetError, SockAddr};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use devmodel::net::NetDevice;
use ksync::SpinLock;
use smoltcp::iface::{Config, Interface, PollIngressSingleResult, SocketHandle, SocketSet};
use smoltcp::socket::{dhcpv4, tcp as stcp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, IpListenEndpoint, Ipv4Address, Ipv4Cidr};

/// Frames taken from the card per poll round (bounded work under the lock, B4). NW-7: Linux NAPI's weight
/// (linux-6.6/include/linux/netdevice.h:2615 `NAPI_POLL_WEIGHT`); 32 paused the poll thread twice as often.
pub const RX_BUDGET: usize = 64;
/// Connections a listener holds beyond its backlog (listening, in handshake or waiting in the accept queue): a
/// burst of SYNs (node/undici open the next connection before closing the idle one, spec §2.4 "knock") finds a
/// listening socket instead of an RST. Linux keeps backlog + 1 in its accept queue.
pub const SYN_SPARE: usize = 2;
/// A reserved listener (MCP, `tcp_listen_private`) keeps at least this many listening sockets per card from the
/// kept pool (V2-PLAN §6, Georg's K3b decision: MCP accepts 3 connections whatever programs do to memory).
pub const RESERVE_MIN: usize = 3;
/// A program's listener makes at most this many listening sockets ahead per card (each carries its buffers); the
/// rest of its backlog is made as connections are taken (NW-5).
pub const PROG_PRECREATE: usize = 4;
/// A closed connection nobody owns any more is aborted (RST) after this long: queued data and FIN within 30 s,
/// else RST (spec net.md §2.4, V1's rule; also FIN_WAIT/LAST_ACK that never ends).
pub const ORPHAN_NS: u64 = 30_000_000_000;

pub fn instant(ns: u64) -> Instant {
    Instant::from_micros((ns / 1000) as i64)
}

pub fn v4(a: Ipv4) -> Ipv4Address {
    Ipv4Address::from(a.0)
}

pub fn from_v4(a: Ipv4Address) -> Ipv4 {
    Ipv4(a.octets())
}

// ---------------------------------------------------------------------------------------------------------
// The interface

/// What routing and `interfaces()` see of a card (kept beside the lock so readers never wait for a poll).
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub link_up: bool,
    pub speed_mbps: u32,
    pub addr: Option<(Ipv4, u8)>,
    pub router: Option<Ipv4>,
    pub dns: Vec<Ipv4>,
    /// Order of this card's first DHCP lease among all cards.
    pub lease_seq: Option<u64>,
    pub lease_expires_ns: Option<u64>,
    pub dhcp: bool,
    /// The card's route metric (`NetDevice::route_metric`, read at registration; WL-4 CR 1).
    pub metric: u32,
}

struct ListenSock {
    l: Arc<ListenShared>,
    handle: SocketHandle,
    /// From the stack's kept pool (a reserved listener): goes back there, never to the heap.
    kept: bool,
}

struct Orphan {
    handle: SocketHandle,
    deadline_ns: u64,
    /// When the deadline's abort was made (its RST then has one second to leave).
    aborted_ns: Option<u64>,
    kept: bool,
}

pub(crate) struct Inner {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    dhcp: Option<SocketHandle>,
    dhcp_state: DhcpState,
    rx: Vec<u8>,
    tx: Vec<u8>,
    txs: TxState,
    listen: Vec<ListenSock>,
    orphans: Vec<Orphan>,
    discovers_logged: u32,
    /// A refill found no memory (logged once until a refill succeeds again).
    listen_short: bool,
}

pub struct Iface {
    pub index: usize,
    pub name: String,
    pub dev: Arc<dyn NetDevice>,
    pub mac: [u8; 6],
    pub(crate) inner: SpinLock<Inner>,
    pub(crate) info: SpinLock<Info>,
    /// Carrier changes seen (Linux `carrier_up_count`/`carrier_down_count`; a card up at registration counts one
    /// up, as `netif_carrier_on` at probe does): netlink's IFLA_CARRIER_* (NW-6).
    pub(crate) carrier_up: AtomicU32,
    pub(crate) carrier_down: AtomicU32,
}

/// Things that happened during a poll, logged by the stack after the card's lock is released.
pub(crate) enum Note {
    Link { up: bool, speed_mbps: u32 },
    Lease(Lease),
    Renewed(Lease),
    Lost,
    Restart { keep: Option<Ipv4> },
    Discover { attempt: u32, next_ns: u64 },
    TxDropped(u32),
    /// Listening sockets could not all be refilled for lack of memory (true), or can again (false).
    ListenShort(bool),
}

/// DISCOVER interval while a card has no link: effectively never (link-up restarts DHCP at once).
const NO_LINK_WAIT_S: u64 = 24 * 3600;

#[derive(Default)]
pub(crate) struct Report {
    pub activity: bool,
    pub busy: bool,
    pub next_ns: Option<u64>,
    pub accepted: Vec<(Arc<AcceptQueue>, Box<dyn crate::conn::Conn>)>,
    pub notes: Vec<Note>,
}

fn ep_addr(e: smoltcp::wire::IpEndpoint) -> SockAddr {
    let smoltcp::wire::IpAddress::Ipv4(a) = e.addr;
    SockAddr { ip: from_v4(a), port: e.port }
}

impl Iface {
    pub(crate) fn new(index: usize, dev: Arc<dyn NetDevice>, now_ns: u64, seed: u64, dhcp: bool) -> Self {
        let mac = dev.mac().0;
        let metric = dev.route_metric();
        let mtu = dev.mtu().max(576);
        let mut rx = alloc::vec![0u8; mtu + 18];
        let mut tx = Vec::new();
        let mut txs = TxState::default();
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        // TCP initial sequence numbers and DHCP xids come from this. Until the stack's seed has RDRAND in it
        // (change request to CP), each card adds what differs between machines and boots: its MAC and the
        // nanosecond it came up (driver bring-up time jitters), all spread by SplitMix64. Guessable for an
        // attacker who can time the boot; enough that a Reload never repeats the previous kernel's numbers.
        let mac64 = u64::from_be_bytes([0, 0, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
        cfg.random_seed = super::mix64(seed ^ super::mix64(mac64 ^ now_ns.rotate_left(17)));
        let iface = {
            let mut phy = Phy { dev: &*dev, rx: &mut rx, tx: &mut tx, st: &mut txs, rx_left: 0, rx_frames: 0, mtu };
            Interface::new(cfg, &mut phy, instant(now_ns))
        };
        let mut sockets = SocketSet::new(Vec::new());
        let dhcp_handle = dhcp.then(|| {
            let mut d = dhcpv4::Socket::new();
            // smoltcp hands out the lease time only with the whole packet. Cards live as long as the kernel, so
            // the buffer is leaked once per card (one MTU).
            d.set_receive_packet_buffer(alloc::boxed::Box::leak(alloc::vec![0u8; 1536].into_boxed_slice()));
            sockets.add(d)
        });
        let name = String::from(dev.name());
        let link = dev.link();
        Iface {
            index,
            name,
            dev,
            mac,
            inner: SpinLock::named("net.iface", Inner {
                iface,
                sockets,
                dhcp: dhcp_handle,
                dhcp_state: DhcpState::new(),
                rx,
                tx,
                txs,
                listen: Vec::new(),
                orphans: Vec::new(),
                discovers_logged: 0,
                listen_short: false,
            }),
            info: SpinLock::named("net.iface.info", Info { dhcp, link_up: link.up, speed_mbps: link.speed_mbps, metric, ..Info::default() }),
            carrier_up: AtomicU32::new(link.up as u32),
            carrier_down: AtomicU32::new(0),
        }
    }

    pub fn info(&self) -> Info {
        self.info.lock().clone()
    }

    /// A fixed address instead of DHCP (host tests; later a CONFIG key).
    pub(crate) fn set_static(&self, addr: Ipv4, prefix: u8, router: Option<Ipv4>) {
        let mut g = self.inner.lock();
        let inner = &mut *g;
        if let Some(h) = inner.dhcp.take() {
            inner.sockets.remove(h);
        }
        apply_addr(&mut inner.iface, Some((addr, prefix)), router);
        let mut i = self.info.lock();
        i.addr = Some((addr, prefix));
        i.router = router;
        i.dhcp = false;
    }

    /// Hands a socket whose owner went away to the background closer (`kept`: it returns to the kept pool when
    /// done).
    pub(crate) fn orphan(inner: &mut Inner, handle: SocketHandle, now_ns: u64, kept: bool) {
        inner.orphans.push(Orphan { handle, deadline_ns: now_ns.saturating_add(ORPHAN_NS), aborted_ns: None, kept });
    }

    /// One round: link, DHCP timing, up to `RX_BUDGET` frames in, everything queued out, timers, DHCP events,
    /// listeners, orphans. Returns when the card wants the next round.
    pub(crate) fn poll(self: &Arc<Self>, stack: &'static Stack, now_ns: u64, listeners: &[Arc<ListenShared>]) -> Report {
        let mut rep = Report::default();
        let now = instant(now_ns);
        let link = self.dev.link();
        let mtu = self.dev.mtu().max(576);
        let mut g = self.inner.lock();
        let inner = &mut *g;

        // Link watch: lost = keep the address; back = ask DHCP again at once (another network may be there).
        let was_up = self.info.lock().link_up;
        if was_up != link.up {
            rep.notes.push(Note::Link { up: link.up, speed_mbps: link.speed_mbps });
            let mut i = self.info.lock();
            i.link_up = link.up;
            i.speed_mbps = link.speed_mbps;
            drop(i);
            if link.up {
                self.carrier_up.fetch_add(1, Ordering::Relaxed);
            } else {
                self.carrier_down.fetch_add(1, Ordering::Relaxed);
            }
            if link.up {
                // Another network may be behind the cable with the same gateway address: forget every learnt
                // MAC as Linux does on a carrier change (smoltcp flushes its neighbour cache on an address
                // update; the addresses stay as they are). An address change by DHCP flushes it too.
                inner.iface.update_ip_addrs(|_| {});
            }
        }
        if inner.dhcp_state.link(link.up) == Action::Restart
            && let Some(h) = inner.dhcp
        {
            inner.sockets.get_mut::<dhcpv4::Socket>(h).reset();
            inner.discovers_logged = 0;
            rep.notes.push(Note::Restart { keep: inner.dhcp_state.lease.as_ref().map(|l| l.addr) });
        }
        if let Some(h) = inner.dhcp {
            let sock = inner.sockets.get_mut::<dhcpv4::Socket>(h);
            let mut rc = sock.get_retry_config();
            // Without a cable nobody can answer: no DISCOVERs then (the second I226 of the Mini-PC flooded the log
            // with dhcp.retry). Link-up resets the socket (`Restart` above), which asks at once.
            let want = if link.up { Duration::from_micros(inner.dhcp_state.next_interval_ns() / 1000) } else { Duration::from_secs(NO_LINK_WAIT_S) };
            if rc.discover_timeout != want {
                rc.discover_timeout = want;
                sock.set_retry_config(rc);
            }
        }

        // Frames in (bounded), frames out, timers.
        inner.txs.full = false;
        let (rx_frames, changed) = {
            let mut phy = Phy { dev: &*self.dev, rx: &mut inner.rx, tx: &mut inner.tx, st: &mut inner.txs, rx_left: RX_BUDGET, rx_frames: 0, mtu };
            let mut changed = false;
            loop {
                match inner.iface.poll_ingress_single(now, &mut phy, &mut inner.sockets) {
                    PollIngressSingleResult::None => break,
                    PollIngressSingleResult::PacketProcessed => {}
                    PollIngressSingleResult::SocketStateChanged => {
                        changed = true;
                        // Before this round's egress answers the SYN with a SYN-ACK.
                        refuse_public(&mut inner.sockets, &inner.listen);
                    }
                }
            }
            // The stall rule before smoltcp's timers run: only a connection that waits for the peer may time out.
            for (_, sock) in inner.sockets.iter_mut() {
                if let smoltcp::socket::Socket::Tcp(t) = sock {
                    tcp::stall_rule(t);
                }
            }
            // Egress is bounded per call; a few calls drain what the sockets have queued.
            for _ in 0..8 {
                match inner.iface.poll_egress(now, &mut phy, &mut inner.sockets) {
                    smoltcp::iface::PollResult::None => break,
                    smoltcp::iface::PollResult::SocketStateChanged => changed = true,
                }
            }
            inner.iface.poll_maintenance(now);
            (phy.rx_frames, changed)
        };
        // A frame held for a full TX ring makes the round busy: the poll thread is back after its short pause.
        rep.busy = rx_frames >= RX_BUDGET || inner.txs.holding;
        rep.activity = rx_frames > 0 || changed;
        if inner.txs.requeued > 0 {
            super::TX_REQUEUED.add(inner.txs.requeued as u64);
            inner.txs.requeued = 0;
        }
        if inner.txs.dropped > 0 {
            rep.notes.push(Note::TxDropped(inner.txs.dropped));
            inner.txs.dropped = 0;
        }

        // DHCP: DISCOVERs sent, a lease, a renew, a loss.
        if inner.txs.discovers > 0 {
            for _ in 0..inner.txs.discovers {
                inner.dhcp_state.discover_sent();
            }
            inner.txs.discovers = 0;
            let attempt = inner.dhcp_state.discovers;
            if attempt > inner.discovers_logged {
                inner.discovers_logged = attempt;
                rep.notes.push(Note::Discover { attempt, next_ns: inner.dhcp_state.next_interval_ns() });
            }
        }
        if let Some(h) = inner.dhcp {
            let ev = match inner.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
                None => None,
                Some(dhcpv4::Event::Configured(c)) => {
                    let lease_s = c.packet.as_ref().and_then(|p| smoltcp::wire::DhcpRepr::parse(p).ok()).and_then(|r| r.lease_duration);
                    Some(Some(Lease {
                        addr: from_v4(c.address.address()),
                        prefix: c.address.prefix_len(),
                        router: c.router.map(from_v4),
                        dns: c.dns_servers.iter().map(|d| from_v4(*d)).collect(),
                        server: from_v4(c.server.address),
                        // 0xFFFF_FFFF = infinite (RFC 2131).
                        lease_ns: lease_s.filter(|s| *s != u32::MAX).map(|s| s as u64 * 1_000_000_000),
                        got_at_ns: now_ns,
                    }))
                }
                Some(dhcpv4::Event::Deconfigured) => Some(None),
            };
            let action = match ev {
                None => inner.dhcp_state.tick(now_ns),
                Some(Some(l)) => inner.dhcp_state.configured(l),
                Some(None) => inner.dhcp_state.deconfigured(now_ns),
            };
            match action {
                Action::Apply(l) => {
                    apply_addr(&mut inner.iface, Some((l.addr, l.prefix)), l.router);
                    let mut i = self.info.lock();
                    i.addr = Some((l.addr, l.prefix));
                    i.router = l.router;
                    i.dns = l.dns.clone();
                    i.lease_expires_ns = l.expires_at_ns();
                    if i.lease_seq.is_none() {
                        i.lease_seq = Some(stack.next_lease_seq());
                    }
                    drop(i);
                    inner.discovers_logged = 0;
                    rep.notes.push(Note::Lease(l));
                    rep.activity = true;
                }
                Action::Renewed => {
                    if let Some(l) = inner.dhcp_state.lease.clone() {
                        let mut i = self.info.lock();
                        i.lease_expires_ns = l.expires_at_ns();
                        i.dns = l.dns.clone();
                        drop(i);
                        rep.notes.push(Note::Renewed(l));
                    }
                }
                Action::Remove => {
                    apply_addr(&mut inner.iface, None, None);
                    let mut i = self.info.lock();
                    i.addr = None;
                    i.router = None;
                    i.dns.clear();
                    i.lease_expires_ns = None;
                    drop(i);
                    rep.notes.push(Note::Lost);
                    rep.activity = true;
                }
                Action::Nothing | Action::Restart => {}
            }
        }

        // Listeners: connections that completed the handshake go to their accept queue; sockets are refilled
        // up to the backlog; a dropped listener's sockets go.
        self.listeners(stack, inner, now_ns, listeners, &mut rep);

        // Orphans: gone once closed (an abort's RST has left: no endpoint any more) or after their deadline. A
        // kept socket goes back to the stack's pool (the MCP reserve), every other one is freed.
        let sockets = &mut inner.sockets;
        inner.orphans.retain_mut(|o| {
            let s = sockets.get_mut::<stcp::Socket>(o.handle);
            let done = matches!(s.state(), stcp::State::TimeWait) || (s.state() == stcp::State::Closed && s.remote_endpoint().is_none());
            // One more second after the abort for its RST (a peer without ARP answer never gets it). Counted from
            // the abort, not the deadline: a late round (a poll gap) still sends the RST first.
            if done || o.aborted_ns.is_some_and(|t| now_ns >= t.saturating_add(1_000_000_000)) {
                let sock = sockets.remove(o.handle);
                if o.kept {
                    stack.give_back(sock);
                }
                return false;
            }
            if o.aborted_ns.is_none() && now_ns >= o.deadline_ns {
                s.abort();
                o.aborted_ns = Some(now_ns);
            }
            true
        });

        rep.next_ns = inner.iface.poll_at(now, &inner.sockets).map(|t| (t.total_micros().max(0) as u64).saturating_mul(1000));
        rep
    }

    fn listeners(self: &Arc<Self>, stack: &'static Stack, inner: &mut Inner, now_ns: u64, live: &[Arc<ListenShared>], rep: &mut Report) {
        let mut i = 0;
        while i < inner.listen.len() {
            let ls = &inner.listen[i];
            if ls.l.queue.is_closed() {
                let ls = inner.listen.swap_remove(i);
                let sock = inner.sockets.remove(ls.handle);
                if ls.kept {
                    stack.give_back(sock);
                }
                continue;
            }
            let s = inner.sockets.get_mut::<stcp::Socket>(ls.handle);
            match s.state() {
                stcp::State::Listen | stcp::State::SynReceived => i += 1,
                stcp::State::Closed => {
                    // A handshake that ended in a reset: listen again.
                    let ep = s.listen_endpoint();
                    let _ = s.listen(ep);
                    i += 1;
                }
                _ => {
                    let (local, peer) = match (s.local_endpoint(), s.remote_endpoint()) {
                        (Some(l), Some(r)) => (ep_addr(l), ep_addr(r)),
                        _ => {
                            i += 1;
                            continue;
                        }
                    };
                    let ls = inner.listen.swap_remove(i);
                    let st = tcp::SmolStream::accepted(stack, self.clone(), ls.handle, local, peer, ls.kept, ls.l.owner);
                    rep.accepted.push((ls.l.queue.clone(), Box::new(st)));
                    rep.activity = true;
                }
            }
        }
        let mut short = false;
        for l in live {
            // Connections taken from the card this round reach the queue only after the card's lock is gone.
            let handed = rep.accepted.iter().filter(|(q, _)| Arc::ptr_eq(q, &l.queue)).count();
            short |= refill(stack, inner, l, handed, true).short;
        }
        if short != inner.listen_short {
            inner.listen_short = short;
            rep.notes.push(Note::ListenShort(short));
        }
        let _ = now_ns;
    }

    #[cfg(test)]
    pub(crate) fn orphan_count(&self) -> usize {
        self.inner.lock().orphans.len()
    }

    /// Tests: removes listening sockets until `keep` are left (as accepted connections would take them).
    #[cfg(test)]
    pub(crate) fn shrink_listen(&self, stack: &Stack, keep: usize) {
        let mut g = self.inner.lock();
        let inner = &mut *g;
        while inner.listen.len() > keep {
            let ls = inner.listen.swap_remove(0);
            let sock = inner.sockets.remove(ls.handle);
            // A kept socket goes back to the pool like at every other removal site (`kept_made` stays exact).
            if ls.kept {
                stack.give_back(sock);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn listen_short(&self) -> bool {
        self.inner.lock().listen_short
    }

    /// This listener's sockets on the card (listening, in handshake, established but not yet queued).
    #[cfg(test)]
    pub(crate) fn listen_count(&self, l: &Arc<ListenShared>) -> usize {
        self.inner.lock().listen.iter().filter(|s| Arc::ptr_eq(&s.l, l)).count()
    }

    /// A new listener: its listening sockets exist on this card before `tcp_listen` returns (as many as memory
    /// allows; the poll rounds refill the rest).
    pub(crate) fn add_listener(&self, stack: &Stack, l: &Arc<ListenShared>) -> Refill {
        refill(stack, &mut self.inner.lock(), l, 0, false)
    }

    /// A dropped listener: its sockets go at once (kept ones back to the pool); a handshake in progress is reset.
    pub(crate) fn drop_listener(&self, stack: &Stack, l: &Arc<ListenShared>, now_ns: u64) {
        let mut g = self.inner.lock();
        let inner = &mut *g;
        let mut i = 0;
        while i < inner.listen.len() {
            if !Arc::ptr_eq(&inner.listen[i].l, l) {
                i += 1;
                continue;
            }
            let ls = inner.listen.swap_remove(i);
            let s = inner.sockets.get_mut::<stcp::Socket>(ls.handle);
            if s.state() == stcp::State::Listen {
                let sock = inner.sockets.remove(ls.handle);
                if ls.kept {
                    stack.give_back(sock);
                }
            } else {
                s.abort();
                Self::orphan(inner, ls.handle, now_ns, ls.kept);
            }
        }
    }
}

/// Spec §2.4 (V1, Georg's decision 08.10.): a private-only listener (MCP) answers a SYN from outside 10/8,
/// 172.16/12 and 192.168/16 (`is_rfc1918`: link-local is refused too, as in V1's `tcp_private`) with RST.
/// smoltcp has put the listening socket into SYN-RECEIVED; it is aborted before the SYN-ACK leaves (egress sends
/// the RST instead) and listens again in `listeners`: no connection, no new socket, nothing allocated for the
/// scanner.
fn refuse_public(sockets: &mut SocketSet<'static>, listen: &[ListenSock]) {
    for ls in listen.iter().filter(|ls| ls.l.private_only) {
        let s = sockets.get_mut::<stcp::Socket>(ls.handle);
        if s.state() != stcp::State::SynReceived {
            continue;
        }
        let Some(peer) = s.remote_endpoint().map(ep_addr) else { continue };
        if peer.ip.is_rfc1918() {
            continue;
        }
        s.abort();
        super::TCP_REFUSED_PUBLIC.inc();
        klog::event!(Debug, "net", "tcp.refused_public", peer = super::ip_str(peer.ip), port = ls.l.port; "private networks only (RST)");
    }
}

/// What a refill did: `have` sockets for the listener on the card afterwards; `short` = memory ran out first.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Refill {
    pub have: usize,
    pub short: bool,
}

/// Keeps `backlog + SYN_SPARE` connections possible for `l` on this card: listening sockets, handshakes in
/// progress and the accept queue together (bounded: a SYN beyond that gets RST). `handed`: connections on their
/// way to the queue (taken from this card in this round, not yet pushed). Stops at the first buffer the heap
/// refuses (R2); the next round tries again. A reserved listener (the MCP reserve) takes its sockets from the
/// stack's kept pool first: they were made while memory was there and come back after each connection, so a
/// heap that a program (or a thousand connections) emptied still leaves MCP its listening sockets.
/// `background` (the poll round): after a refusal the heap is asked again only after `Stack::bg_alloc_open`'s
/// pause (NW-7); a program's own `listen` always asks.
fn refill(stack: &Stack, inner: &mut Inner, l: &Arc<ListenShared>, handed: usize, background: bool) -> Refill {
    let mut have = inner.listen.iter().filter(|s| Arc::ptr_eq(&s.l, l)).count();
    if l.queue.is_closed() {
        return Refill { have, short: false };
    }
    let mut want = listen_want(l).saturating_sub(l.queue.len() + handed);
    if l.prog.is_some() {
        want = want.min(PROG_PRECREATE);
    }
    while have < want {
        let (rx, tx) = l.prog.unwrap_or((tcp::LISTEN_RX, tcp::LISTEN_TX));
        let made = if l.reserve {
            stack.take_kept()
        } else if background && !stack.bg_alloc_open() {
            Err(NetError::NoMemory)
        } else {
            let m = tcp::new_socket(stack, rx, tx);
            if background {
                stack.bg_alloc_done(m.is_ok());
            }
            m
        };
        let Ok(mut s) = made else {
            return Refill { have, short: true };
        };
        tcp::tune_listen(&mut s);
        let ep = IpListenEndpoint { addr: (!l.ip.is_unspecified()).then(|| v4(l.ip).into()), port: l.port };
        if s.listen(ep).is_err() {
            if l.reserve {
                stack.give_back(s);
            }
            break;
        }
        let handle = inner.sockets.add(s);
        inner.listen.push(ListenSock { l: l.clone(), handle, kept: l.reserve });
        have += 1;
    }
    Refill { have, short: false }
}

/// Connections a listener keeps possible per card: its backlog plus the spare for a knocking client, and for a
/// reserved listener never fewer than `RESERVE_MIN` (V2-PLAN §6: at least 3 for MCP).
pub(crate) fn listen_want(l: &ListenShared) -> usize {
    let w = l.backlog.max(1) + SYN_SPARE;
    if l.reserve { w.max(RESERVE_MIN) } else { w }
}

/// Puts the address and default route on the smoltcp interface (None: takes them off).
fn apply_addr(iface: &mut Interface, addr: Option<(Ipv4, u8)>, router: Option<Ipv4>) {
    iface.update_ip_addrs(|a| {
        a.clear();
        if let Some((ip, p)) = addr {
            let _ = a.push(IpCidr::Ipv4(Ipv4Cidr::new(v4(ip), p)));
        }
    });
    match router {
        Some(r) => {
            let _ = iface.routes_mut().add_default_ipv4_route(v4(r));
        }
        None => {
            iface.routes_mut().remove_default_ipv4_route();
        }
    }
}
