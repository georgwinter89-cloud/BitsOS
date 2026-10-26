//! One connection: name lookup, TCP connect (every address in turn, sharing one connect time limit), the TLS
//! handshake, the time limits (`--timeout` per operation, `--max-time` for the whole run) and the mapping of
//! I/O errors to exit codes.

use crate::url::Url;
use crate::{Code, Fail, Res, http, tls};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

/// The connection with its time limits: `idle` for each socket read or write, `end` (from --max-time) for the
/// whole run. A drip-feeding server is stopped by `end`, which `idle` alone cannot do.
pub struct Conn {
    s: Stream,
    idle: Duration,
    end: Option<Instant>,
}

/// The error a socket operation gets once `--max-time` has passed (kind `TimedOut`, told apart in `classify`).
#[derive(Debug)]
struct MaxTime;

impl std::fmt::Display for MaxTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("--max-time reached")
    }
}

impl std::error::Error for MaxTime {}

/// Sets the socket timeouts to what is left (at most `idle`); an error once `end` has passed.
fn arm(sock: &TcpStream, idle: Duration, end: Option<Instant>) -> io::Result<()> {
    let Some(end) = end else { return Ok(()) };
    let left = end.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(ErrorKind::TimedOut, MaxTime));
    }
    let t = Some(idle.min(left));
    sock.set_read_timeout(t).and_then(|_| sock.set_write_timeout(t))
}

/// A socket timeout that hit the `--max-time` end (not the idle limit) becomes the `MaxTime` error.
fn late(e: io::Error, end: Option<Instant>) -> io::Error {
    let timed_out = matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock);
    if timed_out && end.is_some_and(|end| Instant::now() >= end) { io::Error::new(ErrorKind::TimedOut, MaxTime) } else { e }
}

impl Conn {
    fn sock(&self) -> &TcpStream {
        match &self.s {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &s.sock,
        }
    }

    fn io<T>(&mut self, op: impl FnOnce(&mut Stream) -> io::Result<T>) -> io::Result<T> {
        arm(self.sock(), self.idle, self.end)?;
        op(&mut self.s).map_err(|e| late(e, self.end))
    }
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.io(|s| match s {
            Stream::Plain(s) => s.read(b),
            Stream::Tls(s) => s.read(b),
        })
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.io(|s| match s {
            Stream::Plain(s) => s.write(b),
            Stream::Tls(s) => s.write(b),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io(|s| match s {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Phase {
    Handshake,
    Request,
    Head,
    Body,
}

/// `timeout` bounds the whole connect phase: every address gets an even share of what is left (at least 1 s
/// or the rest), so a host with several unreachable addresses still gives up after about `timeout`.
pub fn open(url: &Url, timeout: Duration, end: Option<Instant>, tls_cfg: Option<&Arc<ClientConfig>>) -> Res<Conn> {
    let addrs: Vec<_> = (url.host.as_str(), url.port).to_socket_addrs().map_err(|e| Fail(Code::Dns, format!("cannot resolve {}: {e}", url.host)))?.collect();
    if addrs.is_empty() {
        return Err(Fail(Code::Dns, format!("{} has no address", url.host)));
    }
    let start = Instant::now();
    let connect_end = start.checked_add(timeout).map_or(end, |c| Some(end.map_or(c, |e| e.min(c))));
    let mut last = None;
    let mut tcp = None;
    for (i, a) in addrs.iter().enumerate() {
        let left = connect_end.map_or(timeout, |c| c.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            last = Some((*a, io::Error::from(ErrorKind::TimedOut)));
            break;
        }
        let share = (left / u32::try_from(addrs.len() - i).unwrap_or(u32::MAX)).max(left.min(Duration::from_secs(1)));
        match TcpStream::connect_timeout(a, share) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = Some((*a, e)),
        }
    }
    let tcp = match (tcp, last) {
        (Some(s), _) => s,
        (None, Some((a, e))) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
            let why = if end.is_some_and(|e| Instant::now() >= e) { "--max-time reached".to_string() } else { format!("no answer within {} s", timeout.as_secs()) };
            return Err(Fail(Code::Timeout, format!("connect to {a}: {why}")));
        }
        (None, Some((a, e))) => return Err(Fail(Code::Connect, format!("connect to {a}: {e}"))),
        (None, None) => return Err(Fail(Code::Connect, format!("{}: nothing to connect to", url.host))),
    };
    let set = tcp.set_read_timeout(Some(timeout)).and_then(|_| tcp.set_write_timeout(Some(timeout)));
    set.map_err(|e| Fail(Code::Connect, format!("socket timeouts: {e}")))?;
    let _ = tcp.set_nodelay(true);
    let Some(cfg) = tls_cfg.filter(|_| url.https) else {
        return Ok(Conn { s: Stream::Plain(tcp), idle: timeout, end });
    };
    let name = tls::server_name(&url.host).map_err(|m| Fail(Code::Usage, m))?;
    let conn = ClientConnection::new(cfg.clone(), name).map_err(|e| Fail(Code::Tls, format!("TLS setup: {e}")))?;
    let mut s = StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        let step = arm(&s.sock, timeout, end).and_then(|_| s.conn.complete_io(&mut s.sock).map_err(|e| late(e, end)));
        step.map_err(|e| classify(e, Phase::Handshake, timeout))?;
    }
    Ok(Conn { s: Stream::Tls(Box::new(s)), idle: timeout, end })
}

pub fn http_fail(e: http::Error, timeout: Duration) -> Fail {
    match e {
        http::Error::Io(e) => classify(e, Phase::Head, timeout),
        http::Error::Bad(m) => Fail(Code::Protocol, m),
    }
}

pub fn classify(e: io::Error, phase: Phase, timeout: Duration) -> Fail {
    if let Some(te) = e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
        if tls::is_time_error(te) {
            return Fail(Code::Clock, format!("certificate not valid at this time ({te}); check the system clock or use --verify-time"));
        }
        return Fail(Code::Tls, format!("TLS: {te}"));
    }
    if e.get_ref().is_some_and(|i| i.is::<MaxTime>()) {
        return Fail(Code::Timeout, "the whole run took longer than --max-time".into());
    }
    if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
        return Fail(Code::Timeout, format!("nothing received for {} s", timeout.as_secs()));
    }
    match phase {
        Phase::Handshake => Fail(Code::Tls, format!("connection lost during the TLS handshake: {e}")),
        Phase::Request => Fail(Code::Connect, format!("sending the request: {e}")),
        Phase::Head => Fail(Code::Protocol, format!("reading the response: {e}")),
        Phase::Body if e.kind() == ErrorKind::UnexpectedEof => Fail(Code::Protocol, format!("truncated download: {e}")),
        Phase::Body => Fail(Code::Protocol, format!("receiving the body: {e}")),
    }
}
