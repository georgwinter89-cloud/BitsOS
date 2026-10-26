//! Host tests of the stack: two stacks joined by a virtual cable (an in-memory `NetDevice` pair), each polled
//! by a std thread. They check the glue around smoltcp: accept queues and backlog refill, stream read/write/
//! EOF/reset, `shutdown` waking a blocked reader, listener drop, the loopback beside the cards, UDP, ping,
//! routing refusals. (Waits on the host have no deadline, so every test waits only for things that come.)

extern crate std;
use super::*;
use alloc::collections::VecDeque;
use core::sync::atomic::AtomicBool;
use std::sync::mpsc;
use devmodel::net::{LinkState, MacAddr};
use std::thread;

fn clock() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

type Queue = Arc<SpinLock<VecDeque<Vec<u8>>>>;

/// One end of a virtual cable.
struct End {
    name: String,
    mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    up: Arc<AtomicBool>,
    /// The card's TX ring: frames on the wire not yet taken by the far end (a full ring refuses with Timeout).
    ring: usize,
}

impl NetDevice for End {
    fn name(&self) -> &str {
        &self.name
    }
    fn mac(&self) -> MacAddr {
        MacAddr(self.mac)
    }
    fn link(&self) -> LinkState {
        LinkState { up: self.up.load(Ordering::Acquire), speed_mbps: 1000, full_duplex: true }
    }
    fn transmit(&self, frame: &[u8]) -> Result<(), devmodel::DevError> {
        if !self.up.load(Ordering::Acquire) {
            return Err(devmodel::DevError::Io);
        }
        let mut q = self.tx.lock();
        if q.len() >= self.ring {
            return Err(devmodel::DevError::Timeout);
        }
        q.push_back(frame.to_vec());
        Ok(())
    }
    fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        let f = self.rx.lock().pop_front()?;
        let n = f.len().min(buf.len());
        buf[..n].copy_from_slice(&f[..n]);
        Some(n)
    }
    fn poll_tx(&self, frame: &[u8]) -> bool {
        self.transmit(frame).is_ok()
    }
}

struct Net {
    a: &'static Stack,
    b: &'static Stack,
    /// None: nobody polls (the test drives `Poller`s).
    poll: Option<PollThread>,
    /// Frames A sent (B receives them from here) and the cable's carrier.
    ab: Queue,
    up: Arc<AtomicBool>,
}

impl Net {
    fn poll(&self) -> &PollThread {
        self.poll.as_ref().expect("this Net has a poll thread")
    }
}

const A: Ipv4 = Ipv4([10, 9, 0, 1]);
const B: Ipv4 = Ipv4([10, 9, 0, 2]);

fn queue() -> Queue {
    Arc::new(SpinLock::new(VecDeque::new()))
}

/// A cable between two new ends: (end on our side, end on the far side, frames we send, carrier).
fn cable(name: &str, mac: u8, far_name: &str, far_mac: u8) -> (End, End, Queue, Arc<AtomicBool>) {
    let (rx, tx) = (queue(), queue());
    let up = Arc::new(AtomicBool::new(true));
    let near = End { name: name.into(), mac: [2, 0, 0, 0, 0, mac], rx: rx.clone(), tx: tx.clone(), up: up.clone(), ring: usize::MAX };
    let far = End { name: far_name.into(), mac: [2, 0, 0, 0, 0, far_mac], rx: tx.clone(), tx: rx, up: up.clone(), ring: usize::MAX };
    (near, far, tx, up)
}

/// A std thread that polls the stacks until it is dropped; the drop stops and joins it, so none of its rounds
/// runs afterwards (a test that drives `Poller`s by hand next would otherwise race it).
struct PollThread {
    stop: Arc<AtomicBool>,
    /// Passes over all stacks that ended.
    passes: Arc<AtomicU64>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PollThread {
    /// Waits (bounded) until two more passes ended: the one running now, which may have begun before the
    /// caller's last change (a `hold()`, a clock skew), and one that began after it. The fences pair with the
    /// poll thread's: either this load sees the pass counted or that pass's next round sees the change.
    fn settle(&self) {
        core::sync::atomic::fence(Ordering::SeqCst);
        let p0 = self.passes.load(Ordering::SeqCst);
        eventually("two more poll passes", || self.passes.load(Ordering::SeqCst) >= p0 + 2);
    }
}

impl Drop for PollThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            // A panic in the poll thread already failed the test that caused it (or shows as a hang there).
            let _ = h.join();
        }
    }
}

fn poll_thread(stacks: Vec<&'static Stack>) -> PollThread {
    let stop = Arc::new(AtomicBool::new(false));
    let passes = Arc::new(AtomicU64::new(0));
    let (s, n) = (stop.clone(), passes.clone());
    let handle = thread::spawn(move || {
        let mut p: Vec<Poller> = stacks.into_iter().map(Poller::new).collect();
        while !s.load(Ordering::Acquire) {
            for x in &mut p {
                x.round();
            }
            n.fetch_add(1, Ordering::SeqCst);
            core::sync::atomic::fence(Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_micros(50));
        }
    });
    PollThread { stop, passes, handle: Some(handle) }
}

fn net() -> Net {
    net_on(clock, true)
}

/// Two stacks on one cable with clock `clk`; `poll` false: nobody polls them (the test drives `Poller`s).
fn net_on(clk: fn() -> u64, poll: bool) -> Net {
    net_addrs(clk, A, B, poll)
}

fn net_addrs(clk: fn() -> u64, ip_a: Ipv4, ip_b: Ipv4, poll: bool) -> Net {
    net_ring(clk, ip_a, ip_b, poll, usize::MAX)
}

/// `ring`: frames A's card holds before its TX ring is full.
fn net_ring(clk: fn() -> u64, ip_a: Ipv4, ip_b: Ipv4, poll: bool, ring: usize) -> Net {
    klog::set_serial(false);
    let (mut ea, eb, ab, up) = cable("wire-a", 0xA, "wire-b", 0xB);
    ea.ring = ring;
    let a = Stack::new_static(clk, 1, false);
    let b = Stack::new_static(clk, 2, false);
    a.add_device(Arc::new(ea), false);
    b.add_device(Arc::new(eb), false);
    a.set_static("wire-a", ip_a, 24, None).unwrap();
    b.set_static("wire-b", ip_b, 24, None).unwrap();
    let poll = poll.then(|| poll_thread(alloc::vec![a, b]));
    Net { a, b, poll, ab, up }
}

/// A clock that tests move forward by hand (each test its own: they run in parallel).
macro_rules! skew_clock {
    ($name:ident, $skew:ident) => {
        static $skew: AtomicU64 = AtomicU64::new(0);
        fn $name() -> u64 {
            clock() + $skew.load(Ordering::Acquire)
        }
    };
}

/// Waits (real time, bounded) until `f` holds.
fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed() < std::time::Duration::from_secs(20), "never happened: {what}");
        thread::sleep(std::time::Duration::from_millis(2));
    }
}

const S: u64 = 1_000_000_000;

fn at(ip: Ipv4, port: u16) -> SockAddr {
    SockAddr { ip, port }
}

const T: Option<u64> = Some(30_000_000_000);

fn read_all(s: &dyn TcpStream, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while got.len() < want {
        let n = s.read(&mut buf, T).unwrap();
        assert!(n > 0, "EOF after {} of {want}", got.len());
        got.extend_from_slice(&buf[..n]);
    }
    got
}

#[test]
fn tcp_both_ways_then_eof() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7000), 2).unwrap();
    let c = n.a.tcp_connect(at(B, 7000), T).unwrap();
    let s = l.accept(T).unwrap();
    assert_eq!(s.peer().ip, A);
    assert_eq!(c.peer(), at(B, 7000));
    assert_eq!(c.local().ip, A);
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
    let d2 = data.clone();
    let writer = thread::spawn(move || c.write_all(&d2, T).map(|_| c));
    let got = read_all(&*s, data.len());
    assert!(got == data, "1 MB arrived changed");
    let c = writer.join().unwrap().unwrap();
    s.write_all(b"answer", T).unwrap();
    assert_eq!(read_all(&*c, 6), b"answer");
    s.close();
    let mut b = [0u8; 8];
    assert_eq!(c.read(&mut b, T), Ok(0), "EOF after the peer's close");
    let recs = policy::recent_connections(4096);
    assert!(recs.iter().any(|r| r.kind == policy::ConnKind::Opened && r.remote_port == 7000), "connect recorded (B3)");
    assert!(recs.iter().any(|r| r.kind == policy::ConnKind::Accepted && r.local_port == 7000), "accept recorded (B3)");
}

#[test]
fn shutdown_wakes_reader_and_resets_peer() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7001), 2).unwrap();
    let c = n.a.tcp_connect(at(B, 7001), T).unwrap();
    let s: Arc<dyn TcpStream> = Arc::from(l.accept(T).unwrap());
    let r = s.clone();
    let reader = thread::spawn(move || {
        let mut b = [0u8; 8];
        r.read(&mut b, None)
    });
    thread::sleep(std::time::Duration::from_millis(100));
    s.shutdown();
    assert_eq!(reader.join().unwrap(), Err(NetError::ConnectionReset), "blocked reader woke");
    let mut b = [0u8; 8];
    assert_eq!(c.read(&mut b, T), Err(NetError::ConnectionReset), "peer saw the RST");
}

#[test]
fn dropped_stream_is_eof_for_the_peer() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7002), 2).unwrap();
    let c = n.a.tcp_connect(at(B, 7002), T).unwrap();
    let s = l.accept(T).unwrap();
    c.write_all(b"bye", T).unwrap();
    drop(c);
    assert_eq!(read_all(&*s, 3), b"bye");
    let mut b = [0u8; 8];
    assert_eq!(s.read(&mut b, T), Ok(0));
}

#[test]
fn listener_drop_refuses_and_frees_the_port() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7003), 2).unwrap();
    assert_eq!(n.b.tcp_listen(at(Ipv4::ANY, 7003), 2).err(), Some(NetError::AddrInUse));
    let waiting = n.a.tcp_connect(at(B, 7003), T).unwrap();
    eventually("the poll thread moved it into the accept queue", || !n.b.listeners.lock()[0].queue.is_empty());
    drop(l);
    let mut b = [0u8; 4];
    assert_eq!(waiting.read(&mut b, T), Err(NetError::ConnectionReset), "never accepted: reset");
    assert_eq!(n.a.tcp_connect(at(B, 7003), T).err(), Some(NetError::ConnectionRefused));
    let l2 = n.b.tcp_listen(at(Ipv4::ANY, 7003), 2).expect("port free again");
    let c = n.a.tcp_connect(at(B, 7003), T).unwrap();
    let s = l2.accept(T).unwrap();
    c.write_all(b"x", T).unwrap();
    assert_eq!(read_all(&*s, 1), b"x");
}

#[test]
fn backlog_holds_connections_until_accepted() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7004), 3).unwrap();
    // Three peers before anyone accepts (V1's "knock": a new connection before the idle one closes).
    let cs: Vec<_> = (0..3).map(|_| n.a.tcp_connect(at(B, 7004), T).unwrap()).collect();
    for (i, c) in cs.iter().enumerate() {
        c.write_all(&[i as u8], T).unwrap();
    }
    let mut seen: Vec<u8> = (0..3).map(|_| read_all(&*l.accept(T).unwrap(), 1)[0]).collect();
    seen.sort();
    assert_eq!(seen, [0, 1, 2]);
    // Refilled after the accepts: a fourth one is served too.
    let c4 = n.a.tcp_connect(at(B, 7004), T).unwrap();
    c4.write_all(b"4", T).unwrap();
    assert_eq!(read_all(&*l.accept(T).unwrap(), 1), b"4");
}

#[test]
fn loopback_and_card_share_one_listener() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7005), 2).unwrap();
    let local = n.b.tcp_connect(at(Ipv4::LOCALHOST, 7005), T).unwrap();
    let remote = n.a.tcp_connect(at(B, 7005), T).unwrap();
    local.write_all(b"L", T).unwrap();
    remote.write_all(b"R", T).unwrap();
    let mut got: Vec<(u8, bool)> = (0..2)
        .map(|_| {
            let s = l.accept(T).unwrap();
            (read_all(&*s, 1)[0], s.peer().ip.is_loopback())
        })
        .collect();
    got.sort();
    assert_eq!(got, [(b'L', true), (b'R', false)]);
    // A 127.0.0.1-only listener is not reachable over the card.
    let _lo = n.b.tcp_listen(at(Ipv4::LOCALHOST, 7006), 1).unwrap();
    assert_eq!(n.a.tcp_connect(at(B, 7006), T).err(), Some(NetError::ConnectionRefused));
    assert!(n.b.tcp_connect(at(Ipv4::LOCALHOST, 7006), T).is_ok());
}

#[test]
fn udp_ping_and_routing() {
    let n = net();
    let ub = n.b.udp_bind(at(Ipv4::ANY, 6000)).unwrap();
    let ua = n.a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    assert_eq!(n.b.udp_bind(at(Ipv4::ANY, 6000)).err(), Some(NetError::AddrInUse));
    ua.send_to(b"datagram", at(B, 6000)).unwrap();
    let mut buf = [0u8; 64];
    let (len, from) = ub.recv_from(&mut buf, T).unwrap();
    assert_eq!(&buf[..len], b"datagram");
    assert_eq!(from.ip, A);
    ub.send_to(b"back", from).unwrap();
    let (len, _) = ua.recv_from(&mut buf, T).unwrap();
    assert_eq!(&buf[..len], b"back");
    assert!(n.a.ping(B, 10_000_000_000).is_ok(), "echo answered by smoltcp");
    // No router on these cards: nothing outside 10.9.0.0/24 is reachable.
    assert_eq!(n.a.tcp_connect(at(Ipv4([8, 8, 8, 8]), 80), T).err(), Some(NetError::Unreachable));
    assert_eq!(ua.send_to(b"x", at(Ipv4([8, 8, 8, 8]), 53)), Err(NetError::Unreachable));
    let ifs = n.a.interfaces();
    assert_eq!(ifs.len(), 1);
    assert_eq!(ifs[0].info.addr, Some((A, 24)));
    assert!(!ifs[0].default_route);
}

/// A loopback card that counts the poll's reads.
struct Counted {
    q: SpinLock<VecDeque<Vec<u8>>>,
    reads: AtomicU64,
}

impl NetDevice for Counted {
    fn name(&self) -> &str {
        "raw-test"
    }
    fn mac(&self) -> MacAddr {
        MacAddr([2, 0, 0, 0, 0, 0x7E])
    }
    fn link(&self) -> LinkState {
        LinkState { up: true, speed_mbps: 1000, full_duplex: true }
    }
    fn transmit(&self, frame: &[u8]) -> Result<(), devmodel::DevError> {
        self.q.lock().push_back(frame.to_vec());
        Ok(())
    }
    fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let f = self.q.lock().pop_front()?;
        let n = f.len().min(buf.len());
        buf[..n].copy_from_slice(&f[..n]);
        Some(n)
    }
    fn poll_tx(&self, frame: &[u8]) -> bool {
        self.transmit(frame).is_ok()
    }
}

/// NW-fl: no sleep decides it. The poll thread reads the card, a hold stops that once the pass in flight has
/// ended, held passes leave the frame alone, and after the hold the poll takes it.
#[test]
fn hold_keeps_the_poll_away() {
    let n = net();
    let dev = Arc::new(Counted { q: SpinLock::new(VecDeque::new()), reads: AtomicU64::new(0) });
    n.a.add_device(dev.clone(), false);
    eventually("the poll reads the new card", || dev.reads.load(Ordering::SeqCst) > 0);
    let h = n.a.hold();
    n.poll().settle();
    let reads = dev.reads.load(Ordering::SeqCst);
    dev.transmit(&[0xAB; 60]).unwrap();
    n.poll().settle();
    assert_eq!(dev.reads.load(Ordering::SeqCst), reads, "held passes ran and did not read the card");
    let mut b = [0u8; 128];
    assert_eq!(dev.receive(&mut b), Some(60), "the frame is still there for the raw test");
    dev.transmit(&[0xAB; 60]).unwrap();
    drop(h);
    eventually("the poll takes the frame after the hold", || dev.q.lock().iter().all(|f| f[0] != 0xAB));
    assert!(dev.reads.load(Ordering::SeqCst) > reads + 1, "read by the poll again");
}

skew_clock!(clock_idle, SKEW_IDLE);

/// An idle connection (nothing unacknowledged) is not a stalled one: 31 s without a packet keep it open.
#[test]
fn idle_connection_survives_31_s() {
    let n = net_on(clock_idle, true);
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7110), 2).unwrap();
    let c = n.a.tcp_connect(at(B, 7110), T).unwrap();
    let s = l.accept(T).unwrap();
    c.write_all(b"x", T).unwrap();
    assert_eq!(read_all(&*s, 1), b"x");
    // Quiet first: an unacknowledged byte would rightly stall out after 30 s. B ingested the byte in the pass in
    // flight at the latest and ACKs at once (`tune_listen`); A takes the ACK within the next two passes.
    n.poll().settle();
    n.poll().settle();
    SKEW_IDLE.store(31 * S, Ordering::Release);
    n.poll().settle();
    s.write_all(b"y", T).unwrap();
    assert_eq!(read_all(&*c, 1), b"y", "the idle connection still works");
}

/// Runs `f` in a thread and fails the test if it does not end within `secs` (host waits have no deadline, so a
/// dead poll thread or a missed wake-up would otherwise hang the test run).
fn within<R: Send + 'static>(secs: u64, what: &str, f: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(secs)).unwrap_or_else(|_| panic!("did not end within {secs} s: {what}"))
}

fn ethertype(f: &[u8]) -> u16 {
    if f.len() < 14 { 0 } else { u16::from_be_bytes([f[12], f[13]]) }
}

/// The TCP flags of an IPv4/TCP frame.
fn tcp_flags(f: &[u8]) -> Option<u8> {
    if ethertype(f) != 0x0800 || f.len() < 34 || f[23] != 6 {
        return None;
    }
    let ihl = (f[14] & 0x0F) as usize * 4;
    f.get(14 + ihl + 13).copied()
}

/// The target address of an ARP request.
fn arp_target(f: &[u8]) -> Option<Ipv4> {
    (ethertype(f) == 0x0806 && f.len() >= 42).then(|| Ipv4([f[38], f[39], f[40], f[41]]))
}

skew_clock!(clock_stall, SKEW_STALL);

/// B4 (checker NW-3): smoltcp's stall timeout closes a socket without sending anything when the peer never
/// answers ARP, so no frame moves; the round that ran the timer must still wake the socket's waiter.
#[test]
fn stall_timeout_ends_connect_and_wakes_without_traffic() {
    let n = net_on(clock_stall, false);
    let a = n.a;
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    let h = thread::spawn(move || {
        let r = a.tcp_connect(at(B, 7100), None).err();
        d.store(true, Ordering::Release);
        r
    });
    let mut pa = Poller::new(n.a);
    for _ in 0..20 {
        pa.round();
        thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!done.load(Ordering::Acquire), "nobody answers: connect still waits");
    assert!(n.ab.lock().iter().any(|f| arp_target(f) == Some(B)), "asked for B's MAC");
    SKEW_STALL.store(31 * S, Ordering::Release);
    let (mut woke, mut rounds) = (false, 0);
    let t0 = std::time::Instant::now();
    while !done.load(Ordering::Acquire) {
        let r = pa.round();
        woke |= r.woke;
        rounds += 1;
        assert!(t0.elapsed() < std::time::Duration::from_secs(10), "connect never ended");
        thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(h.join().unwrap(), Some(NetError::TimedOut));
    assert!(woke, "the timer round woke the waiters ({rounds} rounds)");
}

skew_clock!(clock_orphan, SKEW_ORPHAN);

/// Spec §2.4: a closed connection whose data and FIN the peer never takes is reset after 30 s.
#[test]
fn orphan_is_reset_after_30_s() {
    let n = net_on(clock_orphan, true);
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7101), 2).unwrap();
    let c = n.a.tcp_connect(at(B, 7101), T).unwrap();
    let _s = l.accept(T).unwrap();
    let _held = n.b.hold();
    c.write_all(b"never acknowledged", T).unwrap();
    drop(c);
    let card = n.a.ifaces.lock()[0].clone();
    assert_eq!(card.orphan_count(), 1);
    n.poll().settle();
    let rst = |q: &Queue| q.lock().iter().any(|f| tcp_flags(f).is_some_and(|x| x & 0x04 != 0));
    assert!(!rst(&n.ab), "no RST before the deadline");
    SKEW_ORPHAN.store(31 * S, Ordering::Release);
    eventually("RST sent for the orphan", || rst(&n.ab));
    SKEW_ORPHAN.store(33 * S, Ordering::Release);
    eventually("orphan gone", || card.orphan_count() == 0);
}

/// B3: one record per connection and end: Opened (connect), Accepted (listener), Closed on both sides with the
/// bytes each side moved; UDP: one record per peer (Opened by the sender, Accepted by the receiver).
#[test]
fn every_connection_is_recorded_once() {
    use policy::{ConnKind, NetProto};
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7130), 2).unwrap();
    let c = n.a.tcp_connect(at(B, 7130), T).unwrap();
    let s = l.accept(T).unwrap();
    let pa = c.local().port;
    c.write_all(&[1u8; 1000], T).unwrap();
    assert_eq!(read_all(&*s, 1000).len(), 1000);
    s.write_all(&[2u8; 10], T).unwrap();
    assert_eq!(read_all(&*c, 10).len(), 10);
    drop(c);
    drop(s);
    let ub = n.b.udp_bind(at(Ipv4::ANY, 6200)).unwrap();
    let ua = n.a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    let mut buf = [0u8; 16];
    ua.send_to(b"1", at(B, 6200)).unwrap();
    ua.send_to(b"2", at(B, 6200)).unwrap();
    let (_, from) = ub.recv_from(&mut buf, T).unwrap();
    ub.recv_from(&mut buf, T).unwrap();
    ub.send_to(b"r", from).unwrap();
    ua.recv_from(&mut buf, T).unwrap();
    let recs = policy::recent_connections(4096);
    let tcp = |kind: ConnKind, local: u16, remote: u16| -> Vec<(u64, u64)> {
        recs.iter()
            .filter(|r| r.proto == NetProto::Tcp && r.kind == kind && r.local_port == local && r.remote_port == remote)
            .map(|r| (r.bytes_in, r.bytes_out))
            .collect()
    };
    assert_eq!(tcp(ConnKind::Opened, pa, 7130).len(), 1, "connect recorded once");
    assert_eq!(tcp(ConnKind::Accepted, 7130, pa).len(), 1, "accept recorded once");
    assert_eq!(tcp(ConnKind::Closed, pa, 7130), [(10, 1000)], "client close with its bytes");
    assert_eq!(tcp(ConnKind::Closed, 7130, pa), [(1000, 10)], "server close with its bytes");
    let udp = |kind: ConnKind, local: u16, remote: u16| {
        recs.iter().filter(|r| r.proto == NetProto::Udp && r.kind == kind && r.local_port == local && r.remote_port == remote).count()
    };
    assert_eq!(udp(ConnKind::Opened, from.port, 6200), 1, "first datagram out recorded once");
    assert_eq!(udp(ConnKind::Accepted, 6200, from.port), 1, "first datagram in recorded once");
    assert_eq!(udp(ConnKind::Opened, 6200, from.port), 0, "the answer to a known peer is no new record");
}

/// PM-2: the connection view: TCP bytes while the connection is live, the close ends it with them; UDP one
/// entry per peer with the bytes of every datagram, ended with the socket.
#[test]
fn the_view_counts_live_bytes_and_udp_peers() {
    use policy::{ConnFilter, NetProto};
    let n = net();
    let find = |proto: NetProto, local: u16, remote: u16| {
        policy::connections(&ConnFilter::default(), usize::MAX)
            .into_iter()
            .find(|i| i.proto == proto && i.local_port == local && i.remote_port == remote)
    };
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7135), 2).unwrap();
    let c = n.a.tcp_connect(at(B, 7135), T).unwrap();
    let s = l.accept(T).unwrap();
    let pa = c.local().port;
    c.write_all(&[1u8; 300], T).unwrap();
    assert_eq!(read_all(&*s, 300).len(), 300);
    let live = find(NetProto::Tcp, pa, 7135).expect("client entry");
    assert!(live.live() && !live.accepted);
    assert_eq!((live.bytes_in, live.bytes_out), (0, 300));
    assert!(find(NetProto::Tcp, 7135, pa).is_some_and(|i| i.live() && i.accepted && i.bytes_in == 300));
    drop(c);
    drop(s);
    eventually("both ended", || [(pa, 7135), (7135, pa)].iter().all(|&(a, b)| find(NetProto::Tcp, a, b).is_some_and(|i| !i.live())));
    let ub = n.b.udp_bind(at(Ipv4::ANY, 6210)).unwrap();
    let ua = n.a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    ua.send_to(b"abc", at(B, 6210)).unwrap();
    ua.send_to(b"de", at(B, 6210)).unwrap();
    let mut buf = [0u8; 16];
    let (_, from) = ub.recv_from(&mut buf, T).unwrap();
    ub.recv_from(&mut buf, T).unwrap();
    let out = find(NetProto::Udp, from.port, 6210).expect("sender entry");
    assert!(out.live() && (out.bytes_in, out.bytes_out) == (0, 5));
    let inn = find(NetProto::Udp, 6210, from.port).expect("receiver entry");
    assert!(inn.accepted && (inn.bytes_in, inn.bytes_out) == (5, 0));
    drop(ua);
    assert!(find(NetProto::Udp, from.port, 6210).is_some_and(|i| !i.live()), "a UDP peer ends with its socket");
    // A DNS answer on a kernel socket from the server it asked names nothing (only programs' own answers do).
    let dns = n.b.udp_bind(at(Ipv4::ANY, 53)).unwrap();
    let k = n.a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    let names = policy::dns_names();
    k.send_to(b"query", at(B, 53)).unwrap();
    let (_, q) = dns.recv_from(&mut buf, T).unwrap();
    let answer = [0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0, 1, b'k', 0, 0, 1, 0, 1, 0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, 7];
    dns.send_to(&answer, q).unwrap();
    let mut big = [0u8; 64];
    assert_eq!(k.recv_from(&mut big, T).unwrap().0, answer.len());
    assert_eq!(policy::dns_names(), names, "a kernel socket's answer was kept");
}

/// Never panic on input from the network: runts, bad lengths, fragments, broken ARP, oversized and random
/// frames go into the stack; it keeps working afterwards.
#[test]
fn hostile_frames_do_not_hurt() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7140), 2).unwrap();
    let mac_b = [2u8, 0, 0, 0, 0, 0xB];
    let eth = |ty: u16, body: &[u8]| -> Vec<u8> {
        let mut f = Vec::from(mac_b);
        f.extend_from_slice(&[2, 0, 0, 0, 0, 0xEE]);
        f.extend_from_slice(&ty.to_be_bytes());
        f.extend_from_slice(body);
        f
    };
    let ip = |len: u16, ihl: u8, flags_frag: u16, proto: u8, extra: &[u8]| -> Vec<u8> {
        let mut h = alloc::vec![0x40 | ihl, 0];
        h.extend_from_slice(&len.to_be_bytes());
        h.extend_from_slice(&[0, 1]);
        h.extend_from_slice(&flags_frag.to_be_bytes());
        h.extend_from_slice(&[64, proto, 0, 0]);
        h.extend_from_slice(&[10, 9, 0, 9]);
        h.extend_from_slice(&B.0);
        h.extend_from_slice(extra);
        eth(0x0800, &h)
    };
    let mut frames: Vec<Vec<u8>> = (0..14).map(|n| alloc::vec![0xFFu8; n]).collect();
    frames.push(eth(0x0800, &[]));
    frames.push(eth(0x1234, &[0xAB; 100]));
    frames.push(ip(1500, 5, 0, 17, &[0; 8]));
    frames.push(ip(10, 5, 0, 17, &[0; 8]));
    frames.push(ip(40, 2, 0, 6, &[0; 20]));
    frames.push(ip(40, 15, 0, 6, &[0; 20]));
    frames.push(ip(28, 5, 0x2000 | 1, 17, &[0x30, 0x39, 0x1B, 0x58, 0, 8, 0, 0]));
    frames.push(ip(28, 5, 0x0003, 1, &[8, 0, 0, 0, 0, 0, 0, 0]));
    let mut tcp = alloc::vec![0x30, 0x39, (7140u16 >> 8) as u8, 7140u16 as u8, 0, 0, 0, 1, 0, 0, 0, 0, 0x00, 0x02, 0xFF, 0xFF, 0, 0, 0, 0];
    frames.push(ip(40, 5, 0, 6, &tcp));
    tcp[12] = 0xF0;
    frames.push(ip(40, 5, 0, 6, &tcp));
    frames.push(eth(0x0806, &[0, 1, 8, 0, 0, 0, 0, 1]));
    frames.push(eth(0x0806, &[0, 1, 8, 0, 6, 4, 0, 1, 2, 0, 0, 0, 0, 0xEE]));
    frames.push(eth(0x0806, &[0, 1, 8, 0, 6, 4, 0, 2, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 10, 9, 0, 2]));
    frames.push(eth(0x0800, &[0x45; 9000]));
    let mut x: u32 = 0x1234_5678;
    for i in 0..300 {
        let len = 14 + (i * 7) % 300;
        let mut f = eth(if i % 2 == 0 { 0x0800 } else { 0x0806 }, &[]);
        f.truncate(14);
        while f.len() < len {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            f.push(x as u8);
        }
        if i % 3 == 0 && f.len() > 20 {
            f[14] = 0x45;
        }
        frames.push(f);
    }
    {
        let mut q = n.ab.lock();
        for f in frames {
            q.push_back(f);
        }
    }
    eventually("the stack took every frame", || n.ab.lock().is_empty());
    let (a, b) = (n.a, n.b);
    let got = within(20, "TCP after the hostile frames", move || {
        let c = a.tcp_connect(at(B, 7140), T).unwrap();
        let s = l.accept(T).unwrap();
        c.write_all(b"still here", T).unwrap();
        read_all(&*s, 10)
    });
    assert_eq!(got, b"still here");
    assert!(b.ping(A, 10 * S).is_ok());
}

/// Spec §2.4: the backlog replaces V1's knock. `backlog + SYN_SPARE` connections are held while nobody accepts,
/// even when they arrive at once; the next SYN gets RST (Linux would stay silent: pinned here, see the notes).
#[test]
fn backlog_with_spare_then_reset() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7150), 2).unwrap();
    let a = n.a;
    let held = 2 + iface::SYN_SPARE;
    let cs: Vec<_> = (0..held).map(|_| thread::spawn(move || a.tcp_connect(at(B, 7150), T))).collect();
    let cs: Vec<_> = cs.into_iter().map(|h| h.join().unwrap().expect("held, not refused")).collect();
    let shared = n.b.listeners.lock()[0].clone();
    eventually("all queued", || shared.queue.len() == held);
    assert_eq!(n.b.ifaces.lock()[0].listen_count(&shared), 0, "no listening socket beyond the bound");
    assert_eq!(n.a.tcp_connect(at(B, 7150), T).err(), Some(NetError::ConnectionRefused), "one more: RST");
    for (i, c) in cs.iter().enumerate() {
        c.write_all(&[i as u8], T).unwrap();
    }
    let mut seen: Vec<u8> = (0..held).map(|_| read_all(&*l.accept(T).unwrap(), 1)[0]).collect();
    seen.sort();
    assert_eq!(seen, (0..held as u8).collect::<Vec<_>>());
    assert!(n.a.tcp_connect(at(B, 7150), T).is_ok(), "room again after the accepts");
}

/// Linux flushes its neighbour table on a carrier change: after a cable move the same gateway address may have
/// another MAC. The first packet after link-up asks ARP again instead of using the old MAC.
#[test]
fn link_up_forgets_learnt_macs() {
    let n = net();
    let ub = n.b.udp_bind(at(Ipv4::ANY, 6300)).unwrap();
    let ua = n.a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    let mut buf = [0u8; 8];
    ua.send_to(b"1", at(B, 6300)).unwrap();
    ub.recv_from(&mut buf, T).unwrap();
    let _held = n.b.hold();
    n.ab.lock().clear();
    ua.send_to(b"2", at(B, 6300)).unwrap();
    eventually("datagram sent", || !n.ab.lock().is_empty());
    assert_eq!(ethertype(&n.ab.lock()[0]), 0x0800, "MAC known: the datagram goes straight out");
    let link = |up: bool| n.a.interfaces()[0].info.link_up == up;
    n.up.store(false, Ordering::Release);
    eventually("link down seen", || link(false));
    n.up.store(true, Ordering::Release);
    eventually("link up seen", || link(true));
    n.ab.lock().clear();
    ua.send_to(b"3", at(B, 6300)).unwrap();
    eventually("frame sent", || !n.ab.lock().is_empty());
    assert_eq!(arp_target(&n.ab.lock()[0]), Some(B), "asks for the MAC again after link-up");
}

/// The default route belongs to the first card with link and a router; it moves to the next card while the
/// first has no link and comes back with the link (spec §2.3), end to end through the stack.
#[test]
fn default_route_moves_with_the_link() {
    klog::set_serial(false);
    let (e1, _f1, q1, up1) = cable("c1", 0x11, "far1", 0x12);
    let (e2, _f2, q2, _up2) = cable("c2", 0x21, "far2", 0x22);
    let a = Stack::new_static(clock, 3, false);
    a.add_device(Arc::new(e1), false);
    a.add_device(Arc::new(e2), false);
    let (r1, r2) = (Ipv4([10, 9, 0, 254]), Ipv4([10, 8, 0, 254]));
    a.set_static("c1", A, 24, Some(r1)).unwrap();
    a.set_static("c2", Ipv4([10, 8, 0, 1]), 24, Some(r2)).unwrap();
    let _poll = poll_thread(alloc::vec![a]);
    let u = a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    let far = at(Ipv4([192, 0, 2, 53]), 53);
    let asks = |q: &Queue, r: Ipv4| q.lock().iter().any(|f| arp_target(f) == Some(r));
    let default_is = |i: usize| a.interfaces().iter().position(|x| x.default_route) == Some(i);
    eventually("default on c1", || default_is(0));
    u.send_to(b"1", far).unwrap();
    eventually("out through c1's router", || asks(&q1, r1));
    up1.store(false, Ordering::Release);
    eventually("default moved to c2", || default_is(1));
    u.send_to(b"2", far).unwrap();
    eventually("out through c2's router", || asks(&q2, r2));
    up1.store(true, Ordering::Release);
    eventually("default back on c1", || default_is(0));
    q1.lock().clear();
    u.send_to(b"3", far).unwrap();
    eventually("out through c1 again", || !q1.lock().is_empty());
}

/// Spec §2.4: a private-only listener (MCP) answers a SYN from a public address with RST before any connection
/// exists; it allocates nothing for it. NW-4: so does every listener `KSocket` makes (B8); only the stack's own
/// `tcp_listen_public` (tests) accepts the same peer.
#[test]
fn private_only_listener_refuses_public_peers() {
    let (pub_a, pub_b) = (Ipv4([198, 51, 100, 1]), Ipv4([198, 51, 100, 2]));
    let n = net_addrs(clock, pub_a, pub_b, true);
    let l = n.b.tcp_listen_private(at(Ipv4::ANY, 7160), 2).unwrap();
    let open = n.b.tcp_listen_public(at(Ipv4::ANY, 7161), 2).unwrap();
    let plain = n.b.tcp_listen(at(Ipv4::ANY, 7162), 2).unwrap();
    assert_eq!(n.a.tcp_connect(at(pub_b, 7162), T).err(), Some(NetError::ConnectionRefused), "a plain listener is private too");
    drop(plain);
    let shared = n.b.listeners.lock().iter().find(|x| x.port == 7160).cloned().unwrap();
    let card = n.b.ifaces.lock()[0].clone();
    let before = card.listen_count(&shared);
    for _ in 0..3 {
        assert_eq!(n.a.tcp_connect(at(pub_b, 7160), T).err(), Some(NetError::ConnectionRefused), "public peer: RST");
    }
    eventually("listening again", || card.listen_count(&shared) == before);
    assert!(shared.queue.is_empty(), "nothing was accepted");
    let c = n.a.tcp_connect(at(pub_b, 7161), T).expect("a normal listener takes it");
    let s = open.accept(T).unwrap();
    c.write_all(b"ok", T).unwrap();
    assert_eq!(read_all(&*s, 2), b"ok");
    drop(l);
}

// ---------------------------------------------------------------------------------------------------------
// NW-3b

/// RFC 6056: the ephemeral counter starts at a point chosen from the seed, so two boots (or a kernel and the
/// one it reloaded) do not begin with the same 4-tuple. Same seed, same start (host tests stay repeatable).
#[test]
fn ephemeral_ports_start_at_a_random_point() {
    let starts: std::collections::BTreeSet<u16> = (0..64u64).map(|s| ephemeral_start(s.wrapping_mul(0x1234_5678_9ABC))).collect();
    assert!(starts.iter().all(|p| EPHEMERAL.contains(p)));
    assert!(starts.len() > 60, "64 seeds, {} distinct starts", starts.len());
    assert_eq!(ephemeral_start(7), ephemeral_start(7));
    // Neighbouring TSC readings (the kernel's seed) land far apart.
    assert_ne!(ephemeral_start(1000), ephemeral_start(1001));
    // The first connection uses the first port, the next one the port after it.
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7200), 2).unwrap();
    let first = n.a.first_ephemeral_port();
    assert_eq!(first, ephemeral_start(1), "stack A's seed is 1");
    let c1 = n.a.tcp_connect(at(B, 7200), T).unwrap();
    let c2 = n.a.tcp_connect(at(B, 7200), T).unwrap();
    assert_eq!(c1.local().port, first);
    assert_eq!(c2.local().port, if first == *EPHEMERAL.end() { *EPHEMERAL.start() } else { first + 1 });
    let s = l.accept(T).unwrap();
    assert_eq!(s.peer().port, first);
    assert_ne!(n.a.first_ephemeral_port(), n.b.first_ephemeral_port(), "seeds 1 and 2 differ");
}

/// R2: socket buffers are asked for. A connect without memory fails with `NoMemory` before any packet; the
/// next one (memory back) works.
#[test]
fn connect_without_memory_is_no_memory() {
    let n = net();
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7210), 2).unwrap();
    let before = NO_MEMORY_INJECTED.get();
    n.ab.lock().clear();
    n.a.inject_no_memory(1, None);
    assert_eq!(n.a.tcp_connect(at(B, 7210), T).err(), Some(NetError::NoMemory));
    assert!(NO_MEMORY_INJECTED.get() > before, "counted as injected (net.no_memory is only what the heap refused)");
    let c = n.a.tcp_connect(at(B, 7210), T).expect("memory back");
    let s = l.accept(T).unwrap();
    c.write_all(b"ok", T).unwrap();
    assert_eq!(read_all(&*s, 2), b"ok");
    // A receive window nobody can give (4 EiB) is refused by the allocator itself, not by the injection.
    let real = NO_MEMORY.get();
    assert_eq!(n.a.tcp_connect_sized(at(B, 7210), T, 1 << 62, 4096).err(), Some(NetError::NoMemory));
    assert!(NO_MEMORY.get() > real, "a refusal by the heap counts in net.no_memory");
    // UDP and ping take their buffers the same way.
    n.a.inject_no_memory(1, None);
    assert_eq!(n.a.udp_bind(at(Ipv4::ANY, 7211)).err(), Some(NetError::NoMemory));
    let u = n.a.udp_bind(at(Ipv4::ANY, 7211)).expect("port given back after the refusal");
    drop(u);
    n.a.inject_no_memory(1, None);
    assert_eq!(n.a.ping(B, S).err(), Some(NetError::NoMemory));
    assert!(n.a.ping(B, 5 * S).is_ok());
}

/// R2 for listeners: no listening socket for a card at all = `NoMemory` and nothing left behind (port free);
/// a refill short of memory in a poll round keeps what it has, is noted once, and fills up when memory is back.
#[test]
fn listen_without_memory() {
    let n = net_on(clock, false);
    let mut pb = Poller::new(n.b);
    n.b.inject_no_memory(1000, None);
    assert_eq!(n.b.tcp_listen(at(Ipv4::ANY, 7220), 2).err(), Some(NetError::NoMemory));
    assert!(n.b.listeners.lock().is_empty(), "nothing left behind");
    n.b.inject_no_memory(0, None);
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7220), 2).expect("port free, memory back");
    let shared = n.b.listeners.lock()[0].clone();
    let card = n.b.ifaces.lock()[0].clone();
    let want = card.listen_count(&shared);
    assert_eq!(want, 2 + iface::SYN_SPARE);
    // Take all but one listening socket away (as accepted connections would) and refill without memory.
    card.shrink_listen(n.b, 1);
    n.b.inject_no_memory(1000, None);
    pb.round();
    assert_eq!(card.listen_count(&shared), 1, "kept what it had, nothing new");
    assert!(card.listen_short(), "short noted");
    n.b.inject_no_memory(0, None);
    // NW-7: right after a refusal the poll round does not ask the heap again (the pause), then it does.
    let deferred = BG_ALLOC_DEFERRED.get();
    pb.round();
    assert_eq!(card.listen_count(&shared), 1, "asked again within the pause");
    assert!(BG_ALLOC_DEFERRED.get() > deferred, "the skipped refill is counted");
    std::thread::sleep(std::time::Duration::from_nanos(2 * BG_RETRY_MIN_NS));
    pb.round();
    assert_eq!(card.listen_count(&shared), want, "filled up again");
    assert!(!card.listen_short());
    drop(l);
}

/// NW-7: the pause after a refused background allocation doubles from 1 ms up to 250 ms and ends with success.
#[test]
fn background_retry_pause_doubles_and_ends() {
    let n = net_on(clock, false);
    let s = n.b;
    assert!(s.bg_alloc_open());
    let mut gaps = Vec::new();
    for _ in 0..12 {
        s.bg_alloc_done(false);
        gaps.push(s.bg_gap_ns.load(Ordering::Relaxed));
    }
    assert_eq!(gaps[0], BG_RETRY_MIN_NS);
    assert_eq!(gaps[1], 2 * BG_RETRY_MIN_NS);
    assert_eq!(*gaps.last().unwrap(), BG_RETRY_MAX_NS, "capped: {gaps:?}");
    assert!(!s.bg_alloc_open(), "closed within the pause");
    s.bg_alloc_done(true);
    assert!(s.bg_alloc_open(), "success ends the pause");
    s.bg_alloc_done(false);
    assert_eq!(s.bg_gap_ns.load(Ordering::Relaxed), BG_RETRY_MIN_NS, "starts small again");
}

/// The picker is bounded (B4): with every port taken it answers `None` after one pass instead of spinning
/// under `net.ports`. A port whose 4-tuple a live connection holds is skipped; the same port towards another
/// peer port is fine (Linux `__inet_hash_connect`).
#[test]
fn ephemeral_picker_is_bounded_and_skips_live_tuples() {
    let n = net();
    assert_eq!(n.a.ephemeral_port(|_| true), None, "all taken: one pass, then None");
    assert!(n.a.ephemeral_port(|_| false).is_some());
    let l1 = n.b.tcp_listen(at(Ipv4::ANY, 7240), 2).unwrap();
    let l2 = n.b.tcp_listen(at(Ipv4::ANY, 7241), 2).unwrap();
    let c1 = n.a.tcp_connect(at(B, 7240), T).unwrap();
    let p = c1.local().port;
    // The counter wrapped back onto p (16,384 connects later).
    *n.a.next_port.lock() = p;
    let c2 = n.a.tcp_connect(at(B, 7240), T).unwrap();
    assert_ne!(c2.local().port, p, "same 4-tuple as c1: skipped");
    *n.a.next_port.lock() = p;
    let c3 = n.a.tcp_connect(at(B, 7241), T).unwrap();
    assert_eq!(c3.local().port, p, "another peer port: the tuple is free");
    let (s1, s2, s3) = (l1.accept(T).unwrap(), l1.accept(T).unwrap(), l2.accept(T).unwrap());
    for (c, s) in [(&c1, &s1), (&c2, &s2), (&c3, &s3)] {
        c.write_all(b"t", T).unwrap();
        assert_eq!(read_all(&**s, 1), b"t");
    }
}

/// Link-local peers are refused by a private-only listener too (V1 `tcp_private`: RFC 1918 alone).
#[test]
fn private_only_listener_refuses_link_local() {
    let (ll_a, ll_b) = (Ipv4([169, 254, 7, 1]), Ipv4([169, 254, 7, 2]));
    let n = net_addrs(clock, ll_a, ll_b, true);
    let l = n.b.tcp_listen_private(at(Ipv4::ANY, 7250), 2).unwrap();
    let before = TCP_REFUSED_PUBLIC.get();
    assert_eq!(n.a.tcp_connect(at(ll_b, 7250), T).err(), Some(NetError::ConnectionRefused));
    assert!(TCP_REFUSED_PUBLIC.get() > before, "counted");
    assert!(l.accept(Some(S / 10)).is_err(), "nothing accepted");
}

/// R2: a listener whose refill runs out of memory keeps the listening socket it has and still accepts a
/// connection through it; once that one is used, a SYN gets RST (bounded, nothing allocated); when memory is
/// back the card fills up and accepts again.
#[test]
fn short_listener_still_accepts() {
    let n = net_on(clock, false);
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7260), 2).unwrap();
    let shared = n.b.listeners.lock()[0].clone();
    let card = n.b.ifaces.lock()[0].clone();
    let want = card.listen_count(&shared);
    card.shrink_listen(n.b, 1);
    n.b.inject_no_memory(u32::MAX, None);
    {
        let mut pb = Poller::new(n.b);
        pb.round();
    }
    assert_eq!(card.listen_count(&shared), 1, "kept the one it had");
    assert!(card.listen_short());
    let _poll = poll_thread(alloc::vec![n.a, n.b]);
    let c = n.a.tcp_connect(at(B, 7260), T).expect("the kept socket takes it");
    let s = l.accept(T).unwrap();
    c.write_all(b"kept", T).unwrap();
    assert_eq!(read_all(&*s, 4), b"kept");
    eventually("used up", || card.listen_count(&shared) == 0);
    assert_eq!(n.a.tcp_connect(at(B, 7260), T).err(), Some(NetError::ConnectionRefused), "no listening socket left: RST");
    n.b.inject_no_memory(0, None);
    eventually("filled up again", || !card.listen_short() && card.listen_count(&shared) == want);
    let c2 = n.a.tcp_connect(at(B, 7260), T).expect("memory back");
    let s2 = l.accept(T).unwrap();
    c2.write_all(b"back", T).unwrap();
    assert_eq!(read_all(&*s2, 4), b"back");
}

// ---------------------------------------------------------------------------------------------------------
// NW-4

/// The MCP reserve (V2-PLAN §6): every socket buffer refused on B, on every thread (a program took the memory),
/// and still the reserved listener accepts 3 connections side by side and serves them, again and again: its
/// kept sockets come back to the pool after each connection. A plain listener made meanwhile gets nothing. The
/// pool holds at most what the reserved listener wants per card.
#[test]
fn reserve_accepts_three_without_memory() {
    let n = net();
    let mcp = n.b.tcp_listen_private(at(Ipv4::ANY, 7300), 3).unwrap();
    let shared = n.b.listeners.lock().iter().find(|x| x.port == 7300).cloned().unwrap();
    assert!(shared.reserve && shared.private_only);
    let card = n.b.ifaces.lock()[0].clone();
    let want = iface::listen_want(&shared);
    assert!(want >= iface::RESERVE_MIN);
    assert_eq!(card.listen_count(&shared), want, "pre-created before tcp_listen_private returned");
    // (The poll thread may be making one of them when `tcp_listen_private` returns.)
    eventually("spares made too", || {
        let k = n.b.kept_info();
        (k.made, k.idle, k.spare) == (want + iface::RESERVE_MIN, iface::RESERVE_MIN, iface::RESERVE_MIN)
    });
    n.b.inject_no_memory(u32::MAX, None);
    assert_eq!(n.b.tcp_listen(at(Ipv4::ANY, 7301), 2).err(), Some(NetError::NoMemory), "no memory for anyone else");
    for round in 0..4 {
        let returned = n.b.kept_info().returned;
        let cs: Vec<Box<dyn TcpStream>> = (0..3).map(|_| n.a.tcp_connect(at(B, 7300), T).expect("reserve takes it")).collect();
        let ss: Vec<Box<dyn TcpStream>> = (0..3).map(|_| mcp.accept(T).unwrap()).collect();
        // Refilled after accept without any heap: the spares listen while the 3 connections are still open.
        eventually("listening sockets refilled from the spares", || card.listen_count(&shared) == want);
        for (c, s) in cs.iter().zip(&ss) {
            c.write_all(b"log", T).unwrap();
            assert_eq!(read_all(&**s, 3), b"log");
            s.write_all(b"answer", T).unwrap();
            assert_eq!(read_all(&**c, 6), b"answer", "round {round}");
        }
        drop(ss);
        drop(cs);
        eventually("kept sockets back as spares", || {
            n.b.kept_info().returned >= returned + 3 && card.listen_count(&shared) == want && card.orphan_count() == 0
        });
        assert_eq!(n.b.kept_info().idle, iface::RESERVE_MIN, "round {round}: spares whole again");
    }
    let k = n.b.kept_info();
    assert_eq!(k.made, want + iface::RESERVE_MIN, "the pool did not grow past what it serves: {k:?}");
    assert!(k.idle <= k.cap && k.cap == want + iface::RESERVE_MIN, "{k:?}");
    // The test helper that takes listening sockets away gives kept ones back (no drift in `made`).
    // (Memory is still refused: the refill can only take them from the pool again.)
    card.shrink_listen(n.b, 0);
    eventually("refilled from the pool", || card.listen_count(&shared) == want);
    let k2 = n.b.kept_info();
    assert_eq!((k2.made, k2.idle), (k.made, k.idle), "{k2:?}");
    n.b.inject_no_memory(0, None);
    drop(mcp);
    eventually("listener gone: its kept sockets freed", || { let k = n.b.kept_info(); (k.idle, k.made, k.cap) == (0, 0, 0) });
}

/// TCP keep-alive probes (spec §2.4, V1 README "MCP Protokoll"): Windows/undici send one already-acknowledged
/// byte (seq = rcv_nxt - 1), Linux a bare segment at rcv_nxt - 1; both must be answered with an ACK of rcv_nxt,
/// also while the server side has not read what came (a tool runs), or the client gives the connection up after
/// ~70 s. Driven by hand: B's ACKs are read off the cable.
#[test]
fn keepalive_probes_are_acked() {
    use smoltcp::wire::{EthernetFrame, IpProtocol, Ipv4Packet, TcpPacket};
    klog::set_serial(false);
    let (ab, ba) = (queue(), queue());
    let up = Arc::new(AtomicBool::new(true));
    let ea = End { name: "ka-a".into(), mac: [2, 0, 0, 0, 0, 0x1A], rx: ba.clone(), tx: ab.clone(), up: up.clone(), ring: usize::MAX };
    let eb = End { name: "ka-b".into(), mac: [2, 0, 0, 0, 0, 0x1B], rx: ab.clone(), tx: ba.clone(), up, ring: usize::MAX };
    let (a, b) = (Stack::new_static(clock, 11, false), Stack::new_static(clock, 12, false));
    a.add_device(Arc::new(ea), false);
    b.add_device(Arc::new(eb), false);
    a.set_static("ka-a", A, 24, None).unwrap();
    b.set_static("ka-b", B, 24, None).unwrap();
    let l = b.tcp_listen(at(Ipv4::ANY, 7310), 2).unwrap();
    let (c, s) = {
        let poll = poll_thread(alloc::vec![a, b]);
        let c = a.tcp_connect(at(B, 7310), T).unwrap();
        let s = l.accept(T).unwrap();
        c.write_all(b"req", T).unwrap();
        assert_eq!(read_all(&*s, 3), b"req");
        // Both sides quiet: nothing unacknowledged, nothing left on the cable.
        // (B ACKs at once, `tune_listen`; A takes that ACK within the next two passes.)
        poll.settle();
        poll.settle();
        eventually("cable empty", || ab.lock().is_empty() && ba.lock().is_empty());
        (c, s)
    };
    ab.lock().clear();
    ba.lock().clear();
    let (mut pa, mut pb) = (Poller::new(a), Poller::new(b));
    // One byte from A that B never reads (a tool runs): captured on the cable, delivered once.
    c.write_all(b"x", T).unwrap();
    let data = (0..100).find_map(|_| {
        pa.round();
        thread::sleep(std::time::Duration::from_millis(1));
        ab.lock().iter().find(|f| tcp_payload_len(f) == Some(1)).cloned()
    });
    let data = data.expect("A sent its byte");
    ab.lock().clear();
    let seq = tcp_seq(&data);
    ab.lock().push_back(data.clone());
    pb.round();
    ba.lock().clear();
    let acks = |q: &Queue, want: u32| q.lock().iter().any(|f| tcp_payload_len(f) == Some(0) && tcp_ack(f) == Some(want));
    // Windows-style probe: the acknowledged byte again.
    ab.lock().push_back(data.clone());
    pb.round();
    assert!(acks(&ba, seq.wrapping_add(1)), "1-byte probe answered with ACK of rcv_nxt");
    ba.lock().clear();
    // Linux-style probe: no payload, seq = rcv_nxt - 1.
    let mut bare = data[..data.len() - 1].to_vec();
    assert!(EthernetFrame::new_checked(&bare[..]).is_ok());
    let (src, dst) = {
        let ip = Ipv4Packet::new_unchecked(&bare[14..]);
        (ip.src_addr(), ip.dst_addr())
    };
    {
        let mut ip = Ipv4Packet::new_unchecked(&mut bare[14..]);
        let tl = ip.total_len() - 1;
        ip.set_total_len(tl);
        ip.fill_checksum();
        assert_eq!(ip.next_header(), IpProtocol::Tcp);
        let mut t = TcpPacket::new_unchecked(ip.payload_mut());
        t.fill_checksum(&src.into(), &dst.into());
    }
    ab.lock().push_back(bare);
    pb.round();
    assert!(acks(&ba, seq.wrapping_add(1)), "bare probe answered with ACK of rcv_nxt");
    // The byte is still there for the server when it reads; the connection is alive both ways.
    let _poll = poll_thread(alloc::vec![a, b]);
    assert_eq!(read_all(&*s, 1), b"x");
    s.write_all(b"ok", T).unwrap();
    assert_eq!(read_all(&*c, 2), b"ok");
}

fn tcp_of(f: &[u8]) -> Option<(u32, Option<u32>, usize)> {
    use smoltcp::wire::{EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, TcpPacket};
    let eth = EthernetFrame::new_checked(f).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Tcp {
        return None;
    }
    let t = TcpPacket::new_checked(ip.payload()).ok()?;
    Some((t.seq_number().0 as u32, t.ack().then(|| t.ack_number().0 as u32), t.payload().len()))
}

fn tcp_payload_len(f: &[u8]) -> Option<usize> {
    tcp_of(f).map(|x| x.2)
}

fn tcp_seq(f: &[u8]) -> u32 {
    tcp_of(f).map(|x| x.0).unwrap_or(0)
}

fn tcp_ack(f: &[u8]) -> Option<u32> {
    tcp_of(f).and_then(|x| x.1)
}

/// NW-7 fixer, back pressure (Linux `sk_stream_wait_memory`, linux-6.6/net/core/stream.c:117): a writer whose peer
/// does not read blocks once the peer's window and its own send buffer are full and ends with TimedOut at its
/// bound (B4); what it queued is bounded by those buffers and arrives in order without a gap. Then, with a TX ring
/// of 4 frames that refuses most of a window (the card's ring full), a slow reader still gets 2 MB intact and in
/// time: the refused frame is held and sent first in the next round, not lost to TCP's 1 s retransmit timer.
#[test]
fn back_pressure_bounds_the_writer_and_loses_nothing() {
    let n = net_ring(clock, A, B, true, 4);
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7230), 2).unwrap();
    let c: Arc<dyn TcpStream> = Arc::from(n.a.tcp_connect(at(B, 7230), T).unwrap());
    let s = l.accept(T).unwrap();
    let data: Vec<u8> = (0..4_000_000u32).map(|i| (i * 13 + i / 509) as u8).collect();
    let t0 = std::time::Instant::now();
    assert_eq!(c.write_all(&data, Some(300_000_000)), Err(NetError::TimedOut), "a writer nobody reads from must block, not queue 4 MB");
    assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the blocked write ended at its bound (B4)");
    // Everything the writer got rid of sits in its send buffer and the peer's receive buffer.
    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match s.read(&mut buf, Some(500_000_000)) {
            Ok(k) if k > 0 => got.extend_from_slice(&buf[..k]),
            Ok(_) => panic!("EOF"),
            Err(NetError::TimedOut) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(!got.is_empty() && got.len() <= tcp::CONNECT_TX + tcp::LISTEN_RX, "queued {} bytes, more than the buffers hold", got.len());
    assert!(got[..] == data[..got.len()], "the queued bytes arrived changed or with a gap");
    // A slow reader behind a full TX ring: the rest of a 2 MB stream from where the first write stopped.
    let off = got.len();
    let rest = data[off..off + 2_000_000].to_vec();
    let w = c.clone();
    let writer = thread::spawn(move || w.write_all(&rest, T));
    let requeued = TX_REQUEUED.get();
    let mut tail = Vec::new();
    while tail.len() < 2_000_000 {
        let k = s.read(&mut buf, T).unwrap();
        assert!(k > 0, "EOF after {}", tail.len());
        tail.extend_from_slice(&buf[..k]);
        if tail.len() < 256 * 1024 {
            thread::sleep(std::time::Duration::from_micros(200));
        }
    }
    writer.join().unwrap().unwrap();
    assert!(tail[..] == data[off..off + 2_000_000], "2 MB through a full TX ring arrived changed");
    assert!(TX_REQUEUED.get() > requeued, "the full TX ring refused frames and they were held (net.tx_requeued)");
}
