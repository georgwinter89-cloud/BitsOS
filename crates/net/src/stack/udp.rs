//! UDP sockets and ICMP echo over smoltcp. A UDP socket bound to a port has one smoltcp socket on every card
//! (cards that come later get theirs on the next call); a datagram leaves through the card routing picks, a
//! broadcast through every card with link. `ping` sends one echo request and waits for its answer.

use super::iface::{self, Iface};
use super::wake::SockWake;
use super::Stack;
use crate::{Ipv4, NetError, SockAddr, UdpSocket};
use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::task::Waker;
use ksync::SpinLock;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{icmp, udp};
use smoltcp::wire::{IpAddress, IpListenEndpoint};

/// Per card and socket: datagrams waiting to be read or sent (count and bytes).
const UDP_PACKETS: usize = 64;
const UDP_BYTES: usize = 128 * 1024;
/// A full send buffer is waited for this long before the datagram is refused (B4: bounded).
const SEND_WAIT_NS: u64 = 200_000_000;
/// Waits re-check for new cards at least this often.
const SLICE_NS: u64 = 100_000_000;

pub struct SmolUdp {
    stack: &'static Stack,
    port: u16,
    ip: Ipv4,
    socks: SpinLock<Vec<(Arc<Iface>, SocketHandle)>>,
    /// The connection view's entries of this socket's peers (PM-2): bytes each way; they end with the socket.
    peers: policy::UdpPeers,
    /// The port is this socket's to give back (a program socket's port belongs to its `PortGuard`).
    owns_port: bool,
    /// A program's socket (NW-7): smoltcp wakes `inet::READY` through it when a datagram or room arrives.
    waker: Option<Waker>,
    /// The same wake object: its change counter lets a waiter skip a look while nothing happened (NW-7 fixer).
    wake: Option<Arc<SockWake>>,
}

fn prog_waker(stack: &Stack) -> (Option<Waker>, Option<Arc<SockWake>>) {
    let w = SockWake::new(stack.wakes.clone());
    w.set_prog(true);
    (Some(Waker::from(w.clone())), Some(w))
}

/// A UDP port held in the stack's port table until dropped (NW-5: a program socket bound to 127.0.0.1 holds its
/// port without card sockets; one bound to a card or 0.0.0.0 also has a `SmolUdp` on it).
pub struct PortGuard(pub u16);

/// A datagram for a program socket: the bytes it gets, the datagram's length, sender, destination, card.
pub(crate) struct ProgDgram {
    pub data: Vec<u8>,
    pub len: usize,
    pub from: SockAddr,
    pub to: crate::Ipv4,
    pub ifindex: u32,
    /// The card's own address (`ipi_spec_dst` for a broadcast).
    pub card: Option<crate::Ipv4>,
}

impl Drop for PortGuard {
    fn drop(&mut self) {
        UDP_PORTS.lock().remove(&self.0);
    }
}

/// Takes `port` (0: a free ephemeral one, skipping `taken`) in the UDP port table.
pub fn reserve_port(stack: &Stack, port: u16) -> Result<PortGuard, NetError> {
    let mut used = UDP_PORTS.lock();
    let port = if port == 0 {
        stack.ephemeral_port(|p| used.contains(&p)).ok_or(NetError::AddrInUse)?
    } else {
        port
    };
    if !used.insert(port) {
        return Err(NetError::AddrInUse);
    }
    Ok(PortGuard(port))
}

static UDP_PORTS: SpinLock<BTreeSet<u16>> = SpinLock::named("net.udp_ports", BTreeSet::new());

/// A UDP socket, or `NoMemory` when the heap cannot give its buffers (R2).
fn new_socket(stack: &Stack) -> Result<udp::Socket<'static>, NetError> {
    let buf = || -> Result<udp::PacketBuffer<'static>, NetError> {
        Ok(udp::PacketBuffer::new(stack.buffer(UDP_PACKETS, udp::PacketMetadata::EMPTY)?, stack.buffer(UDP_BYTES, 0u8)?))
    };
    Ok(udp::Socket::new(buf()?, buf()?))
}

impl SmolUdp {
    pub(crate) fn bind(stack: &'static Stack, addr: SockAddr) -> Result<SmolUdp, NetError> {
        if addr.ip.is_loopback() {
            return Err(NetError::Unreachable);
        }
        let port = {
            let mut used = UDP_PORTS.lock();
            let port = if addr.port == 0 {
                stack.ephemeral_port(|p| used.contains(&p)).ok_or_else(|| {
                    klog::event!(Warn, "net", "udp.no_port"; "every ephemeral port is bound");
                    NetError::AddrInUse
                })?
            } else {
                addr.port
            };
            if !used.insert(port) {
                return Err(NetError::AddrInUse);
            }
            port
        };
        let u = SmolUdp { stack, port, ip: addr.ip, socks: SpinLock::named("net.udp", Vec::new()), peers: policy::UdpPeers::new(policy::Subject::KERNEL, port), owns_port: true, waker: None, wake: None };
        // Dropping `u` gives the port back.
        if u.sync().is_err() && u.socks.lock().is_empty() {
            klog::event!(Warn, "net", "udp.no_memory", port = port; "no memory for the socket buffers");
            return Err(NetError::NoMemory);
        }
        Ok(u)
    }

    /// A program's UDP socket on the cards (NW-5): `guard`'s port, bound to `ip` (0.0.0.0 or a card's address).
    pub(crate) fn bind_prog(stack: &'static Stack, ip: Ipv4, guard: &PortGuard, owner: policy::Subject) -> Result<SmolUdp, NetError> {
        let (waker, wake) = prog_waker(stack);
        let u = SmolUdp { stack, port: guard.0, ip, socks: SpinLock::named("net.udp", Vec::new()), peers: policy::UdpPeers::new(owner, guard.0), owns_port: false, waker, wake };
        if u.sync().is_err() && u.socks.lock().is_empty() && !stack.iface_list().is_empty() {
            return Err(NetError::NoMemory);
        }
        Ok(u)
    }

    /// The next datagram without waiting: up to `size` bytes, its full length, the sender. `peek` leaves it.
    pub(crate) fn try_recv_prog(&self, size: usize, peek: bool) -> Option<ProgDgram> {
        let _ = self.sync();
        let socks = self.socks.lock().clone();
        for (c, h) in socks {
            let mut g = c.inner.lock();
            let s = g.sockets.get_mut::<udp::Socket>(h);
            let got = if peek {
                s.peek().ok().map(|(d, m)| (d[..d.len().min(size)].to_vec(), d.len(), *m))
            } else {
                s.recv().ok().map(|(d, m)| (d[..d.len().min(size)].to_vec(), d.len(), m))
            };
            if let Some((data, len, m)) = got {
                drop(g);
                let IpAddress::Ipv4(a) = m.endpoint.addr;
                let from = SockAddr { ip: iface::from_v4(a), port: m.endpoint.port };
                // PM-2: also the names a program resolves, from the answers of the DNS servers it asked on its own
                // sockets (never the kernel's; policy checks both).
                let payload = (!peek).then_some(&data[..]);
                self.peers.received(policy::IpAddr::V4(from.ip.0), from.port, if peek { 0 } else { len }, payload);
                let card = c.info().addr.map(|(a, _)| a);
                let to = match m.local_address {
                    Some(IpAddress::Ipv4(l)) => iface::from_v4(l),
                    None => card.unwrap_or(crate::Ipv4::ANY),
                };
                // Linux's ifindex: lo is 1, the cards follow in registration order (`ifview`).
                return Some(ProgDgram { data, len, from, to, ifindex: c.index as u32 + 2, card });
            }
        }
        None
    }

    /// One datagram without waiting: Ok(false) = no room now. Routing as `send_to`; `dev` = the card a
    /// SO_BINDTODEVICE socket is bound to (WL-4 CR 2: only that card, also for a broadcast).
    pub(crate) fn try_send_prog(&self, buf: &[u8], to: SockAddr, dev: Option<usize>) -> Result<bool, NetError> {
        let missing = if self.sync().is_err() { NetError::NoMemory } else { NetError::Unreachable };
        self.peers.sent(policy::IpAddr::V4(to.ip.0), to.port, buf.len());
        let socks = self.socks.lock().clone();
        let one = |c: &Arc<Iface>, h: SocketHandle| -> Result<bool, NetError> {
            let mut g = c.inner.lock();
            let s = g.sockets.get_mut::<udp::Socket>(h);
            if s.payload_send_capacity() < buf.len() {
                return Err(NetError::Other);
            }
            if !s.can_send() {
                if let Some(w) = &self.waker {
                    s.register_send_waker(w);
                }
                return Ok(false);
            }
            s.send_slice(buf, (iface::v4(to.ip), to.port)).map(|_| true).map_err(|_| NetError::Other)
        };
        let r = if to.ip == Ipv4::BROADCAST {
            let mut sent = false;
            for (c, h) in socks.iter().filter(|(c, _)| c.info.lock().link_up && dev.is_none_or(|d| d == c.index)) {
                sent |= one(c, *h)?;
            }
            if socks.is_empty() { Err(missing) } else { Ok(sent) }
        } else {
            let (card, _) = self.stack.route_on(to.ip, dev).ok_or(NetError::Unreachable)?;
            let (c, h) = socks.iter().find(|(c, _)| Arc::ptr_eq(c, &card)).ok_or(missing)?;
            one(c, *h)
        };
        if r == Ok(true) {
            self.stack.kick();
        }
        r
    }

    /// The socket layer's look (its waits and polls): registers the program's waker on every card's socket.
    /// A program socket's change counter (`Conn::change_seq` for UDP); read before `readable`.
    pub(crate) fn change_seq(&self) -> Option<u64> {
        self.wake.as_ref().map(|w| w.seq())
    }

    pub(crate) fn readable(&self) -> bool {
        self.socks.lock().iter().any(|(c, h)| {
            let mut g = c.inner.lock();
            let s = g.sockets.get_mut::<udp::Socket>(*h);
            if let Some(w) = &self.waker {
                s.register_recv_waker(w);
            }
            s.can_recv()
        })
    }

    /// The bytes of the next datagram (SIOCINQ), 0 without one.
    pub(crate) fn next_len(&self) -> usize {
        let socks = self.socks.lock().clone();
        for (c, h) in socks {
            let mut g = c.inner.lock();
            if let Ok((d, _)) = g.sockets.get_mut::<udp::Socket>(h).peek() {
                return d.len();
            }
        }
        0
    }

    /// Gives every card this socket does not have yet its smoltcp socket. `NoMemory`: a card got none (the
    /// next call tries again).
    fn sync(&self) -> Result<(), NetError> {
        let cards = self.stack.iface_list();
        let mut socks = self.socks.lock();
        for c in cards.iter().skip(socks.len()) {
            let mut s = new_socket(self.stack)?;
            let ep = IpListenEndpoint { addr: (!self.ip.is_unspecified()).then(|| iface::v4(self.ip).into()), port: self.port };
            if s.bind(ep).is_err() {
                continue;
            }
            let h = c.inner.lock().sockets.add(s);
            socks.push((c.clone(), h));
        }
        Ok(())
    }

    fn send_on(&self, card: &Arc<Iface>, h: SocketHandle, buf: &[u8], to: SockAddr) -> Result<(), NetError> {
        let deadline = self.stack.now().saturating_add(SEND_WAIT_NS);
        loop {
            let r = {
                let mut g = card.inner.lock();
                let s = g.sockets.get_mut::<udp::Socket>(h);
                if s.payload_send_capacity() < buf.len() {
                    return Err(NetError::Other);
                }
                if s.can_send() { s.send_slice(buf, (iface::v4(to.ip), to.port)).map(|_| true).map_err(|_| NetError::Other) } else { Ok(false) }
            };
            match r {
                Ok(true) => {
                    self.stack.kick();
                    return Ok(());
                }
                Ok(false) => {}
                Err(_) => return Err(NetError::WouldBlock),
            }
            let now = self.stack.now();
            if now >= deadline {
                return Err(NetError::WouldBlock);
            }
            self.stack.kick();
            self.stack.wq.wait_until_timeout(|| card.inner.lock().sockets.get_mut::<udp::Socket>(h).can_send(), deadline - now);
        }
    }

    fn try_recv(&self, buf: &mut [u8]) -> Option<(usize, SockAddr)> {
        let socks = self.socks.lock().clone();
        for (c, h) in socks {
            let mut g = c.inner.lock();
            let s = g.sockets.get_mut::<udp::Socket>(h);
            if let Ok((n, meta)) = s.recv_slice(buf) {
                let IpAddress::Ipv4(a) = meta.endpoint.addr;
                return Some((n, SockAddr { ip: iface::from_v4(a), port: meta.endpoint.port }));
            }
        }
        None
    }

    fn ready(&self) -> bool {
        self.socks.lock().iter().any(|(c, h)| c.inner.lock().sockets.get_mut::<udp::Socket>(*h).can_recv())
    }
}

impl UdpSocket for SmolUdp {
    fn send_to(&self, buf: &[u8], to: SockAddr) -> Result<(), NetError> {
        // A card without its socket (no memory for it yet) answers `NoMemory`, not `Unreachable`.
        let missing = if self.sync().is_err() { NetError::NoMemory } else { NetError::Unreachable };
        if to.ip.is_loopback() || to.ip.is_unspecified() {
            return Err(NetError::Unreachable);
        }
        self.peers.sent(policy::IpAddr::V4(to.ip.0), to.port, buf.len());
        let socks = self.socks.lock().clone();
        if to.ip == Ipv4::BROADCAST {
            let mut sent = false;
            for (c, h) in socks.iter().filter(|(c, _)| c.info.lock().link_up) {
                sent |= self.send_on(c, *h, buf, to).is_ok();
            }
            return if sent { Ok(()) } else { Err(missing) };
        }
        let (card, _) = self.stack.route(to.ip).ok_or(NetError::Unreachable)?;
        let (c, h) = socks.iter().find(|(c, _)| Arc::ptr_eq(c, &card)).ok_or(missing)?;
        self.send_on(c, *h, buf, to)
    }

    fn recv_from(&self, buf: &mut [u8], timeout_ns: Option<u64>) -> Result<(usize, SockAddr), NetError> {
        let deadline = timeout_ns.map(|t| self.stack.now().saturating_add(t));
        loop {
            let _ = self.sync();
            if let Some(r) = self.try_recv(buf) {
                self.peers.received(policy::IpAddr::V4(r.1.ip.0), r.1.port, r.0, None);
                return Ok(r);
            }
            let now = self.stack.now();
            let slice = match deadline {
                Some(d) if now >= d => return Err(NetError::TimedOut),
                Some(d) => (d - now).min(SLICE_NS),
                None => SLICE_NS,
            };
            self.stack.wq.wait_until_timeout(|| self.ready(), slice);
        }
    }
}

impl Drop for SmolUdp {
    fn drop(&mut self) {
        for (c, h) in self.socks.lock().drain(..) {
            c.inner.lock().sockets.remove(h);
        }
        if self.owns_port {
            UDP_PORTS.lock().remove(&self.port);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// ICMP echo

static PING_IDENT: SpinLock<u16> = SpinLock::named("net.ping", 0x4253);

fn echo_request(ident: u16, seq: u16) -> Vec<u8> {
    let mut p = vec![8u8, 0, 0, 0];
    p.extend_from_slice(&ident.to_be_bytes());
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(b"BitsOS V2 echo 0123456789abcdef");
    let c = crate::frames::checksum(&p);
    p[2..4].copy_from_slice(&c.to_be_bytes());
    p
}

pub(crate) fn ping(stack: &'static Stack, dst: Ipv4, timeout_ns: u64) -> Result<u64, NetError> {
    let (card, _) = stack.route(dst).ok_or(NetError::Unreachable)?;
    let ident = {
        let mut g = PING_IDENT.lock();
        *g = g.wrapping_add(1);
        *g
    };
    let seq = 1u16;
    let t0 = stack.now();
    let h = {
        let buf = || -> Result<icmp::PacketBuffer<'static>, NetError> {
            Ok(icmp::PacketBuffer::new(stack.buffer(4, icmp::PacketMetadata::EMPTY)?, stack.buffer(1024, 0u8)?))
        };
        let mut s = icmp::Socket::new(buf()?, buf()?);
        s.bind(icmp::Endpoint::Ident(ident)).map_err(|_| NetError::Other)?;
        s.send_slice(&echo_request(ident, seq), IpAddress::Ipv4(iface::v4(dst))).map_err(|_| NetError::Other)?;
        card.inner.lock().sockets.add(s)
    };
    stack.kick();
    let mut buf = [0u8; 256];
    let answer = |buf: &mut [u8]| -> bool {
        let mut g = card.inner.lock();
        let s = g.sockets.get_mut::<icmp::Socket>(h);
        while let Ok((n, IpAddress::Ipv4(from))) = s.recv_slice(buf) {
            let p = &buf[..n];
            if from == iface::v4(dst) && p.len() >= 8 && p[0] == 0 && p[4..6] == ident.to_be_bytes() && p[6..8] == seq.to_be_bytes() {
                return true;
            }
        }
        false
    };
    let deadline = t0.saturating_add(timeout_ns);
    let mut got = false;
    loop {
        if answer(&mut buf) {
            got = true;
            break;
        }
        let now = stack.now();
        if now >= deadline {
            break;
        }
        stack.wq.wait_until_timeout(|| card.inner.lock().sockets.get_mut::<icmp::Socket>(h).can_recv(), deadline - now);
    }
    let rtt = stack.now().saturating_sub(t0);
    card.inner.lock().sockets.remove(h);
    if got { Ok(rtt) } else { Err(NetError::TimedOut) }
}
