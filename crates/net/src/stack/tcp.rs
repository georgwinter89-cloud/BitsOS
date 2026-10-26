//! TCP streams over smoltcp sockets (`C: KSocket`'s `TcpStream`). A stream owns one socket on one card; the
//! card's lock is taken for each call and never held while waiting. Dropping a stream closes it gracefully in
//! the background (FIN after the queued data; RST after 30 s, `iface::ORPHAN_NS`), `shutdown` resets it (RST)
//! and wakes a reader blocked in another thread. Every connection is recorded for the connection view (B3).

use super::iface::{self, Iface};
use super::wake::{SockWake, WAKE_SLICE_NS};
use super::Stack;
use crate::{NetError, SockAddr, TcpStream};
use alloc::sync::Arc;
use core::task::Waker;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{self as stcp, State};
use smoltcp::time::Duration;
use smoltcp::wire::IpEndpoint;

/// Buffer sizes (bytes). Listening sockets become MCP's connections: answers (screenshots) are larger than
/// requests. Outgoing connections are mostly downloads. Window scaling (RFC 7323) follows from the size.
pub const LISTEN_RX: usize = 64 * 1024;
pub const LISTEN_TX: usize = 256 * 1024;
pub const CONNECT_RX: usize = 256 * 1024;
pub const CONNECT_TX: usize = 128 * 1024;
/// `http::get` on the kernel's stack: 16 MB in < 0.5 s over the LAN needs a wide window.
pub const FETCH_RX: usize = 1024 * 1024;
/// No progress for this long ends a connection with RST (V1 `tcpstall.asm`: 30 s).
const STALL_S: u64 = 30;

/// V1 `tcpstall.asm`, spec §2.4: 30 s **without progress** end a connection, i.e. a handshake or data the peer
/// does not acknowledge. smoltcp's timeout counts from the peer's last packet whenever it is set, so an idle
/// connection (an MCP client between requests) would be reset after 30 s of quiet; the timeout is therefore on
/// only while the connection waits for the peer. Closing states are left to the orphan deadline
/// (`iface::ORPHAN_NS`): a stale timestamp there would turn the FIN into an RST at once.
pub(crate) fn stall_rule(s: &mut stcp::Socket<'static>) {
    let waiting = match s.state() {
        State::SynSent | State::SynReceived => true,
        State::Established | State::CloseWait => s.send_queue() > 0,
        _ => false,
    };
    let want = waiting.then(|| Duration::from_secs(STALL_S));
    if s.timeout() != want {
        s.set_timeout(want);
    }
}

/// A TCP socket with `rx`/`tx` bytes of buffer, or `NoMemory` when the heap cannot give them (R2).
pub fn new_socket(stack: &Stack, rx: usize, tx: usize) -> Result<stcp::Socket<'static>, NetError> {
    let (rb, tb) = (stack.buffer(rx, 0u8)?, stack.buffer(tx, 0u8)?);
    let mut s = stcp::Socket::new(stcp::SocketBuffer::new(rb), stcp::SocketBuffer::new(tb));
    tune(&mut s);
    Ok(s)
}

/// TCP tuning (NW-4), applied to every new socket and again to a kept socket before it listens once more (smoltcp's
/// `listen` resets the connection state, not these settings):
/// - 30 s without progress end a connection (`stall_rule` narrows it to "waiting for the peer");
/// - Nagle off: request/answer traffic (MCP) must not wait for Nagle + delayed ACK; bulk senders write large blocks;
/// - Reno (Cubic needs f64, no SSE in the kernel, R10);
/// - RFC 7323 timestamps (spec §3: window scaling + timestamps): an RTT sample from every ACK instead of one per
///   window, and PAWS on long fast transfers; smoltcp drops them per connection when the peer's SYN has none
///   (Windows by default). Window scaling follows from the buffer sizes.
pub(crate) fn tune(s: &mut stcp::Socket<'static>) {
    s.set_timeout(Some(Duration::from_secs(STALL_S)));
    s.set_nagle_enabled(false);
    s.set_congestion_control(stcp::CongestionControl::Reno);
    s.set_tsval_generator(Some(tsval));
}

/// Listening sockets (services: MCP) also ACK at once instead of after smoltcp's 10 ms: a client that still uses
/// Nagle sends the tail of a request only after our ACK of the segment before (node/Windows), and an answer must
/// not wait for that. Costs one bare ACK per request segment; outgoing connections (downloads) keep the delay,
/// which halves the ACKs of a bulk transfer.
pub(crate) fn tune_listen(s: &mut stcp::Socket<'static>) {
    tune(s);
    s.set_ack_delay(None);
}

/// TSval in milliseconds of the kernel clock (RFC 7323 §5.4: 1 ms to 1 s per tick). The clock restarts with every
/// kernel; a peer's PAWS state is per connection, and a new kernel's connections are new.
fn tsval() -> u32 {
    (time::now_ns() / 1_000_000) as u32
}

pub struct SmolStream {
    /// NW-7: this connection's own wake-up (smoltcp's waker), so a wait sleeps until its connection changed.
    wake: Arc<SockWake>,
    waker: Waker,
    stack: &'static Stack,
    iface: Arc<Iface>,
    handle: SocketHandle,
    local: SockAddr,
    peer: SockAddr,
    /// The peer's FIN was seen (a later Closed is the end, not a reset).
    fin_seen: AtomicBool,
    closed: AtomicBool,
    reset: AtomicBool,
    /// The connection view's entry (PM-2), from the record as opened or accepted on; its close ends it.
    conn: ksync::Once<policy::ConnHandle>,
    /// The socket belongs to the kept pool (a reserved listener's, the MCP reserve): it goes back to the pool
    /// once the connection has closed instead of being freed.
    kept: bool,
    /// Bytes each way, shared with the view's entry (counted from the handshake on, PM-2).
    bytes: Arc<policy::ConnCounters>,
    /// Who owns it (B3 records): the kernel, or the program whose socket it is (NW-5).
    subject: ksync::SpinLock<policy::Subject>,
    /// The handshake completed once (a later Closed without the peer's FIN is a reset, not a refusal).
    established: AtomicBool,
    /// When the SYN left (a connect that ends after the stall time failed by timeout).
    started_ns: AtomicU64,
}

/// B3: the connection view's entry opens once (Opened or Accepted, with its event record) and ends with the
/// Closed record; a Closed without an opened one records nothing.
fn record(kind: policy::ConnKind, s: &SmolStream) {
    if kind == policy::ConnKind::Closed {
        if let Some(h) = s.conn.get() {
            h.close();
        }
        return;
    }
    s.conn.get_or_init(|| {
        let who = *s.subject.lock();
        let accepted = kind == policy::ConnKind::Accepted;
        policy::open_counted(who, policy::NetProto::Tcp, accepted, s.local.port, policy::IpAddr::V4(s.peer.ip.0), s.peer.port, s.bytes.clone())
    });
}

fn addr(e: IpEndpoint) -> SockAddr {
    let smoltcp::wire::IpAddress::Ipv4(a) = e.addr;
    SockAddr { ip: iface::from_v4(a), port: e.port }
}

impl SmolStream {
    fn new(stack: &'static Stack, iface: Arc<Iface>, handle: SocketHandle, local: SockAddr, peer: SockAddr, kept: bool) -> Self {
        let wake = SockWake::new(stack.wakes.clone());
        SmolStream {
            waker: Waker::from(wake.clone()),
            wake,
            kept,
            stack,
            iface,
            handle,
            local,
            peer,
            fin_seen: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            conn: ksync::Once::new(),
            bytes: policy::ConnCounters::new(),
            subject: ksync::SpinLock::named("net.tcp.owner", policy::Subject::KERNEL),
            established: AtomicBool::new(false),
            started_ns: AtomicU64::new(stack.now()),
        }
    }

    /// A connection a listening socket completed (called by the card's poll, under its lock).
    pub(crate) fn accepted(stack: &'static Stack, iface: Arc<Iface>, handle: SocketHandle, local: SockAddr, peer: SockAddr, kept: bool, owner: policy::Subject) -> Self {
        let s = Self::new(stack, iface, handle, local, peer, kept);
        s.own(owner);
        s.established.store(true, Ordering::Release);
        record(policy::ConnKind::Accepted, &s);
        s
    }

    /// Names the owner; a program's connection wakes `inet::READY` from its first change on (NW-7 fixer: not
    /// only after the socket layer's first look).
    fn own(&self, who: policy::Subject) {
        *self.subject.lock() = who;
        if who != policy::Subject::KERNEL {
            self.wake.set_prog(true);
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut stcp::Socket<'static>) -> R) -> R {
        let mut g = self.iface.inner.lock();
        f(g.sockets.get_mut::<stcp::Socket>(self.handle))
    }

    /// Ok(Some(n)) data or EOF (0), Ok(None) nothing yet.
    fn try_read(&self, buf: &mut [u8]) -> Result<Option<usize>, NetError> {
        if self.reset.load(Ordering::Acquire) {
            return Err(NetError::ConnectionReset);
        }
        let r = self.with(|s| {
            if matches!(s.state(), State::CloseWait | State::LastAck | State::Closing | State::TimeWait) {
                self.fin_seen.store(true, Ordering::Release);
            }
            if s.can_recv() {
                return s.recv_slice(buf).map(Some).map_err(|_| NetError::ConnectionReset);
            }
            match s.state() {
                State::Closed if !self.fin_seen.load(Ordering::Acquire) => Err(NetError::ConnectionReset),
                _ if !s.may_recv() => Ok(Some(0)),
                _ => {
                    // Under the same lock as the look: a segment after it wakes this connection.
                    s.register_recv_waker(&self.waker);
                    Ok(None)
                }
            }
        })?;
        if let Some(n) = r
            && n > 0
        {
            self.bytes.add_in(n as u64);
            // The window opened: let the poll thread tell the peer.
            self.stack.kick();
        }
        Ok(r)
    }

    /// Sleeps until this connection changed after `seen` (smoltcp's waker, a reset, a close), in slices (B4).
    /// False once `deadline` has passed.
    fn wait_change(&self, seen: u64, deadline: Option<u64>) -> bool {
        let now = self.stack.now();
        let slice = match deadline {
            Some(d) if now >= d => return false,
            Some(d) => (d - now).min(WAKE_SLICE_NS),
            None => WAKE_SLICE_NS,
        };
        self.wake.wait(seen, slice, || self.reset.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire));
        true
    }
}

/// Waits on the stack's readiness queue until `ready` or the deadline (None: no deadline). False on timeout.
fn wait(stack: &Stack, deadline: Option<u64>, ready: impl FnMut() -> bool) -> bool {
    match deadline {
        Some(d) => {
            let now = stack.now();
            now < d && stack.wq.wait_until_timeout(ready, d - now)
        }
        None => {
            stack.wq.wait_until(ready);
            true
        }
    }
}

impl TcpStream for SmolStream {
    fn read(&self, buf: &mut [u8], timeout_ns: Option<u64>) -> Result<usize, NetError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let deadline = timeout_ns.map(|t| self.stack.now().saturating_add(t));
        loop {
            let seen = self.wake.seq();
            if let Some(n) = self.try_read(buf)? {
                return Ok(n);
            }
            if !self.wait_change(seen, deadline) {
                return match self.try_read(buf)? {
                    Some(n) => Ok(n),
                    None => Err(NetError::TimedOut),
                };
            }
        }
    }

    fn write_all(&self, buf: &[u8], timeout_ns: Option<u64>) -> Result<(), NetError> {
        let deadline = timeout_ns.map(|t| self.stack.now().saturating_add(t));
        let mut off = 0;
        while off < buf.len() {
            if self.reset.load(Ordering::Acquire) {
                return Err(NetError::ConnectionReset);
            }
            if self.closed.load(Ordering::Acquire) {
                return Err(NetError::Closed);
            }
            let seen = self.wake.seq();
            let n = self.with(|s| {
                if !s.may_send() {
                    return Err(NetError::ConnectionReset);
                }
                if s.can_send() {
                    s.send_slice(&buf[off..]).map_err(|_| NetError::ConnectionReset)
                } else {
                    s.register_send_waker(&self.waker);
                    Ok(0)
                }
            })?;
            if n > 0 {
                off += n;
                self.bytes.add_out(n as u64);
                self.stack.kick();
                continue;
            }
            if !self.wait_change(seen, deadline) {
                return Err(NetError::TimedOut);
            }
        }
        Ok(())
    }

    fn peer(&self) -> SockAddr {
        self.peer
    }

    fn local(&self) -> SockAddr {
        self.local
    }

    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.with(|s| s.close());
            self.stack.kick();
        }
    }

    fn shutdown(&self) {
        if !self.reset.swap(true, Ordering::AcqRel) {
            self.with(|s| s.abort());
            self.stack.kick();
            self.wake.fire();
            self.stack.wake_sockets();
        }
    }

    fn readable(&self) -> bool {
        self.with(|s| s.can_recv())
    }
}

impl Drop for SmolStream {
    fn drop(&mut self) {
        let now = self.stack.now();
        {
            let mut g = self.iface.inner.lock();
            if !self.reset.load(Ordering::Acquire) {
                let s = g.sockets.get_mut::<stcp::Socket>(self.handle);
                // Closed with data it never read: a reset, as Linux's tcp_close (RFC 2525 2.17).
                if s.recv_queue() > 0 { s.abort() } else { s.close() }
            }
            Iface::orphan(&mut g, self.handle, now, self.kept);
        }
        // A program connection that came up without the socket layer seeing it (closed right after).
        if self.established.load(Ordering::Acquire) && !self.conn.is_completed() {
            record(policy::ConnKind::Opened, self);
        }
        record(policy::ConnKind::Closed, self);
        self.stack.kick();
    }
}

klog::counter!(CONNECT_FAILED, "net.tcp_connect_failed");

/// An outgoing connection: route, ephemeral port, SYN, wait for the handshake.
pub(crate) fn connect(stack: &'static Stack, to: SockAddr, timeout_ns: Option<u64>, rx: usize, tx: usize, owner: policy::Subject) -> Result<SmolStream, NetError> {
    let start = stack.now();
    let deadline = timeout_ns.map(|t| start.saturating_add(t));
    let mut s = connect_start(stack, to, None, rx, tx, owner, None)?;
    loop {
        let st = s.with(|x| x.state());
        match st {
            State::Established | State::CloseWait => break,
            State::Closed => {
                CONNECT_FAILED.inc();
                // smoltcp gives up after the stall timeout; anything sooner is the peer's reset.
                let e = if stack.now().saturating_sub(start) >= STALL_S * 1_000_000_000 { NetError::TimedOut } else { NetError::ConnectionRefused };
                klog::event!(Debug, "net", "tcp.connect_failed", peer = super::ip_str(to.ip), port = to.port, error = alloc::format!("{e:?}"));
                s.reset.store(true, Ordering::Release);
                return Err(e);
            }
            _ => {}
        }
        let ready = || matches!(s.with(|x| x.state()), State::Established | State::CloseWait | State::Closed);
        if !wait(stack, deadline, ready) && !ready() {
            CONNECT_FAILED.inc();
            klog::event!(Debug, "net", "tcp.connect_failed", peer = super::ip_str(to.ip), port = to.port, error = "TimedOut");
            s.shutdown();
            return Err(NetError::TimedOut);
        }
    }
    s.local = s.with(|x| x.local_endpoint()).map(addr).unwrap_or(s.local);
    s.established.store(true, Ordering::Release);
    record(policy::ConnKind::Opened, &s);
    Ok(s)
}

/// The start of a connection without waiting (NW-5's non-blocking connect): route, buffers, the local port
/// (`local_port`, else an ephemeral one that no live connection on the card uses towards this peer), SYN. The
/// stream is in SYN-SENT; `Conn::state` tells when it is up. `owner` is named in its connection records.
/// `dev`: the card a SO_BINDTODEVICE socket is bound to (WL-4 CR 2), None = route by address.
pub(crate) fn connect_start(stack: &'static Stack, to: SockAddr, local_port: Option<u16>, rx: usize, tx: usize, owner: policy::Subject, dev: Option<usize>) -> Result<SmolStream, NetError> {
    let Some((ifc, _pick)) = stack.route_on(to.ip, dev) else {
        klog::event!(Debug, "net", "tcp.unreachable", peer = super::ip_str(to.ip), port = to.port);
        return Err(NetError::Unreachable);
    };
    // The buffers before the card's lock (a large receive window is a large allocation).
    let mut sock = new_socket(stack, rx, tx).inspect_err(|_| {
        CONNECT_FAILED.inc();
        klog::event!(Warn, "net", "tcp.no_memory", peer = super::ip_str(to.ip), port = to.port, rx = rx, tx = tx;
            "no memory for the socket buffers: connect refused before any packet");
    })?;
    let (handle, local) = {
        let mut g = ifc.inner.lock();
        let inner = &mut *g;
        // A port whose 4-tuple a connection on this card still holds (TIME-WAIT, a long-lived one after the
        // counter wrapped) is skipped.
        let remote = iface::v4(to.ip);
        let same_tuple = |p: u16| {
            inner.sockets.iter().any(|(_, x)| match x {
                smoltcp::socket::Socket::Tcp(t) => {
                    t.local_endpoint().is_some_and(|l| l.port == p)
                        && t.remote_endpoint().is_some_and(|r| r.port == to.port && r.addr == remote.into())
                }
                _ => false,
            })
        };
        let port = match local_port {
            Some(p) if same_tuple(p) => return Err(NetError::AddrInUse),
            Some(p) => p,
            None => match stack.ephemeral_port(|p| same_tuple(p) || crate::sock::inet::tcp_port_bound(p)) {
                Some(p) => p,
                None => {
                    drop(g);
                    CONNECT_FAILED.inc();
                    klog::event!(Warn, "net", "tcp.no_port", peer = super::ip_str(to.ip), port = to.port;
                        "every ephemeral port is taken towards this peer");
                    return Err(NetError::AddrInUse);
                }
            },
        };
        if sock.connect(inner.iface.context(), (iface::v4(to.ip), to.port), port).is_err() {
            // No source address on the card (the lease went in between).
            return Err(NetError::Unreachable);
        }
        let local_ip = inner.iface.ipv4_addr().map(iface::from_v4).unwrap_or(crate::Ipv4::ANY);
        (inner.sockets.add(sock), SockAddr { ip: local_ip, port })
    };
    super::TCP_CONNECTS.inc();
    let s = SmolStream::new(stack, ifc, handle, local, to, false);
    s.own(owner);
    stack.kick();
    Ok(s)
}

const KEEPALIVE_S: u64 = 7200;

impl crate::conn::Conn for SmolStream {
    fn try_recv(&self, buf: &mut [u8], peek: bool) -> Result<Option<usize>, NetError> {
        if !peek {
            return self.try_read(buf);
        }
        if self.reset.load(Ordering::Acquire) {
            return Err(NetError::ConnectionReset);
        }
        self.with(|s| {
            if s.can_recv() {
                return s.peek_slice(buf).map(Some).map_err(|_| NetError::ConnectionReset);
            }
            match s.state() {
                State::SynSent | State::SynReceived => Ok(None),
                State::Closed if !self.fin_seen.load(Ordering::Acquire) => Err(NetError::ConnectionReset),
                _ if !s.may_recv() => Ok(Some(0)),
                _ => Ok(None),
            }
        })
    }

    fn discard(&self, n: usize) -> Result<usize, NetError> {
        if self.reset.load(Ordering::Acquire) {
            return Err(NetError::ConnectionReset);
        }
        let k = self.with(|s| {
            if matches!(s.state(), State::CloseWait | State::LastAck | State::Closing | State::TimeWait) {
                self.fin_seen.store(true, Ordering::Release);
            }
            // `recv` hands out one contiguous part of the ring at a time: a range across the wrap takes two.
            let mut done = 0;
            while done < n {
                let k = s
                    .recv(|b| {
                        let k = (n - done).min(b.len());
                        (k, k)
                    })
                    .unwrap_or(0);
                if k == 0 {
                    break;
                }
                done += k;
            }
            done
        });
        if k > 0 {
            self.bytes.add_in(k as u64);
            self.stack.kick();
        }
        Ok(k)
    }

    fn try_send(&self, buf: &[u8]) -> Result<usize, NetError> {
        if self.reset.load(Ordering::Acquire) {
            return Err(NetError::ConnectionReset);
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(NetError::Closed);
        }
        let n = self.with(|s| {
            if matches!(s.state(), State::SynSent | State::SynReceived) {
                return Ok(0);
            }
            if !s.may_send() {
                return Err(NetError::ConnectionReset);
            }
            if s.can_send() { s.send_slice(buf).map_err(|_| NetError::ConnectionReset) } else { Ok(0) }
        })?;
        if n > 0 {
            self.bytes.add_out(n as u64);
            self.stack.kick();
        }
        Ok(n)
    }

    fn state(&self) -> crate::conn::ConnState {
        let reset = self.reset.load(Ordering::Acquire);
        // NW-7: the socket layer's waits and polls look here first (kernel users never call `state`): the
        // wakers make the next change wake `inet::READY`.
        self.wake.set_prog(true);
        let (st, can_recv, may_recv, can_send) = self.with(|s| {
            s.register_recv_waker(&self.waker);
            s.register_send_waker(&self.waker);
            (s.state(), s.can_recv(), s.may_recv(), s.can_send())
        });
        if matches!(st, State::CloseWait | State::LastAck | State::Closing | State::TimeWait) {
            self.fin_seen.store(true, Ordering::Release);
        }
        let connecting = matches!(st, State::SynSent | State::SynReceived);
        if !connecting && st != State::Closed {
            self.established.store(true, Ordering::Release);
        }
        let fin = self.fin_seen.load(Ordering::Acquire);
        let error = if reset {
            Some(NetError::ConnectionReset)
        } else if st == State::Closed && !fin {
            Some(if !self.established.load(Ordering::Acquire) {
                if self.stack.now().saturating_sub(self.started_ns.load(Ordering::Relaxed)) >= STALL_S * 1_000_000_000 {
                    NetError::TimedOut
                } else {
                    NetError::ConnectionRefused
                }
            } else {
                NetError::ConnectionReset
            })
        } else {
            None
        };
        crate::conn::ConnState {
            connecting: connecting && !reset,
            readable: can_recv,
            eof: !connecting && (!may_recv || fin) && !can_recv,
            writable: can_send,
            error,
            closed: self.closed.load(Ordering::Acquire) || matches!(st, State::Closed | State::TimeWait | State::FinWait1 | State::FinWait2 | State::Closing | State::LastAck),
        }
    }

    fn finish(&self) {
        TcpStream::close(self);
    }

    fn recv_queue(&self) -> usize {
        self.with(|s| s.recv_queue())
    }

    fn send_queue(&self) -> usize {
        self.with(|s| s.send_queue())
    }

    fn set_nodelay(&self, on: bool) {
        self.with(|s| s.set_nagle_enabled(!on));
    }

    fn set_keepalive(&self, on: bool) {
        self.with(|s| s.set_keep_alive(on.then(|| Duration::from_secs(KEEPALIVE_S))));
        self.stack.kick();
    }

    fn set_owner(&self, who: policy::Subject) {
        self.own(who);
    }

    fn change_seq(&self) -> Option<u64> {
        Some(self.wake.seq())
    }

    fn on_established(&self) {
        self.established.store(true, Ordering::Release);
        if !self.conn.is_completed() {
            record(policy::ConnKind::Opened, self);
        }
    }
}
