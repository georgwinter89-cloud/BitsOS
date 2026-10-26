//! TCP between kernel users on 127.0.0.1: the whole `KSocket` in G0, and the 127.0.0.1 part of the smoltcp
//! stack since NW-3 (a `0.0.0.0` listener hears both). Byte queues with wait queues; dynamic everything.
//!
//! Life cycle (MC-1's change request 2, Linux semantics): dropping a listener gives its port back and resets
//! the connections nobody accepted; dropping a stream sends EOF to the peer and makes the peer's writes fail;
//! `shutdown` resets both directions and wakes a reader blocked on either side (V1 `srv_kill`).

use crate::accept::AcceptQueue;
use crate::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{SpinLock, WaitQueue};

/// Bytes one direction holds before a writer waits for the reader (the smoltcp path's `LISTEN_TX`): a stuck
/// or slow reader on 127.0.0.1 holds the writer back instead of growing the kernel heap.
pub const PIPE_LIMIT: usize = 256 * 1024;

/// One direction of a connection.
struct Pipe {
    data: SpinLock<VecDeque<u8>>,
    /// The writer closed (FIN): the reader gets EOF after the data.
    eof: AtomicBool,
    /// The reader is gone (its stream was dropped): writes fail.
    reader_gone: AtomicBool,
    /// Reset: both ends fail at once, queued data is dropped.
    reset: AtomicBool,
    wq: WaitQueue,
}

impl Pipe {
    fn new() -> Arc<Self> {
        Arc::new(Pipe {
            data: SpinLock::named("loopback.pipe", VecDeque::new()),
            eof: AtomicBool::new(false),
            reader_gone: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            wq: WaitQueue::new(),
        })
    }
    fn set(&self, flag: &AtomicBool) {
        flag.store(true, Ordering::Release);
        self.wq.wake_all();
        crate::sock::inet::notify();
    }
    fn is(&self, flag: &AtomicBool) -> bool {
        let _ = self;
        flag.load(Ordering::Acquire)
    }
}

struct Stream {
    rx: Arc<Pipe>,
    tx: Arc<Pipe>,
    local: SockAddr,
    peer: SockAddr,
}

impl TcpStream for Stream {
    fn read(&self, buf: &mut [u8], timeout_ns: Option<u64>) -> Result<usize, NetError> {
        let rx = &self.rx;
        let ready = || !rx.data.lock().is_empty() || rx.is(&rx.eof) || rx.is(&rx.reset);
        let ok = match timeout_ns {
            Some(t) => rx.wq.wait_until_timeout(ready, t),
            None => {
                rx.wq.wait_until(ready);
                true
            }
        };
        if rx.is(&rx.reset) {
            return Err(NetError::ConnectionReset);
        }
        let mut d = rx.data.lock();
        if d.is_empty() {
            drop(d);
            return if rx.is(&rx.eof) {
                Ok(0)
            } else if ok {
                Err(NetError::WouldBlock)
            } else {
                Err(NetError::TimedOut)
            };
        }
        let was_full = d.len() >= PIPE_LIMIT;
        let n = buf.len().min(d.len());
        for (b, v) in buf.iter_mut().zip(d.drain(..n)) {
            *b = v;
        }
        drop(d);
        // Room again: a writer may wait on the same queue.
        if was_full && rx.wq.waiter_count() > 0 {
            rx.wq.wake_all();
        }
        crate::sock::inet::notify();
        Ok(n)
    }
    /// Queues up to `PIPE_LIMIT` bytes and waits for the reader beyond that; `TimedOut` when no room came
    /// within `timeout_ns` (the bytes queued before stay queued, as on TCP).
    fn write_all(&self, buf: &[u8], timeout_ns: Option<u64>) -> Result<(), NetError> {
        let tx = &self.tx;
        let deadline = timeout_ns.and_then(|t| ksync::now_ns().map(|n| n.saturating_add(t)));
        let mut off = 0;
        loop {
            if tx.is(&tx.reset) || tx.is(&tx.reader_gone) {
                return Err(NetError::ConnectionReset);
            }
            if tx.is(&tx.eof) {
                return Err(NetError::Closed);
            }
            let n = {
                let mut d = tx.data.lock();
                let n = PIPE_LIMIT.saturating_sub(d.len()).min(buf.len() - off);
                d.extend(buf[off..off + n].iter().copied());
                n
            };
            off += n;
            if n > 0 {
                tx.wq.wake_all();
                crate::sock::inet::notify();
            }
            if off == buf.len() {
                return Ok(());
            }
            let ready = || tx.data.lock().len() < PIPE_LIMIT || tx.is(&tx.reset) || tx.is(&tx.reader_gone) || tx.is(&tx.eof);
            let ok = match (timeout_ns, deadline, ksync::now_ns()) {
                (None, ..) => {
                    tx.wq.wait_until(ready);
                    true
                }
                (Some(_), Some(d), Some(now)) => now < d && tx.wq.wait_until_timeout(ready, d - now),
                // No clock (host): the timeout counts from this wait.
                (Some(t), ..) => tx.wq.wait_until_timeout(ready, t),
            };
            if !ok && !ready() {
                return Err(NetError::TimedOut);
            }
        }
    }
    fn peer(&self) -> SockAddr {
        self.peer
    }
    fn local(&self) -> SockAddr {
        self.local
    }
    fn close(&self) {
        self.tx.set(&self.tx.eof);
    }
    fn shutdown(&self) {
        self.tx.data.lock().clear();
        self.rx.data.lock().clear();
        self.tx.set(&self.tx.reset);
        self.rx.set(&self.rx.reset);
    }
    fn readable(&self) -> bool {
        !self.rx.data.lock().is_empty()
    }
}

impl crate::conn::Conn for Stream {
    fn try_recv(&self, buf: &mut [u8], peek: bool) -> Result<Option<usize>, NetError> {
        let rx = &self.rx;
        if rx.is(&rx.reset) {
            return Err(NetError::ConnectionReset);
        }
        let mut d = rx.data.lock();
        if d.is_empty() {
            return Ok(rx.is(&rx.eof).then_some(0));
        }
        let n = buf.len().min(d.len());
        if peek {
            for (b, v) in buf.iter_mut().zip(d.iter()) {
                *b = *v;
            }
            return Ok(Some(n));
        }
        let was_full = d.len() >= PIPE_LIMIT;
        for (b, v) in buf.iter_mut().zip(d.drain(..n)) {
            *b = v;
        }
        drop(d);
        if was_full && rx.wq.waiter_count() > 0 {
            rx.wq.wake_all();
        }
        crate::sock::inet::notify();
        Ok(Some(n))
    }
    fn discard(&self, n: usize) -> Result<usize, NetError> {
        let rx = &self.rx;
        if rx.is(&rx.reset) {
            return Err(NetError::ConnectionReset);
        }
        let mut d = rx.data.lock();
        let k = n.min(d.len());
        let was_full = d.len() >= PIPE_LIMIT;
        d.drain(..k);
        drop(d);
        if was_full && rx.wq.waiter_count() > 0 {
            rx.wq.wake_all();
        }
        crate::sock::inet::notify();
        Ok(k)
    }
    fn try_send(&self, buf: &[u8]) -> Result<usize, NetError> {
        let tx = &self.tx;
        if tx.is(&tx.reset) || tx.is(&tx.reader_gone) {
            return Err(NetError::ConnectionReset);
        }
        if tx.is(&tx.eof) {
            return Err(NetError::Closed);
        }
        let n = {
            let mut d = tx.data.lock();
            let n = PIPE_LIMIT.saturating_sub(d.len()).min(buf.len());
            d.extend(buf[..n].iter().copied());
            n
        };
        if n > 0 {
            tx.wq.wake_all();
            crate::sock::inet::notify();
        }
        Ok(n)
    }
    fn state(&self) -> crate::conn::ConnState {
        let (rx, tx) = (&self.rx, &self.tx);
        let reset = rx.is(&rx.reset) || tx.is(&tx.reset);
        // One pipe lock at a time (a struct literal would keep both guards to its end).
        let readable = !rx.data.lock().is_empty();
        let room = tx.data.lock().len() < PIPE_LIMIT;
        crate::conn::ConnState {
            connecting: false,
            readable,
            eof: rx.is(&rx.eof),
            writable: room || reset || tx.is(&tx.reader_gone),
            error: reset.then_some(NetError::ConnectionReset),
            closed: tx.is(&tx.eof) || reset,
        }
    }
    fn finish(&self) {
        self.tx.set(&self.tx.eof);
    }
    fn recv_queue(&self) -> usize {
        self.rx.data.lock().len()
    }
    fn send_queue(&self) -> usize {
        self.tx.data.lock().len()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Closed with data it never read: a reset, as Linux's tcp_close (RFC 2525 2.17).
        if self.rx.is(&self.rx.reset) || self.tx.is(&self.tx.reset) {
            return;
        }
        if !self.rx.data.lock().is_empty() {
            self.shutdown();
            return;
        }
        self.tx.set(&self.tx.eof);
        self.rx.data.lock().clear();
        self.rx.set(&self.rx.reader_gone);
    }
}

struct Listener {
    queue: Arc<AcceptQueue>,
    port: u16,
}

impl TcpListener for Listener {
    fn accept(&self, timeout_ns: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        self.queue.accept(timeout_ns)
    }
    fn local(&self) -> SockAddr {
        SockAddr { ip: Ipv4::LOCALHOST, port: self.port }
    }
}

impl Drop for Listener {
    /// Closing the queue frees the port (a closed queue's table entry counts as free) and resets the waiting.
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// Which side of the loopback a listener hears: IPv4 peers, IPv6 ones (AF_INET6 sockets to ::1), or both (a
/// dual-stack `::`). Kernel listeners are IPv4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    V4,
    V6,
    Both,
}

impl Side {
    fn slots(self) -> &'static [bool] {
        match self {
            Side::V4 => &[false],
            Side::V6 => &[true],
            Side::Both => &[false, true],
        }
    }
    pub fn has_v4(self) -> bool {
        self != Side::V6
    }
}

/// The 127.0.0.1 port table, one slot per port and side (an IPv6-only listener and an AF_INET one share a port,
/// as on Linux). Entries point at the listener's accept queue; a closed or dropped queue is a free port, so a
/// listener needs no way back into the table to give its port back.
pub struct Loopback {
    listeners: SpinLock<BTreeMap<(u16, bool), Weak<AcceptQueue>>>,
    next_port: SpinLock<u16>,
}

const EPHEMERAL_FIRST: u16 = 40000;

fn live(w: &Weak<AcceptQueue>) -> Option<Arc<AcceptQueue>> {
    w.upgrade().filter(|q| !q.is_closed())
}

impl Loopback {
    pub const fn new() -> Self {
        Loopback { listeners: SpinLock::named("loopback.listeners", BTreeMap::new()), next_port: SpinLock::new(EPHEMERAL_FIRST) }
    }

    /// Puts `queue` on IPv4 `port` (the smoltcp stack's `0.0.0.0` listeners share their queue with the loopback).
    /// The port is free again once the queue is closed or dropped.
    pub fn register(&self, port: u16, queue: &Arc<AcceptQueue>) -> Result<(), NetError> {
        self.register_side(port, Side::V4, queue)
    }

    /// `register` on the slots of `side`: all of them or none.
    pub fn register_side(&self, port: u16, side: Side, queue: &Arc<AcceptQueue>) -> Result<(), NetError> {
        let mut l = self.listeners.lock();
        if side.slots().iter().any(|v6| l.get(&(port, *v6)).and_then(live).is_some()) {
            return Err(NetError::AddrInUse);
        }
        l.retain(|_, w| live(w).is_some());
        for v6 in side.slots() {
            l.insert((port, *v6), Arc::downgrade(queue));
        }
        Ok(())
    }

    /// Whether a listener holds IPv4 `port` here.
    pub fn is_listening(&self, port: u16) -> bool {
        self.listeners.lock().get(&(port, false)).and_then(live).is_some()
    }

    /// Connects to a listener on 127.0.0.0/8.
    pub fn connect(&self, to: SockAddr) -> Result<Box<dyn TcpStream>, NetError> {
        self.connect_conn(to).map(|c| c as Box<dyn TcpStream>)
    }

    /// `connect` for a program socket (it needs the `Conn` side of the stream).
    pub fn connect_conn(&self, to: SockAddr) -> Result<Box<dyn crate::conn::Conn>, NetError> {
        self.connect_side(to, Ipv4::LOCALHOST, false)
    }

    /// `connect_conn` from source address `me` (127.0.0.0/8) to the IPv4 or the IPv6 side (`v6`: an AF_INET6
    /// socket to ::1, whose stand-in both ends then carry).
    pub fn connect_side(&self, to: SockAddr, me: Ipv4, v6: bool) -> Result<Box<dyn crate::conn::Conn>, NetError> {
        if !to.ip.is_loopback() || !me.is_loopback() {
            return Err(NetError::Unreachable);
        }
        let queue = self.listeners.lock().get(&(to.port, v6)).and_then(live).ok_or(NetError::ConnectionRefused)?;
        let port = {
            let mut p = self.next_port.lock();
            *p = p.wrapping_add(1).max(EPHEMERAL_FIRST);
            *p
        };
        let (a, b) = (Pipe::new(), Pipe::new());
        let me = SockAddr { ip: me, port };
        queue.push(Box::new(Stream { rx: a.clone(), tx: b.clone(), local: to, peer: me }))?;
        Ok(Box::new(Stream { rx: b, tx: a, local: me, peer: to }))
    }

    /// A listener on the loopback only.
    pub fn listen(&self, port: u16) -> Result<Box<dyn TcpListener>, NetError> {
        let queue = Arc::new(AcceptQueue::new());
        self.register(port, &queue)?;
        Ok(Box::new(Listener { queue, port }))
    }
}

impl Default for Loopback {
    fn default() -> Self {
        Self::new()
    }
}

/// G0's whole stack (and `net::init`'s fallback when the smoltcp stack cannot start).
impl KSocket for Loopback {
    fn tcp_listen(&self, addr: SockAddr, _backlog: usize) -> Result<Box<dyn TcpListener>, NetError> {
        self.listen(addr.port)
    }
    fn tcp_connect(&self, to: SockAddr, _timeout_ns: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        self.connect(to)
    }
    fn udp_bind(&self, _addr: SockAddr) -> Result<Box<dyn UdpSocket>, NetError> {
        Err(NetError::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const T: Option<u64> = Some(1);

    fn lo() -> &'static Loopback {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Loopback::new()))
    }

    #[test]
    fn loopback_round_trip() {
        let lo = lo();
        let l = lo.tcp_listen(SockAddr { ip: Ipv4::ANY, port: 8765 }, 3).unwrap();
        let c = lo.tcp_connect(SockAddr { ip: Ipv4::LOCALHOST, port: 8765 }, None).unwrap();
        let s = l.accept(T).unwrap();
        c.write_all(b"ping", None).unwrap();
        let mut b = [0u8; 8];
        assert_eq!(s.read(&mut b, T).unwrap(), 4);
        s.write_all(b"pong", None).unwrap();
        s.close();
        assert_eq!(c.read(&mut b, T).unwrap(), 4);
        assert_eq!(c.read(&mut b, T).unwrap(), 0, "EOF after close");
        assert!(lo.tcp_listen(SockAddr { ip: Ipv4::ANY, port: 8765 }, 1).is_err());
        assert!(Ipv4([192, 168, 100, 20]).is_private() && !Ipv4([8, 8, 8, 8]).is_private());
    }

    /// NW-3b: the private-only listener's address rule is RFC 1918 alone (V1 `tcp_private`); `is_private`
    /// keeps loopback and link-local for its other users.
    #[test]
    fn rfc1918_is_stricter_than_private() {
        for ip in [[10, 0, 0, 1], [172, 16, 0, 1], [172, 31, 255, 254], [192, 168, 100, 20]] {
            assert!(Ipv4(ip).is_rfc1918() && Ipv4(ip).is_private(), "{ip:?}");
        }
        for ip in [[169, 254, 1, 1], [127, 0, 0, 1]] {
            assert!(!Ipv4(ip).is_rfc1918() && Ipv4(ip).is_private(), "{ip:?}");
        }
        for ip in [[172, 15, 0, 1], [172, 32, 0, 1], [192, 169, 0, 1], [11, 0, 0, 1], [8, 8, 8, 8]] {
            assert!(!Ipv4(ip).is_rfc1918() && !Ipv4(ip).is_private(), "{ip:?}");
        }
    }

    /// The contract's default: `tcp_listen_private` of a KSocket without its own is `tcp_listen` (the port is
    /// then taken for a plain listen too).
    #[test]
    fn listen_private_defaults_to_listen() {
        let lo = lo();
        let ks: &dyn KSocket = lo;
        let l = ks.tcp_listen_private(SockAddr { ip: Ipv4::ANY, port: 8770 }, 1).unwrap();
        assert_eq!(ks.tcp_listen(SockAddr { ip: Ipv4::ANY, port: 8770 }, 1).err(), Some(NetError::AddrInUse));
        let c = ks.tcp_connect(SockAddr { ip: Ipv4::LOCALHOST, port: 8770 }, T).unwrap();
        assert!(l.accept(T).is_ok());
        drop(c);
    }

    #[test]
    fn dropped_listener_frees_port_and_resets_waiting() {
        let lo = lo();
        let to = SockAddr { ip: Ipv4::LOCALHOST, port: 9000 };
        let l = lo.tcp_listen(to, 3).unwrap();
        let waiting = lo.tcp_connect(to, None).unwrap();
        drop(l);
        let mut b = [0u8; 4];
        assert_eq!(waiting.read(&mut b, T), Err(NetError::ConnectionReset), "not accepted: reset");
        assert_eq!(lo.tcp_connect(to, None).err(), Some(NetError::ConnectionRefused), "nobody listens any more");
        let l2 = lo.tcp_listen(to, 3).expect("port free again");
        let c = lo.tcp_connect(to, None).unwrap();
        let s = l2.accept(T).unwrap();
        c.write_all(b"x", None).unwrap();
        assert_eq!(s.read(&mut b, T).unwrap(), 1);
    }

    #[test]
    fn dropped_stream_is_eof_and_breaks_writes() {
        let lo = lo();
        let to = SockAddr { ip: Ipv4::LOCALHOST, port: 9001 };
        let l = lo.tcp_listen(to, 3).unwrap();
        let c = lo.tcp_connect(to, None).unwrap();
        let s = l.accept(T).unwrap();
        c.write_all(b"last", None).unwrap();
        drop(c);
        let mut b = [0u8; 8];
        assert_eq!(s.read(&mut b, T).unwrap(), 4, "data before the drop still arrives");
        assert_eq!(s.read(&mut b, T).unwrap(), 0, "then EOF");
        assert_eq!(s.write_all(b"x", None), Err(NetError::ConnectionReset));
    }

    #[test]
    fn shutdown_resets_both_sides() {
        let lo = lo();
        let to = SockAddr { ip: Ipv4::LOCALHOST, port: 9002 };
        let l = lo.tcp_listen(to, 3).unwrap();
        let c = lo.tcp_connect(to, None).unwrap();
        let s = l.accept(T).unwrap();
        c.write_all(b"unread", None).unwrap();
        s.shutdown();
        let mut b = [0u8; 8];
        assert_eq!(s.read(&mut b, T), Err(NetError::ConnectionReset), "own reader");
        assert_eq!(c.read(&mut b, T), Err(NetError::ConnectionReset), "peer reader");
        assert_eq!(c.write_all(b"x", None), Err(NetError::ConnectionReset));
    }

    #[test]
    fn shutdown_wakes_a_blocked_reader() {
        let lo = lo();
        let to = SockAddr { ip: Ipv4::LOCALHOST, port: 9003 };
        let l = lo.tcp_listen(to, 3).unwrap();
        let _c = lo.tcp_connect(to, None).unwrap();
        let s: Arc<dyn TcpStream> = Arc::from(l.accept(T).unwrap());
        let r = s.clone();
        let reader = std::thread::spawn(move || {
            let mut b = [0u8; 8];
            r.read(&mut b, None)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        s.shutdown();
        assert_eq!(reader.join().unwrap(), Err(NetError::ConnectionReset));
    }

    /// A reader that does not read holds the writer at `PIPE_LIMIT` (the heap does not grow); reading lets it on.
    #[test]
    fn full_pipe_holds_the_writer() {
        let lo = lo();
        let to = SockAddr { ip: Ipv4::LOCALHOST, port: 9004 };
        let l = lo.tcp_listen(to, 3).unwrap();
        let c: Arc<dyn TcpStream> = Arc::from(lo.tcp_connect(to, None).unwrap());
        let s = l.accept(T).unwrap();
        // (Host waits have no clock, so the `TimedOut` side is a ktest: `nw.loopback.full_pipe`.)
        let big = alloc::vec![7u8; PIPE_LIMIT + 5000];
        let w = c.clone();
        let writer = std::thread::spawn(move || w.write_all(&big, None));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!writer.is_finished(), "waits while the pipe is full");
        let mut got = 0usize;
        let mut b = alloc::vec![0u8; 64 * 1024];
        while got < PIPE_LIMIT + 5000 {
            // No timeout: under host load the writer thread may need longer than one tick to refill the pipe
            // (DP-1/6: failed 3 of 40 runs with `T`); the writer finishing is what this test waits for.
            got += s.read(&mut b, None).unwrap();
        }
        assert_eq!(writer.join().unwrap(), Ok(()));
        assert_eq!(got, PIPE_LIMIT + 5000);
        drop(s);
        assert_eq!(c.write_all(&[1u8; 10], None), Err(NetError::ConnectionReset), "reader gone: no wait");
    }

    #[test]
    fn not_loopback_is_unreachable() {
        let lo = lo();
        assert_eq!(lo.tcp_connect(SockAddr { ip: Ipv4([10, 0, 2, 2]), port: 80 }, None).err(), Some(NetError::Unreachable));
        assert!(lo.udp_bind(SockAddr { ip: Ipv4::ANY, port: 0 }).is_err());
    }
}

/// A loopback `NetDevice` (contract test and the G0 stub): transmitted frames come back on receive.
pub struct LoopDevice {
    name: alloc::string::String,
    q: SpinLock<VecDeque<alloc::vec::Vec<u8>>>,
}

impl LoopDevice {
    pub fn new(name: &str) -> Self {
        LoopDevice { name: alloc::string::String::from(name), q: SpinLock::new(VecDeque::new()) }
    }
}

impl devmodel::net::NetDevice for LoopDevice {
    fn name(&self) -> &str {
        &self.name
    }
    fn mac(&self) -> devmodel::net::MacAddr {
        devmodel::net::MacAddr([2, 0, 0, 0, 0, 1])
    }
    fn link(&self) -> devmodel::net::LinkState {
        devmodel::net::LinkState { up: true, speed_mbps: 10_000, full_duplex: true }
    }
    fn transmit(&self, frame: &[u8]) -> Result<(), devmodel::DevError> {
        self.q.lock().push_back(frame.to_vec());
        Ok(())
    }
    fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        let f = self.q.lock().pop_front()?;
        let n = f.len().min(buf.len());
        buf[..n].copy_from_slice(&f[..n]);
        Some(n)
    }
    fn poll_tx(&self, frame: &[u8]) -> bool {
        // No locks on the emergency path: drop the frame if the queue is busy.
        match self.q.try_lock() {
            Some(mut q) => {
                q.push_back(frame.to_vec());
                true
            }
            None => false,
        }
    }
}
