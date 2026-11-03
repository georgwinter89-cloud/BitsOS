//! Connections and requests, the kernel server's rules (crates/mcp server.rs, conn.rs) on std sockets: private
//! peers only; at most `conns` connections, a knocking peer makes the idlest give way (idle ones first, a new
//! silent one or one whose head is still coming after `first`), else it is closed after `full`; keep-alive and
//! pipelining; 60 s between requests, 10 s for a request from its first byte (408), 10 s + 1 s per 64 KB to take
//! an answer; refusals answered without a body and closed. Each request leases one gate session for its
//! commands: `auth` for each bearer token, `list`, the call, `answered` once the answer went out.
//!
//! MC-8b: once a client is paired (`tls`), every connection is mutual TLS first: a first byte that opens no TLS
//! record (plain HTTP) or a handshake that fails (a key that is not paired) closes it without a word; the token
//! is still checked by the kernel inside TLS. Before that, one `bitsos/pair` over the plain path pairs the client.
//!
//! MC-10: each TLS connection knows its client's key (the SHA-256 of the certificate it showed), which goes with
//! every list and call, so the kernel offers a client with full access every tool. Under `mcp=service` (strict)
//! a leaked token alone opens nothing: plain HTTP carries only the pairing, and the pairing waits for Georg's
//! yes at the Mini-PC's keyboard (the installer's `pair_client` question) before anything is written.

use crate::link::Pool;
use crate::tls::Tls;
use crate::tools::{self, Catalog, ServiceHost};
use mcp_wire::{Json, b64, http, rpc};
use rustls::ServerConnection;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MS: u64 = 1_000_000;
/// Reads wait at most this long at a time, so eviction and stop are seen promptly.
const SLICE: Duration = Duration::from_millis(100);
const READ_CHUNK: usize = 16 * 1024;
/// The JSON-RPC method of the one-time pairing (host/mcp/pair.mjs).
pub const PAIR_METHOD: &str = "bitsos/pair";
/// The first byte of a TLS record that carries a handshake (RFC 8446 5.1, ContentType handshake).
const TLS_HANDSHAKE: u8 = 0x16;
/// How long a refused connection is read away before it is closed (`Conn::linger`).
const LINGER: Duration = Duration::from_millis(200);

/// The kernel server's limits (`ServerConfig::new`).
#[derive(Clone, Debug)]
pub struct Limits {
    pub conns: usize,
    pub idle_ns: u64,
    pub slow_ns: u64,
    pub first_ns: u64,
    pub full_ns: u64,
}

impl Limits {
    pub fn new(conns: usize) -> Limits {
        Limits { conns: conns.max(3), idle_ns: 60_000 * MS, slow_ns: 10_000 * MS, first_ns: 1_000 * MS, full_ns: 2_000 * MS }
    }
}

const NEW: u8 = 0;
const IDLE: u8 = 1;
const HEAD: u8 = 2;
const BUSY: u8 = 3;
const EVICTED: u8 = 4;

struct Slot {
    peer: SocketAddr,
    accepted_ns: u64,
    last_ns: AtomicU64,
    state: AtomicU8,
}

impl Slot {
    fn enter(&self, to: u8, now: Option<u64>) -> bool {
        let mut s = self.state.load(Ordering::Acquire);
        loop {
            if s == EVICTED {
                return false;
            }
            if s == to {
                return true;
            }
            if let Some(n) = now {
                self.last_ns.store(n, Ordering::Release);
            }
            match self.state.compare_exchange(s, to, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => s = now,
            }
        }
    }
    fn evicted(&self) -> bool {
        self.state.load(Ordering::Acquire) == EVICTED
    }
}

/// The running service.
pub struct Service {
    pool: Pool,
    limits: Limits,
    slots: Mutex<Vec<Arc<Slot>>>,
    stop: AtomicBool,
    threads: AtomicUsize,
    local: Mutex<Option<SocketAddr>>,
    tls: Option<Arc<Tls>>,
    refusals: AtomicU64,
    /// MC-10, `mcp=service`: plain HTTP only for the pairing, and the pairing asked at the keyboard.
    strict: AtomicBool,
}

impl Service {
    /// Plain HTTP with the token only (MC-8a; also what a start without TLS serves).
    pub fn new(pool: Pool, limits: Limits) -> Arc<Service> {
        Service::with_tls(pool, limits, None)
    }
    /// With `tls`: plain until the one-time pairing, TLS only from then (and from the start once one is paired).
    pub fn with_tls(pool: Pool, limits: Limits, tls: Option<Arc<Tls>>) -> Arc<Service> {
        Arc::new(Service {
            pool,
            limits,
            slots: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            threads: AtomicUsize::new(0),
            local: Mutex::new(None),
            tls,
            refusals: AtomicU64::new(0),
            strict: AtomicBool::new(false),
        })
    }
    /// MC-10: the strict rules of `mcp=service`.
    pub fn set_strict(&self, on: bool) {
        self.strict.store(on, Ordering::Release);
    }
    fn strict(&self) -> bool {
        self.strict.load(Ordering::Acquire)
    }
    /// True while connections must be TLS (a client is paired).
    pub fn tls_only(&self) -> bool {
        self.tls.as_ref().is_some_and(|t| t.on())
    }
    /// A silent refusal (not TLS, a failed handshake) into the log: the first ones, then every power of two, so a
    /// knocking host cannot fill the log.
    fn refused(&self, msg: &str) {
        let n = self.refusals.fetch_add(1, Ordering::AcqRel) + 1;
        if n <= 8 || n.is_power_of_two() {
            crate::log::info(&format!("{msg} ({n} refused so far)"));
        }
    }
    /// Connections open now (evicted ones that are still closing excluded).
    pub fn connections(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|s| !s.evicted()).count()
    }
    /// Ends the accept loop and every connection (within about a slice).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(a) = *self.local.lock().unwrap_or_else(|e| e.into_inner()) {
            // Wakes the blocking accept.
            let _ = TcpStream::connect_timeout(&a, Duration::from_millis(200));
        }
    }
    /// Connection threads still running.
    pub fn threads(&self) -> usize {
        self.threads.load(Ordering::Acquire)
    }
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    fn remove(&self, slot: &Arc<Slot>) {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).retain(|s| !Arc::ptr_eq(s, slot));
    }
}

/// The peers MCP takes: private networks and the loopback (the kernel server's `Ipv4::is_private`).
pub fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            a == 10 || a == 127 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) || (a == 169 && b == 254)
        }
        IpAddr::V6(v) => v.to_ipv4_mapped().is_some_and(|m| private(IpAddr::V4(m))) || v.is_loopback(),
    }
}

/// Serves `listener` until `svc.stop()`.
pub fn run(listener: TcpListener, svc: Arc<Service>) {
    if let Ok(mut a) = listener.local_addr() {
        if a.ip().is_unspecified() {
            a.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        *svc.local.lock().unwrap_or_else(|e| e.into_inner()) = Some(a);
    }
    let mut errors = 0u64;
    while !svc.stopping() {
        match listener.accept() {
            Ok((s, peer)) => {
                errors = 0;
                if svc.stopping() {
                    break;
                }
                admit(&svc, s, peer);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                errors += 1;
                if errors == 1 || errors.is_power_of_two() {
                    crate::log::warn(&format!("accept: {e} ({errors} in a row); retrying in 50 ms"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn idlest(svc: &Service, now: u64) -> Option<(Arc<Slot>, u8)> {
    svc.slots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|s| {
            let st = s.state.load(Ordering::Acquire);
            let since = s.last_ns.load(Ordering::Acquire);
            let rank = match st {
                IDLE => 0,
                NEW if now.saturating_sub(s.accepted_ns) >= svc.limits.first_ns => 0,
                HEAD if now.saturating_sub(since) >= svc.limits.first_ns => 1,
                _ => return None,
            };
            Some(((rank, since), s.clone(), st))
        })
        .min_by_key(|(key, _, _)| *key)
        .map(|(_, s, st)| (s, st))
}

fn admit(svc: &Arc<Service>, s: TcpStream, peer: SocketAddr) {
    if !private(peer.ip()) {
        crate::log::warn(&format!("refused {peer}: not a private network"));
        let _ = s.shutdown(Shutdown::Both);
        return;
    }
    let since = crate::now_ns();
    loop {
        let now = crate::now_ns();
        let open = svc.connections();
        if open < svc.limits.conns {
            break;
        }
        if let Some((v, from)) = idlest(svc, now) {
            if v.state.compare_exchange(from, EVICTED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                crate::log::info(&format!("evicted {} for {peer}", v.peer));
            }
            continue;
        }
        if now.saturating_sub(since) >= svc.limits.full_ns || svc.stopping() {
            crate::log::warn(&format!("full: no connection could give way; closed {peer}"));
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let now = crate::now_ns();
    let slot = Arc::new(Slot { peer, accepted_ns: now, last_ns: AtomicU64::new(now), state: AtomicU8::new(NEW) });
    svc.slots.lock().unwrap_or_else(|e| e.into_inner()).push(slot.clone());
    svc.threads.fetch_add(1, Ordering::AcqRel);
    let (csh, cslot) = (svc.clone(), slot.clone());
    let spawned = std::thread::Builder::new().name("bitsmcp-conn".into()).spawn(move || {
        let s = match csh.tls.as_ref().filter(|t| t.on()).map(|t| t.connection()) {
            None => Stream::Plain(s),
            Some(Ok(tls)) => Stream::Tls { tls: Box::new(tls), sock: s },
            Some(Err(e)) => {
                crate::log::warn(&format!("no TLS connection for {peer}: {e}; closed it"));
                Stream::Closed(s)
            }
        };
        let mut c = Conn { svc: csh, slot: cslot, s, peer: peer.to_string(), client: String::new(), buf: Vec::new(), chunk: vec![0; READ_CHUNK] };
        c.run();
    });
    if spawned.is_err() {
        svc.remove(&slot);
        svc.threads.fetch_sub(1, Ordering::AcqRel);
        crate::log::warn(&format!("no thread for {peer}; closed it"));
    }
}

/// A pairing that passed its checks.
struct Pairing {
    id: Json,
    der: Vec<u8>,
    fp: String,
    name: String,
}

enum Next {
    KeepAlive,
    Close,
}

enum Got {
    Bytes,
    Gone,
    TimedOut,
}

/// A connection's bytes: plain TCP, or TLS over it once a client is paired.
enum Stream {
    Plain(TcpStream),
    Tls { tls: Box<ServerConnection>, sock: TcpStream },
    /// Nothing is read or written (no TLS state could be made).
    Closed(TcpStream),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) | Stream::Closed(s) | Stream::Tls { sock: s, .. } => s,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls { tls, sock } => rustls::Stream::new(tls.as_mut(), sock).read(buf),
            Stream::Closed(_) => Ok(0),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls { tls, sock } => rustls::Stream::new(tls.as_mut(), sock).write(buf),
            Stream::Closed(_) => Err(ErrorKind::NotConnected.into()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls { tls, sock } => rustls::Stream::new(tls.as_mut(), sock).flush(),
            Stream::Closed(_) => Ok(()),
        }
    }
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted)
}

/// A failed handshake in plain words (the peer is not told any of it).
fn handshake_failure(e: &std::io::Error) -> String {
    match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
        Some(rustls::Error::InvalidCertificate(_)) => String::from("its key is not paired"),
        Some(rustls::Error::NoCertificatesPresented) => String::from("it showed no key"),
        Some(other) => other.to_string(),
        None => e.to_string(),
    }
}

/// True when `body` is a `bitsos/pair` request (the pairing needs no gate session).
fn is_pairing(body: &[u8]) -> bool {
    std::str::from_utf8(body).ok().and_then(|t| Json::parse(t).ok()).is_some_and(|j| j.get("method").and_then(Json::as_str) == Some(PAIR_METHOD))
}

struct Conn {
    svc: Arc<Service>,
    slot: Arc<Slot>,
    s: Stream,
    peer: String,
    /// MC-10: the key the TLS client proved (SHA-256 of its certificate), "" on a plain connection.
    client: String,
    buf: Vec<u8>,
    chunk: Vec<u8>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Stream::Tls { tls, sock } = &mut self.s
            && !tls.is_handshaking()
        {
            tls.send_close_notify();
            let _ = sock.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = tls.write_tls(sock);
        }
        let _ = self.s.tcp().shutdown(Shutdown::Both);
        self.svc.remove(&self.slot);
        self.svc.threads.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Conn {
    fn read_some(&mut self, left_ns: u64) -> Got {
        let t = Duration::from_nanos(left_ns.clamp(1_000_000, SLICE.as_nanos() as u64));
        let _ = self.s.tcp().set_read_timeout(Some(t));
        match self.s.read(&mut self.chunk) {
            Ok(0) => Got::Gone,
            Ok(n) => {
                self.buf.extend_from_slice(&self.chunk[..n]);
                Got::Bytes
            }
            Err(e) if timed_out(&e) => Got::TimedOut,
            Err(_) => Got::Gone,
        }
    }

    /// Waits up to `deadline` for the first byte without taking it; false: gone, stopped, evicted or too late.
    fn first_byte(&mut self, deadline: u64) -> Option<u8> {
        let mut b = [0u8; 1];
        loop {
            let now = crate::now_ns();
            if now >= deadline || self.svc.stopping() || self.slot.evicted() {
                return None;
            }
            let _ = self.s.tcp().set_read_timeout(Some(Duration::from_nanos((deadline - now).clamp(1_000_000, SLICE.as_nanos() as u64))));
            match self.s.tcp().peek(&mut b) {
                Ok(0) => return None,
                Ok(_) => return Some(b[0]),
                Err(e) if timed_out(&e) => {}
                Err(_) => return None,
            }
        }
    }

    /// The TLS handshake, within a request's 10 s from the accept. False: the connection is closed without a
    /// word (no HTTP status: a host that is not paired learns nothing beyond the failed handshake).
    fn handshake(&mut self) -> bool {
        if !matches!(self.s, Stream::Tls { .. }) {
            return true;
        }
        let deadline = self.slot.accepted_ns.saturating_add(self.svc.limits.slow_ns);
        match self.first_byte(deadline) {
            Some(TLS_HANDSHAKE) => {}
            Some(_) => {
                self.svc.refused(&format!("refused {}: no TLS (plain HTTP is off since a client is paired)", self.peer));
                self.linger();
                return false;
            }
            None => return false,
        }
        let Stream::Tls { tls, sock } = &mut self.s else { return false };
        while tls.is_handshaking() {
            let now = crate::now_ns();
            if now >= deadline || self.svc.stopping() || self.slot.evicted() {
                return false;
            }
            let t = Duration::from_nanos((deadline - now).clamp(1_000_000, SLICE.as_nanos() as u64));
            let _ = sock.set_read_timeout(Some(t));
            let _ = sock.set_write_timeout(Some(Duration::from_nanos(deadline - now)));
            match tls.complete_io(sock) {
                Ok(_) => {}
                Err(e) if timed_out(&e) => {}
                Err(e) => {
                    let why = handshake_failure(&e);
                    self.svc.refused(&format!("refused {}: TLS handshake failed: {why}", self.peer));
                    self.linger();
                    return false;
                }
            }
        }
        // The verifier let only a paired certificate through, and TLS 1.3's CertificateVerify proved its key.
        match tls.peer_certificates().and_then(|c| c.first()) {
            Some(c) => self.client = crate::cert::fingerprint(c.as_ref()),
            None => return false,
        }
        true
    }

    fn run(&mut self) {
        if matches!(self.s, Stream::Closed(_)) || !self.handshake() {
            return;
        }
        let mut idle_since = self.slot.accepted_ns;
        loop {
            // The pairing turned plain HTTP off: plain connections still open end before their next request.
            if matches!(self.s, Stream::Plain(_)) && self.svc.tls_only() {
                return;
            }
            let skip = http::empty_lines(&self.buf);
            self.buf.drain(..skip);
            if self.buf.is_empty() {
                if self.svc.stopping() || self.slot.evicted() {
                    return;
                }
                let idle = crate::now_ns().saturating_sub(idle_since);
                if idle >= self.svc.limits.idle_ns {
                    return;
                }
                match self.read_some(self.svc.limits.idle_ns - idle) {
                    Got::Gone => return,
                    Got::Bytes | Got::TimedOut => continue,
                }
            }
            if !self.slot.enter(HEAD, Some(crate::now_ns())) {
                return;
            }
            match self.request() {
                Next::Close => return,
                Next::KeepAlive => {
                    idle_since = crate::now_ns();
                    if http::empty_lines(&self.buf) == self.buf.len() && !self.slot.enter(IDLE, Some(idle_since)) {
                        return;
                    }
                }
            }
        }
    }

    fn fill(&mut self, deadline: u64, enough: impl Fn(&[u8]) -> bool) -> Got {
        while !enough(&self.buf) {
            let now = crate::now_ns();
            if now >= deadline {
                return Got::TimedOut;
            }
            if self.svc.stopping() || self.slot.evicted() {
                return Got::Gone;
            }
            if let Got::Gone = self.read_some(deadline - now) {
                return Got::Gone;
            }
        }
        Got::Bytes
    }

    fn request(&mut self) -> Next {
        let t0 = crate::now_ns();
        let deadline = t0.saturating_add(self.svc.limits.slow_ns);
        let scanned = std::cell::Cell::new(0usize);
        let head_done = |b: &[u8]| {
            let from = scanned.replace(b.len()).saturating_sub(2);
            http::head_end(b, from).is_some() || b.len() >= http::HEAD_MAX
        };
        match self.fill(deadline, head_done) {
            Got::Gone => return Next::Close,
            Got::TimedOut => return self.refuse(408),
            Got::Bytes => {}
        }
        let end = match http::head_end(&self.buf, 0) {
            Some(e) if e <= http::HEAD_MAX => e,
            _ => return self.refuse(431),
        };
        // A pairing on another connection turned plain HTTP off while this head came in: no answer at all.
        if self.plain_is_off() {
            return Next::Close;
        }
        // The head is complete: from here the connection is busy and no longer gives way to a new one (the waits
        // below are bounded). Evicted while the head came in: say so instead of closing silently.
        if !self.slot.enter(BUSY, Some(crate::now_ns())) {
            return self.refuse(503);
        }
        let svc = self.svc.clone();
        let Some(mut lease) = svc.pool.take(Duration::from_nanos(svc.limits.slow_ns)) else {
            crate::log::warn(&format!("no free gate session for {} within 10 s ({} in use)", self.peer, svc.pool.size()));
            return self.refuse(500);
        };
        let peer = self.peer.clone();
        let head = http::parse_head_by(&self.buf[..end], &mut |tok| lease.command(&mcp_wire::gate::encode_auth(&peer, tok)).is_ok());
        if head.status != 0 {
            if head.status == 401 {
                crate::log::info(&format!("refused {peer}: 401 (no valid token)"));
            }
            return self.refuse(head.status);
        }
        let total = end + head.length;
        if head.expect && self.buf.len() < total && !self.send(b"HTTP/1.1 100 Continue\r\n\r\n") {
            return Next::Close;
        }
        match self.fill(deadline, |b| b.len() >= total) {
            Got::Gone => return Next::Close,
            Got::TimedOut => return self.refuse(408),
            Got::Bytes => {}
        }
        let body: Vec<u8> = self.buf.drain(..total).skip(end).collect();
        if self.plain_is_off() {
            return Next::Close;
        }
        if is_pairing(&body) {
            // The token is judged; under mcp=service Georg says yes at the keyboard first (the lease waits for
            // that, bounded by the kernel); the pairing writes to the stick (fsync, maybe slow), so the gate
            // session is given back before and taken again only to count the answer (bounded like every take).
            let a = match self.pairing_check(&body) {
                Err(reply) => {
                    drop(lease);
                    reply
                }
                Ok(p) => {
                    let asked = if svc.strict() { self.pairing_asked(&mut lease, &p) } else { Ok(()) };
                    drop(lease);
                    match asked {
                        Ok(()) => self.pairing_done(p),
                        Err(reply) => reply,
                    }
                }
            };
            if self.answer(200, a.as_bytes(), true)
                && let Some(mut l) = svc.pool.take(Duration::from_nanos(svc.limits.slow_ns))
            {
                let _ = l.command(&mcp_wire::gate::encode_answered());
            }
            return Next::Close;
        }
        // MC-10, mcp=service: a token over plain HTTP opens nothing but the pairing.
        if svc.strict() && matches!(self.s, Stream::Plain(_)) {
            self.svc.refused(&format!("refused {peer}: under mcp=service plain HTTP carries only the pairing (host/mcp/pair.mjs); 403"));
            return self.refuse(403);
        }
        let Some(catalog) = Catalog::fetch(&mut lease, &self.client) else {
            crate::log::warn("the gate did not answer list");
            return self.refuse(500);
        };
        let client = self.client.clone();
        let a = rpc::handle_with(&body, crate::now_ns(), &catalog, &ServiceHost, &tools::INFO, &mut |t, d, args| tools::run(&mut lease, t, d, args, &peer, &client));
        let sent = self.answer(a.status, a.body.as_bytes(), head.close);
        if sent {
            // RB-6's proof that MCP answered (the kernel counts mcp.answered).
            let _ = lease.command(&mcp_wire::gate::encode_answered());
        }
        if sent && !head.close { Next::KeepAlive } else { Next::Close }
    }

    /// True on a plain connection once a client is paired (plain HTTP is off from then on).
    fn plain_is_off(&self) -> bool {
        matches!(self.s, Stream::Plain(_)) && self.svc.tls_only()
    }

    /// After a refused handshake: no more bytes from us (the TLS alert rustls queued is out), then what the peer
    /// still sends is read away for a moment, so the close is a FIN and not a reset that could swallow the
    /// alert (Linux resets a socket closed with unread data: linux-6.6/net/ipv4/tcp.c:2793, __tcp_close). Bounded: 200 ms.
    fn linger(&mut self) {
        let tcp = self.s.tcp();
        let _ = tcp.shutdown(Shutdown::Write);
        let until = crate::now_ns().saturating_add(LINGER.as_nanos() as u64);
        let mut chunk = [0u8; 2048];
        loop {
            let now = crate::now_ns();
            if now >= until {
                return;
            }
            let _ = tcp.set_read_timeout(Some(Duration::from_nanos((until - now).max(1_000_000))));
            match (&*tcp).read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    }

    /// `bitsos/pair` (one JSON-RPC request, the token already passed): everything the one-time pairing checks
    /// before it may ask or write (TLS this start, a usable certificate, the expected server key). Err: the
    /// JSON-RPC reply that refuses it.
    fn pairing_check(&self, body: &[u8]) -> Result<Pairing, String> {
        let j = std::str::from_utf8(body).ok().and_then(|t| Json::parse(t).ok()).unwrap_or(Json::Null);
        let id = j.get("id").cloned().unwrap_or(Json::Null);
        let params = j.get("params");
        let der = params.and_then(|p| p.get("certificate")).and_then(Json::as_str).and_then(b64::decode);
        // `expect`: the fingerprint the host was told (the console line); a different key pairs nothing.
        let expect = params.and_then(|p| p.get("expect")).and_then(Json::as_str);
        let checked = match (&self.svc.tls, &self.s, der) {
            (_, Stream::Tls { .. }, _) => Err((-32000, crate::tls::Refused::Closed.text())),
            (None, ..) => Err((-32000, String::from("pairing is not available: the service has no TLS this start (see its start line)"))),
            (Some(_), _, None) => Err((-32602, String::from("params.certificate must be the client's DER certificate in base64"))),
            (Some(t), _, Some(_)) if expect.is_some_and(|e| !e.eq_ignore_ascii_case(&t.fingerprint())) => {
                Err((-32000, format!("the service's key is sha256 {}, not the expected {}; not paired", t.fingerprint(), expect.unwrap_or(""))))
            }
            (Some(t), _, Some(d)) => t.usable(&d).map(|()| d).map_err(|r| (-32000, r.text())),
        };
        match checked {
            Ok(der) => Ok(Pairing { id, fp: crate::cert::fingerprint(&der), name: mcp_wire::access::common_name(&der).unwrap_or_default(), der }),
            Err((code, msg)) => Err(self.pairing_refused(id, code, &msg)),
        }
    }

    /// MC-10, mcp=service: Georg's yes at the keyboard for this pairing (the installer's `pair_client`).
    fn pairing_asked(&self, lease: &mut crate::link::Lease<'_>, p: &Pairing) -> Result<(), String> {
        use mcp_wire::gate::{Request, decode_result, encode_install};
        let args = Json::obj(vec![("fingerprint", Json::str(&p.fp)), ("name", Json::str(&p.name))]);
        let r = Request { tool: String::from(mcp_wire::access::PAIR_CLIENT), args, timeout_ms: mcp_wire::installer::DECISION_WAIT_MS, peer: self.peer.clone(), client: String::new() };
        crate::log::info(&format!("pairing {} ({}) from {}: asking at the keyboard (mcp=service)", mcp_wire::access::fp_short(&p.fp), p.name, self.peer));
        let why = match lease.command(&encode_install(&r)).map(|t| decode_result(&t)) {
            Ok(Some(Ok(Ok(_)))) => return Ok(()),
            Ok(Some(Ok(Err(e)))) => format!("{e:?}"),
            Ok(_) => String::from("the kernel's answer was not understood"),
            Err(errno) => format!("nobody can confirm it: no installer runs (errno {errno}); under mcp=service a pairing needs Georg's yes at the keyboard"),
        };
        Err(self.pairing_refused(p.id.clone(), -32000, &format!("not paired: {why}")))
    }

    /// The pairing itself: written to the stick, plain HTTP off from now.
    fn pairing_done(&self, p: Pairing) -> String {
        let Some(t) = &self.svc.tls else { return self.pairing_refused(p.id, -32000, "no TLS") };
        match t.pair(&p.der) {
            Ok(client) => {
                crate::log::info(&format!("paired the client {client} from {} (the one-time pairing; plain HTTP is off from now)", self.peer));
                let mut c = String::new();
                b64::encode_into(t.cert(), &mut c);
                let result = Json::obj(vec![("paired", Json::Str(client)), ("server_certificate", Json::Str(c)), ("server_fingerprint", Json::Str(t.fingerprint()))]);
                Json::obj(vec![("jsonrpc", Json::str("2.0")), ("id", p.id), ("result", result)]).to_string()
            }
            Err(r) => self.pairing_refused(p.id, -32000, &r.text()),
        }
    }

    fn pairing_refused(&self, id: Json, code: i64, msg: &str) -> String {
        crate::log::warn(&format!("pairing from {} refused: {msg}", self.peer));
        let e = Json::obj(vec![("code", Json::Num(code as f64)), ("message", Json::str(msg))]);
        Json::obj(vec![("jsonrpc", Json::str("2.0")), ("id", id), ("error", e)]).to_string()
    }

    /// Answers without a body and closes.
    fn refuse(&mut self, status: u16) -> Next {
        self.answer(status, b"", true);
        Next::Close
    }

    fn answer(&mut self, status: u16, body: &[u8], close: bool) -> bool {
        let head = http::answer_head(status, body.len(), close);
        let mut out = Vec::with_capacity(head.len() + body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(body);
        self.send(&out)
    }

    fn send(&mut self, out: &[u8]) -> bool {
        let limit = self.svc.limits.slow_ns.saturating_add((out.len() as u64 >> 16) * 1_000_000_000);
        let _ = self.s.tcp().set_write_timeout(Some(Duration::from_nanos(limit)));
        match self.s.write_all(out).and_then(|()| self.s.flush()) {
            Ok(()) => true,
            Err(e) => {
                crate::log::info(&format!("answer to {} not sent ({} bytes): {e}", self.peer, out.len()));
                false
            }
        }
    }
}
