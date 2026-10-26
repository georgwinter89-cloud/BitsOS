//! `C: KSocket` on the stack: listeners (one accept queue fed by the loopback and every card), connect (127.x to
//! the loopback, everything else over the card routing picks), UDP.

use super::{ip_str, tcp, udp, ListenShared, Stack};
use crate::accept::AcceptQueue;
use crate::loopback::Side;
use crate::{KSocket, NetError, SockAddr, TcpListener, TcpStream, UdpSocket};
use alloc::boxed::Box;
use alloc::sync::Arc;

struct Listener {
    shared: Arc<ListenShared>,
    stack: &'static Stack,
}

/// A program's listening socket on the stack (NW-5): connections are taken without waiting; dropping it closes
/// the port like a kernel listener.
pub struct ProgListener(Listener);

impl ProgListener {
    pub fn try_accept(&self) -> Option<Box<dyn crate::conn::Conn>> {
        let c = self.0.shared.queue.try_pop();
        if c.is_some() {
            // Room in the backlog again: the poll thread refills the listening sockets.
            self.0.stack.kick();
        }
        c
    }
    pub fn pending(&self) -> usize {
        self.0.shared.queue.len()
    }
}

impl TcpListener for Listener {
    fn accept(&self, timeout_ns: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        let r = self.shared.queue.accept(timeout_ns);
        if r.is_ok() {
            // Room in the backlog again: the poll thread refills the listening sockets.
            self.stack.kick();
        }
        r
    }
    fn local(&self) -> SockAddr {
        SockAddr { ip: self.shared.ip, port: self.shared.port }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // The queue closes first: a poll round still holding the old listener list refills nothing for a
        // closed queue, so `drop_listener` below removes the last listening sockets and the port is free at
        // once; a connection pushed meanwhile is reset by the closed queue.
        self.shared.queue.close();
        self.stack.listeners.lock().retain(|l| !Arc::ptr_eq(l, &self.shared));
        self.stack.listeners_gen.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        // The reserve shrinks first, so the listening sockets given back below are freed (a listener started
        // again makes new ones while memory is there).
        if self.shared.reserve && !self.shared.ip.is_loopback() {
            self.stack.kept_reserve(super::iface::listen_want(&self.shared), false);
        }
        let now = self.stack.now();
        for i in self.stack.iface_list() {
            i.drop_listener(self.stack, &self.shared, now);
        }
        self.stack.kick();
        klog::event!(Debug, "net", "tcp.unlisten", port = self.shared.port);
    }
}

impl KSocket for Stack {
    /// Every listener the kernel's stack makes is private-only (NW-4, spec §2.4, B8: "every BitsOS service listens
    /// on private addresses only", V1 for MCP and listening programs alike): a SYN from outside RFC 1918 gets RST
    /// before any state. Only `tcp_listen_public` (tests) makes another kind.
    fn tcp_listen(&self, addr: SockAddr, backlog: usize) -> Result<Box<dyn TcpListener>, NetError> {
        self.listen_with(addr, backlog, true, false)
    }

    /// The lifeline's listener (MCP): private-only as every listener, plus the reserve (NW-4, V2-PLAN §6): its
    /// listening sockets (at least `iface::RESERVE_MIN` per card, made now while memory is there) come from the
    /// stack's kept pool and return to it after each connection; the pool also holds `RESERVE_MIN` idle spares per
    /// card (made now, topped up every round while memory is there) that refill the listening sockets an accept
    /// used. So a program that empties the heap cannot take MCP away. SYNs from outside the private networks are aborted in SYN-RECEIVED inside the round that
    /// received them, so the peer gets RST instead of SYN-ACK and nothing is allocated (`iface::refuse_public`).
    fn tcp_listen_private(&self, addr: SockAddr, backlog: usize) -> Result<Box<dyn TcpListener>, NetError> {
        self.listen_with(addr, backlog, true, true)
    }

    fn tcp_connect(&self, to: SockAddr, timeout_ns: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        self.this()?.tcp_connect_sized(to, timeout_ns, tcp::CONNECT_RX, tcp::CONNECT_TX)
    }

    fn udp_bind(&self, addr: SockAddr) -> Result<Box<dyn UdpSocket>, NetError> {
        udp::SmolUdp::bind(self.this()?, addr).map(|u| Box::new(u) as Box<dyn UdpSocket>)
    }
}

impl Stack {
    /// Whether a listener of this stack (the kernel's or a program's) holds `port` on `ip` or on 0.0.0.0.
    pub(crate) fn listening_on(&self, ip: crate::Ipv4, port: u16) -> bool {
        let cards = self.listeners.lock().iter().any(|x| x.port == port && !x.queue.is_closed() && (x.ip == ip || x.ip.is_unspecified() || ip.is_unspecified()));
        cards || ((ip.is_loopback() || ip.is_unspecified()) && self.lo.is_listening(port))
    }

    /// A listener that also takes peers outside the private networks. Not reachable through `KSocket` (B8: no
    /// BitsOS service listens publicly); for tests that need the difference.
    pub fn tcp_listen_public(&self, addr: SockAddr, backlog: usize) -> Result<Box<dyn TcpListener>, NetError> {
        self.listen_with(addr, backlog, false, false)
    }

    fn listen_with(&self, addr: SockAddr, backlog: usize, private_only: bool, reserve: bool) -> Result<Box<dyn TcpListener>, NetError> {
        self.listen_inner(addr, Side::V4, backlog, private_only, reserve, policy::Subject::KERNEL, None).map(|l| Box::new(l) as Box<dyn TcpListener>)
    }

    /// A program's listener (NW-5): private networks only like every listener of this stack, its owner named in
    /// the records of what it accepts, `rx`/`tx` buffer sizes for its connections.
    pub(crate) fn listen_prog(&self, addr: SockAddr, side: Side, backlog: usize, owner: policy::Subject, rx: usize, tx: usize) -> Result<ProgListener, NetError> {
        self.listen_inner(addr, side, backlog, true, false, owner, Some((rx, tx))).map(|l| ProgListener(l))
    }

    fn listen_inner(&self, addr: SockAddr, side: Side, backlog: usize, private_only: bool, reserve: bool, owner: policy::Subject, prog: Option<(usize, usize)>) -> Result<Listener, NetError> {
        let st = self.this()?;
        if addr.port == 0 {
            return Err(NetError::Other);
        }
        let shared = Arc::new(ListenShared {
            ip: addr.ip,
            port: addr.port,
            backlog: backlog.max(1),
            queue: Arc::new(AcceptQueue::new()),
            private_only,
            reserve,
            owner,
            prog,
        });
        {
            let mut l = st.listeners.lock();
            // The cards carry IPv4 only: an IPv6-only listener (on the loopback) never meets their listeners.
            if side.has_v4() && l.iter().any(|x| x.port == addr.port && !x.queue.is_closed() && (x.ip == addr.ip || x.ip.is_unspecified() || addr.ip.is_unspecified())) {
                return Err(NetError::AddrInUse);
            }
            // 127.x and 0.0.0.0 listeners also hear the loopback.
            if addr.ip.is_unspecified() || addr.ip.is_loopback() {
                st.lo.register_side(addr.port, side, &shared.queue)?;
            }
            if side.has_v4() && !addr.ip.is_loopback() {
                l.push(shared.clone());
                st.listeners_gen.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            }
        }
        if reserve && !addr.ip.is_loopback() {
            st.kept_reserve(super::iface::listen_want(&shared), true);
        }
        let listener = Listener { shared: shared.clone(), stack: st };
        if !addr.ip.is_loopback() {
            // R2: a card that got no listening socket at all would refuse every SYN; the caller hears it now.
            // (No card yet is fine: the poll thread fills new cards.) Dropping `listener` undoes everything.
            let cards = st.iface_list();
            let none = cards.iter().map(|i| i.add_listener(st, &shared)).filter(|r| r.short && r.have == 0).count();
            if none > 0 {
                klog::event!(Warn, "net", "tcp.listen_no_memory", ip = ip_str(addr.ip), port = addr.port, cards = none;
                    "no memory for a listening socket");
                drop(listener);
                return Err(NetError::NoMemory);
            }
            // The reserve's idle spares exist before the caller hears "listening" (a heap emptied right after
            // still lets the first accepts be refilled); short is logged and retried by every poll round.
            if reserve {
                st.top_up_kept(false);
            }
        }
        st.kick();
        let k = st.kept_info();
        klog::event!(Info, "net", "tcp.listen", ip = ip_str(addr.ip), port = addr.port, backlog = backlog, private_only = private_only,
            reserve = reserve, kept_made = k.made, pid = owner.pid);
        Ok(listener)
    }

    /// A TCP connection with chosen buffer sizes (the HTTP client takes a large receive buffer).
    pub fn tcp_connect_sized(&'static self, to: SockAddr, timeout_ns: Option<u64>, rx: usize, tx: usize) -> Result<Box<dyn TcpStream>, NetError> {
        self.tcp_connect_owned(to, timeout_ns, rx, tx, policy::Subject::KERNEL)
    }

    /// `tcp_connect_sized` for a kernel connection made on someone's behalf (MCP fetch, PM-2 CR3): the view's
    /// entry names `owner` from the handshake on, so it is not counted as the kernel's.
    pub fn tcp_connect_owned(&'static self, to: SockAddr, timeout_ns: Option<u64>, rx: usize, tx: usize, owner: policy::Subject) -> Result<Box<dyn TcpStream>, NetError> {
        if to.ip.is_loopback() {
            return self.lo.connect(to);
        }
        tcp::connect(self, to, timeout_ns, rx, tx, owner).map(|s| Box::new(s) as Box<dyn TcpStream>)
    }
}
