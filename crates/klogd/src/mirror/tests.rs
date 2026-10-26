//! Host tests of the mirror: the normal path through a fake UDP socket, read back by a model of
//! `console-listen.mjs` (its `take`/`text` rules: start datagram, positions, `[... n bytes lost]`, line ends),
//! the early buffer, losses, the emergency path and quiesce on a fake card.

use super::ktests::{FakeCard, fake_route, log_text};
use super::wire::{self, DGRAM_MAX, HEAD, Kind, Target};
use super::Mirror;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use klog::{Level, Sink};
use ksync::SpinLock;
use net::{NetError, SockAddr, UdpSocket};

const T: Target = Target { ip: [10, 0, 2, 2], port: 23300 };
const SRC: [u8; 4] = [10, 0, 2, 15];
static CLOCK: AtomicU64 = AtomicU64::new(5_000_000_000);
fn clock() -> u64 {
    CLOCK.fetch_add(1000, Ordering::Relaxed)
}

struct FakeSock {
    sent: SpinLock<Vec<Vec<u8>>>,
    fail: AtomicBool,
}
impl Default for FakeSock {
    fn default() -> Self {
        FakeSock { sent: SpinLock::new(Vec::new()), fail: AtomicBool::new(false) }
    }
}
impl UdpSocket for FakeSock {
    fn send_to(&self, buf: &[u8], to: SockAddr) -> Result<(), NetError> {
        assert_eq!((to.ip.0, to.port), (T.ip, T.port));
        if self.fail.load(Ordering::Acquire) {
            return Err(NetError::WouldBlock);
        }
        self.sent.lock().push(buf.to_vec());
        Ok(())
    }
    fn recv_from(&self, _: &mut [u8], _: Option<u64>) -> Result<(usize, SockAddr), NetError> {
        Err(NetError::WouldBlock)
    }
}

/// What console-listen.mjs prints for one sender's datagrams (log stream; the screen stream likewise).
struct Listener {
    id: Option<u32>,
    log_next: Option<u64>,
    scr_next: Option<u64>,
    log_line: String,
    scr_line: String,
    out: Vec<String>,
}

impl Listener {
    fn new() -> Self {
        Listener { id: None, log_next: None, scr_next: None, log_line: String::new(), scr_line: String::new(), out: Vec::new() }
    }
    fn take(&mut self, d: &[u8]) {
        let h = wire::parse_head(d).expect("the listener ignores this datagram");
        let body = &d[HEAD..];
        match h.kind {
            Kind::Begin => {
                let n = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let ip = &body[4..8];
                self.id = Some(h.id);
                self.log_next = Some(h.pos);
                self.scr_next = Some(0);
                self.out.push(alloc::format!("==== {}.{}.{}.{} start {n}: mirror from {} ====", ip[0], ip[1], ip[2], ip[3], if body[8] == 1 { "Console" } else { "CONFIG.TXT" }));
            }
            Kind::End => {
                let scr_end = u64::from_le_bytes(body[0..8].try_into().unwrap());
                let log_end = u64::from_le_bytes(body[8..16].try_into().unwrap());
                if log_end > self.log_next.unwrap() {
                    self.log_line.push_str(&alloc::format!("[... {} bytes lost]", log_end - self.log_next.unwrap()));
                }
                if scr_end > self.scr_next.unwrap() {
                    self.scr_line.push_str(&alloc::format!("[... {} bytes lost]", scr_end - self.scr_next.unwrap()));
                }
                for (mark, line) in [("log~ ", &mut self.log_line), ("scr~ ", &mut self.scr_line)] {
                    if !line.is_empty() {
                        self.out.push(alloc::format!("{mark}{line}"));
                        line.clear();
                    }
                }
                self.out.push(String::from("==== mirror ended ===="));
            }
            Kind::Log | Kind::Screen => {
                assert_eq!(Some(h.id), self.id, "a start the listener never saw");
                let (next, line, mark) = if h.kind == Kind::Log {
                    (&mut self.log_next, &mut self.log_line, "log| ")
                } else {
                    (&mut self.scr_next, &mut self.scr_line, "scr| ")
                };
                let n = next.unwrap();
                let (mut pos, mut bytes) = (h.pos, body);
                if pos + bytes.len() as u64 <= n {
                    return;
                }
                if pos < n {
                    bytes = &bytes[(n - pos) as usize..];
                    pos = n;
                }
                if pos > n {
                    line.push_str(&alloc::format!("[... {} bytes lost]", pos - n));
                }
                for &b in bytes {
                    if b == b'\n' {
                        self.out.push(alloc::format!("{mark}{line}"));
                        line.clear();
                    } else {
                        assert!(b >= 32, "control byte {b} would garble the listener");
                        line.push(if b < 127 { b as char } else { '¤' });
                    }
                }
                *next = Some(pos + bytes.len() as u64);
            }
        }
    }
}

fn mirror(bytes: usize) -> &'static Mirror {
    alloc::boxed::Box::leak(alloc::boxed::Box::new(Mirror::new(T, bytes, clock).unwrap()))
}

fn start(m: &Mirror, sock: &FakeSock) -> u64 {
    let mut b = [0u8; DGRAM_MAX];
    let (n, pos, _) = m.begin(0xAB, 2, [192, 168, 100, 20], &mut b).unwrap();
    sock.send_to(&b[..n], SockAddr { ip: net::Ipv4(T.ip), port: T.port }).unwrap();
    m.set_live(true);
    pos
}

#[test]
fn the_boot_before_the_network_reaches_the_listener_in_order() {
    let m = mirror(64 << 10);
    let sock = FakeSock::default();
    for i in 0..200 {
        m.write_line(Level::Info, &alloc::format!("[    0.{i:06}] I boot line n={i}"));
    }
    assert_eq!(m.flush(&sock, 10).unwrap(), (0, 0), "not live: nothing goes out (R9 until the network)");
    assert_eq!(start(m, &sock), 0);
    m.write_line(Level::Info, "[    1.000000] I net dhcp.lease ip=10.0.2.15");
    let (n, lost) = m.flush(&sock, 1000).unwrap();
    assert!(n >= 5 && lost == 0, "{n} datagrams");
    assert_eq!(m.pending(), 0);
    let mut l = Listener::new();
    for d in sock.sent.lock().iter() {
        assert!(d.len() <= DGRAM_MAX);
        l.take(d);
    }
    assert_eq!(l.out[0], "==== 192.168.100.20 start 2: mirror from CONFIG.TXT ====");
    assert_eq!(l.out.len(), 202);
    for i in 0..200 {
        assert_eq!(l.out[1 + i], alloc::format!("log| [    0.{i:06}] I boot line n={i}"));
    }
    assert_eq!(l.out[201], "log| [    1.000000] I net dhcp.lease ip=10.0.2.15");
    assert!(l.log_line.is_empty(), "every datagram ends at a line end when more followed");
}

#[test]
fn an_overflow_before_the_network_starts_at_a_whole_line_and_says_nothing_lost() {
    let m = mirror(1000);
    let sock = FakeSock::default();
    for i in 0..100 {
        m.write_line(Level::Info, &alloc::format!("[    0.{i:06}] I early n={i}"));
    }
    let pos = start(m, &sock);
    let (written, _, oldest, lost, _) = m.ring_state();
    assert!(pos > oldest && pos < written && lost == 0, "pos={pos} oldest={oldest}");
    m.flush(&sock, 100).unwrap();
    let mut l = Listener::new();
    sock.sent.lock().iter().for_each(|d| l.take(d));
    assert!(l.out[1].starts_with("log| [    0.0000") && l.out[1].contains("I early n="), "{:?}", l.out[1]);
    assert!(l.out.iter().all(|x| !x.contains("lost")));
    assert_eq!(l.out.last().unwrap(), "log| [    0.000099] I early n=99");
}

#[test]
fn text_overwritten_while_sends_fail_shows_as_exact_bytes_lost() {
    let m = mirror(400);
    let sock = FakeSock::default();
    start(m, &sock);
    m.write_line(Level::Info, "first");
    m.flush(&sock, 10).unwrap();
    sock.fail.store(true, Ordering::Release);
    m.write_line(Level::Info, "x".repeat(150).as_str());
    assert_eq!(m.flush(&sock, 10), Err(NetError::WouldBlock));
    for i in 0..4 {
        m.write_line(Level::Info, &alloc::format!("{i}{}", "y".repeat(99)));
    }
    sock.fail.store(false, Ordering::Release);
    let (_, lost) = m.flush(&sock, 10).unwrap();
    assert!(lost > 0);
    let mut l = Listener::new();
    sock.sent.lock().iter().for_each(|d| l.take(d));
    assert_eq!(l.out[1], "log| first");
    let gap = l.out.iter().find(|x| x.contains("bytes lost")).expect("a loss marker");
    assert!(gap.starts_with(&alloc::format!("log| [... {lost} bytes lost]")), "{gap}");
    assert_eq!(l.out.last().unwrap(), &alloc::format!("log| 3{}", "y".repeat(99)));
    assert_eq!(m.ring_state().3, lost);
}

#[test]
fn utf8_lines_arrive_as_cp437() {
    let m = mirror(4096);
    let sock = FakeSock::default();
    start(m, &sock);
    m.write_line(Level::Info, "Grüße → ok");
    m.flush(&sock, 10).unwrap();
    let d = sock.sent.lock()[1].clone();
    assert_eq!(&d[HEAD..], b"Gr\x81\xe1e ? ok\n");
}

#[test]
fn emergency_path_on_a_fake_card() {
    let card = FakeCard::new();
    let m = mirror(64 << 10);
    let sock = FakeSock::default();
    // Not live: nothing on the wire, but the line is kept for later.
    assert_eq!(m.emergency("early panic").frames, 0);
    m.set_route(Some(fake_route(&card)));
    let pos = start(m, &sock);
    assert_eq!(pos, 0);
    m.write_line(Level::Info, "sent normally");
    m.flush(&sock, 10).unwrap();
    m.write_line(Level::Info, "queued, not sent");
    let rep = m.emergency("PANIC line");
    assert_eq!((rep.frames, rep.dropped, rep.screen), (1, false, false));
    // The first emergency send repeats the tail: the listener drops what it has, so normal + raw read as one.
    let mut l = Listener::new();
    sock.sent.lock().iter().for_each(|d| l.take(d));
    for d in card.datagrams([10, 0, 2, 15]).unwrap() {
        l.take(&d);
    }
    assert_eq!(&l.out[1..], &["log| early panic", "log| sent normally", "log| queued, not sent", "log| PANIC line"]);
    // klogd's copy of the record is not appended a second time.
    let w = m.ring_state().0;
    m.write_line(Level::Error, "PANIC line");
    assert_eq!(m.ring_state().0, w);
    m.write_line(Level::Error, "PANIC line");
    assert!(m.ring_state().0 > w, "skipped once only");
    // Held ring: the screen stream.
    card.frames.lock().clear();
    {
        let _g = m.st.lock();
        let rep = m.emergency("held");
        assert!(rep.screen && rep.frames == 1);
    }
    for d in card.datagrams([10, 0, 2, 15]).unwrap() {
        l.take(&d);
    }
    assert_eq!(l.out.last().unwrap(), "scr| held");
    // Quiesce: the rest and the end; afterwards nothing.
    card.frames.lock().clear();
    let r = m.stop();
    assert!(r.was_live && r.ended && !r.timed_out && !r.no_route, "{r:?}");
    for d in card.datagrams([10, 0, 2, 15]).unwrap() {
        l.take(&d);
    }
    assert_eq!(&l.out[l.out.len() - 2..], &["log| PANIC line", "==== mirror ended ===="]);
    assert_eq!(m.stop(), super::StopReport::default(), "a second stop does nothing");
    assert_eq!(m.emergency("late").frames, 0);
    assert_eq!(m.flush(&sock, 10).unwrap(), (0, 0));
    let text = log_text(&sock.sent.lock()[1..], 0xAB, 0).unwrap();
    assert!(text.starts_with(b"early panic\nsent normally\n"));
}

#[test]
fn a_full_card_drops_and_the_text_goes_next_time() {
    let card = FakeCard::new();
    let m = mirror(64 << 10);
    let sock = FakeSock::default();
    m.set_route(Some(fake_route(&card)));
    start(m, &sock);
    card.refuse.store(true, Ordering::Release);
    let rep = m.emergency("one");
    assert!(rep.dropped && rep.frames == 0);
    card.refuse.store(false, Ordering::Release);
    let rep = m.emergency("two");
    assert_eq!(rep.frames, 1);
    let mut l = Listener::new();
    sock.sent.lock().iter().for_each(|d| l.take(d));
    card.datagrams([10, 0, 2, 15]).unwrap().iter().for_each(|d| l.take(d));
    assert_eq!(&l.out[1..], &["log| one", "log| two"]);
}

fn routes_to(m: &Mirror, c: &Arc<FakeCard>) -> bool {
    m.route().is_some_and(|r| core::ptr::addr_eq(Arc::as_ptr(&r.card), Arc::as_ptr(c)))
}

#[test]
fn route_snapshots_publish_only_changes_and_stay_two_slots() {
    let a = FakeCard::new();
    let b = FakeCard::new();
    let m = mirror(1024);
    assert!(m.route().is_none());
    assert!(!m.set_route(None), "None over None is no change");
    assert!(m.set_route(Some(fake_route(&a))));
    assert!(!m.set_route(Some(fake_route(&a))), "same route: nothing stored");
    assert!(routes_to(m, &a));
    assert!(m.set_route(None));
    assert!(m.route().is_none());
    // A flapping link for weeks: still two snapshots, no copy of a card kept anywhere else.
    for i in 0..10_000 {
        m.set_route(Some(fake_route(if i % 2 == 0 { &a } else { &b })));
    }
    assert!(routes_to(m, &b));
    assert_eq!((Arc::strong_count(&a), Arc::strong_count(&b)), (2, 2), "the test's own + one slot each");
    // The current slot busy on another CPU: the reader takes the one before it and never waits.
    {
        let _busy = m.routes[m.route_cur.load(Ordering::Acquire) & 1].lock();
        assert!(routes_to(m, &a));
    }
    assert!(routes_to(m, &b));
}

#[test]
fn a_resume_after_a_stop_starts_anew_without_repeats_or_loss() {
    let card = FakeCard::new();
    let m = mirror(64 << 10);
    let sock = FakeSock::default();
    assert!(!m.resume(), "never stopped");
    assert_eq!(m.stop(), super::StopReport::default(), "never live");
    m.set_route(Some(fake_route(&card)));
    start(m, &sock);
    m.write_line(Level::Info, "one");
    m.flush(&sock, 10).unwrap();
    m.write_line(Level::Info, "two");
    let r = m.stop();
    assert!(r.was_live && r.ended && !r.dropped && r.frames >= 2, "{r:?}");
    assert_eq!(m.take_stop_report(), Some(r), "kept for the thread to log");
    assert_eq!(m.take_stop_report(), None, "once");
    let mut l = Listener::new();
    sock.sent.lock().drain(..).for_each(|d| l.take(&d));
    card.datagrams(SRC).unwrap().iter().for_each(|d| l.take(d));
    card.frames.lock().clear();
    // Stopped: the ring keeps collecting, nothing goes on the wire.
    m.write_line(Level::Info, "while stopped");
    assert_eq!(m.emergency("panic while stopped").frames, 0);
    assert!(m.flush(&sock, 10).unwrap() == (0, 0) && sock.sent.lock().is_empty());
    assert!(m.resume());
    assert!(!m.resume(), "one resume per stop");
    assert!(m.take_resume() && !m.take_resume());
    // The thread's new start: from where the stop ended, a new id, the screen stream from 0 again.
    let mut b = [0u8; DGRAM_MAX];
    let (n, pos, skipped) = m.begin(0xCD, 2, [192, 168, 100, 20], &mut b).unwrap();
    assert!(pos > 0 && skipped == 0, "pos={pos} skipped={skipped}");
    sock.send_to(&b[..n], SockAddr { ip: net::Ipv4(T.ip), port: T.port }).unwrap();
    m.set_live(true);
    m.flush(&sock, 10).unwrap();
    sock.sent.lock().iter().for_each(|d| l.take(d));
    {
        let _held = m.st.lock();
        assert!(m.emergency("held again").screen);
    }
    assert!(m.emergency("after resume").frames > 0);
    card.datagrams(SRC).unwrap().iter().for_each(|d| l.take(d));
    let starts = l.out.iter().filter(|o| o.contains(" start 2: mirror from")).count();
    assert_eq!(starts, 2, "{:?}", l.out);
    let rest: Vec<&str> = l.out.iter().filter(|o| !o.contains(" start 2: ")).map(|o| o.as_str()).collect();
    assert_eq!(
        rest,
        ["log| one", "log| two", "==== mirror ended ====", "log| while stopped", "log| panic while stopped", "scr| held again", "log| after resume"]
    );
    assert!(!l.out.iter().any(|o| o.contains("lost")), "{:?}", l.out);
    // A stop without a route snapshot says so.
    m.set_route(None);
    let r = m.stop();
    assert!(r.was_live && r.no_route && !r.ended && !r.timed_out, "{r:?}");
    assert_eq!(m.take_stop_report(), Some(r));
}
