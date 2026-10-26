//! In-kernel tests of the stack (NW-3). Under `xtask ktest` (QEMU user network on a virtio-net card: DHCP gives
//! 10.0.2.15, the gateway 10.0.2.2 answers ping) `nw.stack.dhcp`, `ping_gateway` and `loopback_*` run for real;
//! the tests that need a host-side partner (an HTTP server on the lane port 24510, reached as 10.0.2.2:24510;
//! a host client through hostfwd 24521 -> 24520; the fake LAN of `host/tests/nw/40-fake-lan.sh`) skip at once
//! when the partner is missing, and the scenario scripts require them to pass.

mod hardening;
mod lifeline;
mod netlink;
mod parallel;
mod sha256;

use crate::http;
use crate::stack::{self, Stack};
use crate::{Ipv4, KSocket, NetError, SockAddr, TcpStream};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};

const SEC: u64 = 1_000_000_000;
/// QEMU user network (slirp): our address, the gateway (= the host's 127.0.0.1 for TCP), the DNS server.
const SLIRP_GUEST: Ipv4 = Ipv4([10, 0, 2, 15]);
const SLIRP_GW: Ipv4 = Ipv4([10, 0, 2, 2]);
const SLIRP_DNS: Ipv4 = Ipv4([10, 0, 2, 3]);
/// The scenario's host HTTP server (lane ports 24500-24999).
const HOST_HTTP: SockAddr = SockAddr { ip: SLIRP_GW, port: 24510 };
/// Guest port the host reaches through `--hostfwd 24521:24520`.
const INBOUND_PORT: u16 = 24520;
/// Loopback tests' ports.
const LO_PORT: u16 = 24990;
/// The fake LAN card's MAC (set by 40-fake-lan.sh).
const LAN_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x4e, 0x57, 0x30];
const LAN_ROUTER: Ipv4 = Ipv4([192, 168, 77, 1]);

/// Free RAM (the G0 heap never reuses memory, so this shows what a quiet stack costs).
fn free_bytes() -> u64 {
    mm_core::frames().map(|f| f.stats().free_bytes).unwrap_or(0)
}

fn st() -> Result<&'static Stack, String> {
    stack::start().map_err(String::from)
}

fn e(x: impl core::fmt::Debug) -> String {
    format!("{x:?}")
}

/// A configured card, or skip/fail: no card at all is a skip, a card without lease after 30 s a failure.
fn configured(s: &'static Stack) -> Result<Option<stack::IfSummary>, String> {
    if crate::netdev::devices().is_empty() {
        return Ok(None);
    }
    s.wait_configured(30 * SEC).ok_or("no DHCP lease on any card within 30 s")?;
    Ok(s.interfaces().into_iter().find(|i| i.default_route).or_else(|| s.interfaces().into_iter().find(|i| i.info.addr.is_some())))
}

fn dhcp() -> TestResult {
    let s = st()?;
    let Some(i) = configured(s)? else { return ktest::skip("no network card") };
    let (ip, prefix) = i.info.addr.ok_or("no address")?;
    klog::event!(Info, "net", "ktest.dhcp", ifname = i.name.as_str(), ip = stack::ip_str(ip), prefix = prefix,
        router = i.info.router.map(stack::ip_str).unwrap_or_default());
    if crate::route::in_subnet(ip, SLIRP_GW, 24) {
        kassert!(ip == SLIRP_GUEST, "QEMU user net gives 10.0.2.15, got {}", stack::ip_str(ip));
        kassert!(i.info.router == Some(SLIRP_GW), "router {:?}", i.info.router);
        kassert!(i.info.dns.contains(&SLIRP_DNS), "dns {:?}", i.info.dns);
        kassert!(i.info.lease_expires_ns.is_some(), "slirp leases have a time");
    } else {
        kassert!(ip.is_private(), "a home network address, got {}", stack::ip_str(ip));
    }
    kassert!(i.default_route, "the leased card owns the default route");
    Ok(())
}
ktest!("nw.stack.dhcp", Safe, dhcp);

/// A quiet stack (and the drivers' pollers under it) costs no memory: the G0 heap never reuses freed memory, so
/// an allocation per poll round ran QEMU's 2 GiB out within half a minute before this was fixed. NW-4: with
/// every card interrupt-driven the poll thread still looks every `IRQ_SAFETY_NS` (and wakes on interrupts);
/// with a polled card it looks every 500 µs or more often.
fn quiet_memory() -> TestResult {
    let s = st()?;
    let _ = configured(s)?;
    sched::sleep_ns(SEC / 2);
    let (r0, f0, irq0) = (stack::POLL_ROUNDS.get(), free_bytes(), crate::netdev::IRQ_WAKES.get());
    sched::sleep_ns(2 * SEC);
    let (rounds, used) = (stack::POLL_ROUNDS.get() - r0, f0.saturating_sub(free_bytes()));
    let irq = crate::netdev::all_irq_driven();
    klog::event!(Info, "net", "ktest.quiet", rounds = rounds, used_kb = used / 1024, irq_mode = irq,
        irq_wakes = crate::netdev::IRQ_WAKES.get() - irq0);
    let least = if irq { 2 * SEC / stack::IRQ_SAFETY_NS / 2 } else { 100 };
    kassert!(rounds > least, "the poll thread is not polling ({rounds} rounds in 2 s, irq mode {irq})");
    kassert!(used <= 4 << 20, "{} KiB used in 2 quiet seconds", used / 1024);
    Ok(())
}
ktest!("nw.stack.quiet_memory", Safe, quiet_memory);

fn ping_gateway() -> TestResult {
    let s = st()?;
    let Some(i) = configured(s)? else { return ktest::skip("no network card") };
    let gw = i.info.router.ok_or("no router in the lease")?;
    let mut rtts = Vec::new();
    for _ in 0..3 {
        rtts.push(s.ping(gw, 5 * SEC).map_err(|x| format!("ping {}: {x:?}", stack::ip_str(gw)))?);
    }
    klog::event!(Info, "net", "ktest.ping", to = stack::ip_str(gw), us = format!("{}/{}/{}", rtts[0] / 1000, rtts[1] / 1000, rtts[2] / 1000));
    if gw == SLIRP_GW {
        // QEMU only (nothing leaves the machine): an address nobody has must time out, not answer.
        kassert!(s.ping(Ipv4([192, 0, 2, 1]), SEC / 2) == Err(NetError::TimedOut), "TEST-NET-1 must not answer");
    }
    Ok(())
}
ktest!("nw.stack.ping_gateway", Safe, ping_gateway);

// ---------------------------------------------------------------------------------------------------------
// Loopback life cycle (MC-1's change requests), through the stack's 127.0.0.1 part.

fn lo_at(port: u16) -> SockAddr {
    SockAddr { ip: Ipv4::LOCALHOST, port }
}

fn loopback_listener_drop() -> TestResult {
    let s = st()?;
    let l = s.tcp_listen(SockAddr { ip: Ipv4::ANY, port: LO_PORT }, 3).map_err(e)?;
    let waiting = s.tcp_connect(lo_at(LO_PORT), Some(SEC)).map_err(e)?;
    drop(l);
    let mut b = [0u8; 4];
    kassert!(waiting.read(&mut b, Some(SEC)) == Err(NetError::ConnectionReset), "a connection nobody accepted is reset");
    kassert!(s.tcp_connect(lo_at(LO_PORT), Some(SEC)).err() == Some(NetError::ConnectionRefused), "port released");
    let l2 = s.tcp_listen(SockAddr { ip: Ipv4::ANY, port: LO_PORT }, 3).map_err(|x| format!("listen again: {x:?}"))?;
    let c = s.tcp_connect(lo_at(LO_PORT), Some(SEC)).map_err(e)?;
    let a = l2.accept(Some(SEC)).map_err(e)?;
    drop(c);
    kassert!(a.read(&mut b, Some(SEC)) == Ok(0), "a dropped stream is EOF for the peer");
    kassert!(a.write_all(b"x", Some(SEC)) == Err(NetError::ConnectionReset), "and writes to it fail");
    Ok(())
}
ktest!("nw.stack.loopback_listener_drop", Safe, loopback_listener_drop);

fn loopback_shutdown_wakes() -> TestResult {
    let s = st()?;
    let l = s.tcp_listen(SockAddr { ip: Ipv4::LOCALHOST, port: LO_PORT + 1 }, 1).map_err(e)?;
    let c = s.tcp_connect(lo_at(LO_PORT + 1), Some(SEC)).map_err(e)?;
    let a: Arc<dyn TcpStream> = Arc::from(l.accept(Some(SEC)).map_err(e)?);
    let result = Arc::new(ksync::SpinLock::new(None));
    let (r, out) = (a.clone(), result.clone());
    let h = sched::spawn("ktest-reader", sched::Class::Normal, move || {
        let mut b = [0u8; 8];
        *out.lock() = Some(r.read(&mut b, Some(10 * SEC)));
    })
    .map_err(e)?;
    sched::sleep_ns(SEC / 10);
    kassert!(!h.is_finished(), "the reader should be blocked");
    a.shutdown();
    kassert!(h.join_timeout(2 * SEC), "shutdown did not wake the blocked reader");
    sched::reap();
    let got = *result.lock();
    kassert!(got == Some(Err(NetError::ConnectionReset)), "reader got {got:?}");
    let mut b = [0u8; 4];
    kassert!(c.read(&mut b, Some(SEC)) == Err(NetError::ConnectionReset), "the peer sees the reset");
    Ok(())
}
ktest!("nw.stack.loopback_shutdown", Safe, loopback_shutdown_wakes);

/// A reader that does not read holds a 127.0.0.1 writer at `PIPE_LIMIT` (checker NW-3: the pipe was unbounded):
/// the write times out with the caller's timeout, and goes on once the reader reads.
fn loopback_full_pipe() -> TestResult {
    let s = st()?;
    let l = s.tcp_listen(SockAddr { ip: Ipv4::LOCALHOST, port: LO_PORT + 2 }, 1).map_err(e)?;
    let c = s.tcp_connect(lo_at(LO_PORT + 2), Some(SEC)).map_err(e)?;
    let a = l.accept(Some(SEC)).map_err(e)?;
    let big = alloc::vec![7u8; crate::loopback::PIPE_LIMIT + 1000];
    let t0 = time::now_ns();
    kassert!(c.write_all(&big, Some(SEC / 10)) == Err(NetError::TimedOut), "a full pipe must time out");
    kassert!(time::now_ns() - t0 < 5 * SEC, "the timeout was not kept");
    let mut b = alloc::vec![0u8; 64 * 1024];
    let mut got = 0;
    while got < 64 * 1024 {
        got += a.read(&mut b, Some(SEC)).map_err(e)?;
    }
    kassert!(c.write_all(&big[..1000], Some(SEC)).is_ok(), "room again after reading");
    Ok(())
}
ktest!("nw.stack.loopback_full_pipe", Safe, loopback_full_pipe);

// ---------------------------------------------------------------------------------------------------------
// With the scenario's host partners (QEMU only: they talk to 10.0.2.2).

/// A connection to the host server, or None when nobody listens there (no scenario: skip). Only under QEMU's
/// user network (the default router is 10.0.2.2): on a real network nothing is sent to 10.0.2.2.
fn host(s: &'static Stack) -> Result<Option<alloc::boxed::Box<dyn TcpStream>>, String> {
    // Only the scenario that started the server says so (CONFIG net_test_host): a gate in another worktree
    // would otherwise reach a lane's server on the shared host port and fail where it should skip.
    if crate::config("net_test_host").is_none() {
        return Ok(None);
    }
    let Some(i) = configured(s)? else { return Ok(None) };
    if i.info.router != Some(SLIRP_GW) {
        return Ok(None);
    }
    match s.tcp_connect(HOST_HTTP, Some(5 * SEC)) {
        Ok(c) => Ok(Some(c)),
        Err(NetError::ConnectionRefused) | Err(NetError::Unreachable) => Ok(None),
        Err(x) => Err(format!("connect to 10.0.2.2:24510: {x:?}")),
    }
}

const NO_HOST: &str = "no host server of this run (host/tests/nw/30-stack.sh starts one and sets CONFIG net_test_host)";

/// Where `nw.stack.fetch` downloads from: CONFIG `net_test_url=http://<ip>:<port>` (the Mini-PC check: the
/// Gaming-PC runs `host/tests/nw/stack_server.py`), else the scenario's server under QEMU.
fn fetch_base(s: &'static Stack) -> Result<Option<String>, String> {
    if let Some(u) = crate::config("net_test_url") {
        return Ok(Some(String::from(u.trim().trim_end_matches('/'))));
    }
    Ok(host(s)?.map(|_| String::from("http://10.0.2.2:24510")))
}

fn host_get(path: &str, limit: u64) -> Result<Vec<u8>, String> {
    let url = format!("http://10.0.2.2:24510{path}");
    http::get(st()?, &url, &http::Options::new(limit)).map(|(_, b)| b).map_err(|x| format!("GET {path}: {x:?}"))
}

fn tcp_host() -> TestResult {
    let s = st()?;
    let Some(c) = host(s)? else { return ktest::skip(NO_HOST) };
    c.write_all(b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", Some(5 * SEC)).map_err(e)?;
    let mut got = Vec::new();
    let mut b = [0u8; 512];
    loop {
        match c.read(&mut b, Some(10 * SEC)).map_err(e)? {
            0 => break,
            n => got.extend_from_slice(&b[..n]),
        }
    }
    kassert!(got.starts_with(b"HTTP/1.") && got.ends_with(b"pong\n"), "answer {:?}", String::from_utf8_lossy(&got));
    kassert!(c.local().ip == SLIRP_GUEST && c.peer() == HOST_HTTP);
    Ok(())
}
ktest!("nw.stack.tcp_host", QemuOnly, tcp_host);

/// UDP both ways: the scenario's echo on 10.0.2.2:24511 (stack_server.py) sends each datagram back.
fn udp_host() -> TestResult {
    let s = st()?;
    if host(s)?.is_none() {
        return ktest::skip(NO_HOST);
    }
    let echo = SockAddr { ip: SLIRP_GW, port: HOST_HTTP.port + 1 };
    let u = s.udp_bind(SockAddr { ip: Ipv4::ANY, port: 0 }).map_err(e)?;
    let mut b = [0u8; 64];
    // UDP may lose a datagram (slirp's first one waits for its ARP): three tries.
    for _ in 0..3 {
        u.send_to(b"bitsos udp", echo).map_err(e)?;
        match u.recv_from(&mut b, Some(2 * SEC)) {
            Ok((n, from)) => {
                kassert!(&b[..n] == b"bitsos udp" && from == echo, "got {:?} from {from:?}", &b[..n]);
                return Ok(());
            }
            Err(NetError::TimedOut) => {}
            Err(x) => return Err(format!("recv: {x:?}")),
        }
    }
    Err(String::from("no UDP echo from 10.0.2.2:24511 in three tries"))
}
ktest!("nw.stack.udp_host", QemuOnly, udp_host);

/// `/file` and `/chunked` against the hash the server publishes (`/file.sha256`: "<hex> <size>"), then the
/// caller's size limit and a 404. Timing is reported (`ktest.fetch`), judged only on the Mini-PC.
fn fetch() -> TestResult {
    let s = st()?;
    let Some(base) = fetch_base(s)? else { return ktest::skip("no test server: net_test_url= in CONFIG, or 30-stack.sh under QEMU") };
    let get = |path: &str, limit: u64| {
        http::get(s, &format!("{base}{path}"), &http::Options::new(limit)).map(|(_, b)| b).map_err(|x| format!("GET {path}: {x:?}"))
    };
    let meta = get("/file.sha256", 200)?;
    let meta = String::from_utf8_lossy(&meta).into_owned();
    let mut it = meta.split_whitespace();
    let want_hex = it.next().ok_or("empty /file.sha256")?.to_ascii_lowercase();
    let size: u64 = it.next().and_then(|x| x.parse().ok()).ok_or("no size in /file.sha256")?;
    for path in ["/file", "/chunked"] {
        let mut h = sha256::Sha256::new();
        let got = core::cell::Cell::new(0u64);
        // NW-3b: the head comes first (Content-Length for /file, chunked without a length for /chunked).
        let mut head: Option<(Option<u64>, bool)> = None;
        let t0 = time::now_ns();
        http::get_to_with(
            s,
            &format!("{base}{path}"),
            &http::Options::new(size),
            &mut |hd| {
                head = Some((hd.content_length, got.get() == 0));
                Ok(())
            },
            &mut |d| {
                h.update(d);
                got.set(got.get() + d.len() as u64);
                Ok(())
            },
        )
        .map_err(|x| format!("GET {path}: {x:?}"))?;
        let n = got.get();
        let (len, first) = head.ok_or("head callback not called")?;
        kassert!(first, "{path}: head after body bytes");
        kassert!(len == if path == "/file" { Some(size) } else { None }, "{path}: content-length {len:?}");
        let us = (time::now_ns() - t0) / 1000;
        let got = sha256::hex(&h.finish());
        klog::event!(Info, "net", "ktest.fetch", path = path, bytes = n, ms = us / 1000, mb_per_s = n / us.max(1));
        kassert!(n == size, "{path}: {n} bytes, want {size}");
        kassert!(got == want_hex, "{path}: sha256 {got}, want {want_hex}");
    }
    let small = http::get(s, &format!("{base}/file"), &http::Options::new(size - 1));
    kassert!(small.as_ref().err() == Some(&http::HttpError::TooLarge { limit: size - 1 }), "limit: {:?}", small.err());
    let missing = http::get(s, &format!("{base}/nothing"), &http::Options::new(10));
    kassert!(missing.as_ref().err() == Some(&http::HttpError::Status(404)), "404: {:?}", missing.err());
    Ok(())
}
ktest!("nw.stack.fetch", Safe, fetch);

/// The host connects in: through hostfwd 127.0.0.1:24521 -> guest 24520 (how MCP is reached in QEMU).
fn inbound() -> TestResult {
    let s = st()?;
    if host(s)?.is_none() {
        return ktest::skip(NO_HOST);
    }
    let l = s.tcp_listen(SockAddr { ip: Ipv4::ANY, port: INBOUND_PORT }, 2).map_err(e)?;
    // Ask the host server to connect to us now.
    let r = host_get("/connect-in", 100)?;
    kassert!(r == b"ok\n", "host answer {:?}", String::from_utf8_lossy(&r));
    let c = l.accept(Some(30 * SEC)).map_err(|x| format!("no connection from the host within 30 s: {x:?}"))?;
    kassert!(c.peer().ip.is_private(), "peer {:?}", c.peer());
    let mut got = Vec::new();
    let mut b = [0u8; 256];
    while !got.ends_with(b"\n") {
        let n = c.read(&mut b, Some(10 * SEC)).map_err(e)?;
        kassert!(n > 0, "EOF before the line");
        got.extend_from_slice(&b[..n]);
    }
    kassert!(got == b"hello from the host\n", "got {:?}", String::from_utf8_lossy(&got));
    c.write_all(b"hello from BitsOS\n", Some(5 * SEC)).map_err(e)?;
    c.close();
    kassert!(c.read(&mut b, Some(10 * SEC)) == Ok(0), "host closes after the answer");
    Ok(())
}
ktest!("nw.stack.inbound", QemuOnly, inbound);

// ---------------------------------------------------------------------------------------------------------
// The fake LAN (40-fake-lan.sh): second card on `-netdev dgram`, a Python router at 192.168.77.1.

fn lan_card(s: &'static Stack) -> Option<stack::IfSummary> {
    s.interfaces().into_iter().find(|i| i.mac == LAN_MAC)
}

fn wait_for(limit_ns: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + limit_ns;
    while time::now_ns() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(SEC / 20);
    }
    f()
}

fn lan_addr(s: &'static Stack) -> Option<Ipv4> {
    lan_card(s).and_then(|i| i.info.addr).map(|(a, _)| a)
}

/// Repeated DISCOVER, lease, renew, rebind, NAK and a new address, with two cards and both routes.
fn lan_dhcp() -> TestResult {
    let s = st()?;
    if lan_card(s).is_none() {
        return ktest::skip("no fake-LAN card (host/tests/nw/40-fake-lan.sh)");
    }
    kassert!(wait_for(30 * SEC, || lan_addr(s) == Some(Ipv4([192, 168, 77, 20]))), "no lease .20 on the fake LAN: {:?}", lan_card(s));
    let lan = lan_card(s).ok_or("card gone")?;
    kassert!(lan.info.router == Some(LAN_ROUTER), "router {:?}", lan.info.router);
    kassert!(!lan.default_route, "the user-net card leased first and keeps the default route");
    // Connected route over the LAN card, default route over the other one.
    s.ping(LAN_ROUTER, 5 * SEC).map_err(|x| format!("ping the fake router: {x:?}"))?;
    s.ping(SLIRP_GW, 5 * SEC).map_err(|x| format!("ping the user-net gateway: {x:?}"))?;
    klog::event!(Info, "net", "ktest.lan_routes_ok");
    // The router renews once, ignores the next renew (rebind at T2), then NAKs: a new address .21 follows.
    kassert!(wait_for(60 * SEC, || lan_addr(s) == Some(Ipv4([192, 168, 77, 21]))), "no new lease .21 after the NAK: {:?}", lan_card(s));
    s.ping(LAN_ROUTER, 5 * SEC).map_err(|x| format!("ping after the new lease: {x:?}"))?;
    Ok(())
}
ktest!("nw.lan.dhcp", QemuOnly, lan_dhcp);

/// Cable pulled and plugged back (QMP set_link): address kept while down, DHCP asked again when back.
fn lan_link() -> TestResult {
    let s = st()?;
    let Some(lan) = lan_card(s) else { return ktest::skip("no fake-LAN card (host/tests/nw/40-fake-lan.sh)") };
    let addr = lan.info.addr.ok_or("no address before the link test")?;
    let seq = klog::next_seq();
    klog::event!(Info, "net", "ktest.lan_link_wait"; "pull the cable now (QMP set_link off)");
    kassert!(wait_for(60 * SEC, || lan_card(s).is_some_and(|i| !i.info.link_up)), "link down not seen");
    kassert!(lan_addr(s) == Some(addr.0), "address kept while the link is down");
    klog::event!(Info, "net", "ktest.lan_link_down_seen");
    kassert!(wait_for(60 * SEC, || lan_card(s).is_some_and(|i| i.info.link_up)), "link up not seen");
    let asked = || klog::read_since(seq, 10_000).iter().any(|r| r.name == "dhcp.restart" && r.text.contains(lan.name.as_str()));
    let answered = || {
        klog::read_since(seq, 10_000).iter().any(|r| (r.name == "dhcp.renewed" || r.name == "dhcp.lease") && r.text.contains(lan.name.as_str()))
    };
    kassert!(wait_for(10 * SEC, asked), "no dhcp.restart after link up");
    kassert!(wait_for(30 * SEC, answered), "no answer to the DHCP restart");
    kassert!(lan_addr(s) == Some(addr.0), "same network, same address");
    s.ping(LAN_ROUTER, 5 * SEC).map_err(|x| format!("ping after the link came back: {x:?}"))?;
    Ok(())
}
ktest!("nw.lan.link", QemuOnly, lan_link);
