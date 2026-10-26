//! A tiny HTTP/1.1 GET client for `fetch` and Reload (spec §3: the kernel keeps only this; programs bring their
//! own HTTP). `http://<IPv4>[:port]/path` only: no DNS and no redirects in the kernel, so the client never
//! connects anywhere the caller did not name (R9). Bodies with Content-Length, chunked, or until close; the
//! caller gives the size limit and checks the hash itself (`get_to` streams into the caller's sink;
//! `get_to_with` shows the caller the response head first, so it can size its memory from Content-Length).
//! Hostile-input bounds (documented, not counts, R1): head 64 KiB, chunk-size and trailer lines 4 KiB,
//! chunk sizes up to 16 hex digits.

use crate::{Ipv4, KSocket, NetError, SockAddr, TcpStream};
use alloc::string::String;
use alloc::vec::Vec;

const HEAD_MAX: usize = 64 * 1024;
const LINE_MAX: usize = 4096;
const SEC: u64 = 1_000_000_000;
/// One read from the socket (the body is streamed through it).
const READ_BUF: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    BadUrl(&'static str),
    /// `https://`: TLS is not a kernel concern (spec §3).
    Https,
    /// A host name instead of an IPv4 address (the kernel has no DNS).
    NeedsAddress,
    Connect(NetError),
    Io(NetError),
    TimedOut,
    BadResponse(&'static str),
    /// The answer said or sent more than the caller allows.
    TooLarge { limit: u64 },
    /// Not 2xx; the body was not read.
    Status(u16),
    /// The caller's sink refused the data.
    Sink,
    /// The caller's head callback (`get_to_with`) refused the answer before any of the body was read.
    HeadRefused,
    /// No memory for the transfer's own buffers or the socket (R2: to the caller, never a panic).
    NoMemory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url<'a> {
    pub addr: SockAddr,
    /// `host[:port]` as written (the Host header).
    pub authority: &'a str,
    pub path: &'a str,
}

fn parse_ipv4(s: &str) -> Option<Ipv4> {
    let mut o = [0u8; 4];
    let mut parts = s.split('.');
    for b in o.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        *b = p.parse().ok()?;
    }
    parts.next().is_none().then_some(Ipv4(o))
}

pub fn parse_url(url: &str) -> Result<Url<'_>, HttpError> {
    let (scheme, rest) = url.split_once("://").ok_or(HttpError::BadUrl("no scheme"))?;
    if scheme.eq_ignore_ascii_case("https") {
        return Err(HttpError::Https);
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return Err(HttpError::BadUrl("only http://"));
    }
    let rest = rest.split('#').next().unwrap_or("");
    let cut = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(cut);
    let path = if path.is_empty() { "/" } else { path };
    if path.starts_with('?') {
        return Err(HttpError::BadUrl("query without path"));
    }
    if path.bytes().any(|c| c <= b' ' || c == 0x7F) {
        return Err(HttpError::BadUrl("space or control character in the path"));
    }
    if authority.contains('@') {
        return Err(HttpError::BadUrl("user info"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok().filter(|p| *p != 0).ok_or(HttpError::BadUrl("port"))?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(HttpError::BadUrl("no host"));
    }
    let ip = match parse_ipv4(host) {
        Some(ip) => ip,
        None if host.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.') => return Err(HttpError::NeedsAddress),
        None => return Err(HttpError::BadUrl("host")),
    };
    Ok(Url { addr: SockAddr { ip, port }, authority, path })
}

pub fn request(u: &Url<'_>) -> String {
    alloc::format!("GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: BitsOS-V2\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n", u.path, u.authority)
}

// ---------------------------------------------------------------------------------------------------------
// Response head

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    /// Lower-case names, values trimmed.
    pub headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
    pub chunked: bool,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Where the head ends (after CRLF CRLF or LF LF), if it is complete.
pub fn head_end(b: &[u8]) -> Option<usize> {
    for (i, &c) in b.iter().enumerate() {
        if c != b'\n' {
            continue;
        }
        let next = &b[i + 1..];
        if next.first() == Some(&b'\n') {
            return Some(i + 2);
        }
        if next.starts_with(b"\r\n") {
            return Some(i + 3);
        }
    }
    None
}

pub fn parse_head(b: &[u8]) -> Result<Head, HttpError> {
    let text = core::str::from_utf8(b).map_err(|_| HttpError::BadResponse("head is not text"))?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let status_line = lines.next().ok_or(HttpError::BadResponse("empty"))?;
    let mut sp = status_line.splitn(3, ' ');
    let ver = sp.next().unwrap_or("");
    if ver != "HTTP/1.1" && ver != "HTTP/1.0" {
        return Err(HttpError::BadResponse("not HTTP/1.x"));
    }
    let code = sp.next().unwrap_or("");
    if code.len() != 3 || !code.bytes().all(|c| c.is_ascii_digit()) {
        return Err(HttpError::BadResponse("status code"));
    }
    let status: u16 = code.parse().map_err(|_| HttpError::BadResponse("status code"))?;
    let mut h = Head { status, headers: Vec::new(), content_length: None, chunked: false };
    for l in lines {
        if l.is_empty() {
            break;
        }
        let (n, v) = l.split_once(':').ok_or(HttpError::BadResponse("header without colon"))?;
        if n.is_empty() || n.bytes().any(|c| c <= b' ') {
            return Err(HttpError::BadResponse("header name"));
        }
        let (n, v) = (n.to_ascii_lowercase(), String::from(v.trim()));
        match n.as_str() {
            "content-length" => {
                if v.is_empty() || v.len() > 19 || !v.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(HttpError::BadResponse("content-length"));
                }
                let len: u64 = v.parse().map_err(|_| HttpError::BadResponse("content-length"))?;
                if h.content_length.is_some_and(|o| o != len) {
                    return Err(HttpError::BadResponse("two different content-lengths"));
                }
                h.content_length = Some(len);
            }
            "transfer-encoding" => {
                let codings: Vec<&str> = v.split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
                match codings.as_slice() {
                    [] => {}
                    [c] if c.eq_ignore_ascii_case("chunked") => h.chunked = true,
                    [c] if c.eq_ignore_ascii_case("identity") => {}
                    _ => return Err(HttpError::BadResponse("transfer coding other than chunked")),
                }
            }
            _ => {}
        }
        h.headers.push((n, v));
    }
    Ok(h)
}

// ---------------------------------------------------------------------------------------------------------
// Body

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// Reading the size line: value so far, hex digits seen, inside an extension, line length.
    Size { val: u64, digits: u8, ext: bool, len: usize },
    Data { left: u64 },
    /// CR LF after a chunk's data.
    DataEnd { cr: bool },
    /// Trailer lines after the last chunk; `len` of the current line.
    Trailer { len: usize },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    Length { left: u64 },
    Chunked(Chunk),
    UntilClose,
}

/// Decodes a body fed in pieces of any size; every byte of payload goes to `out`.
#[derive(Debug)]
pub struct Decoder {
    body: Body,
    limit: u64,
    pub received: u64,
}

impl Decoder {
    pub fn new(h: &Head, limit: u64) -> Result<Self, HttpError> {
        let body = if h.status == 204 || h.status == 304 {
            Body::Length { left: 0 }
        } else if h.chunked {
            Body::Chunked(Chunk::Size { val: 0, digits: 0, ext: false, len: 0 })
        } else if let Some(n) = h.content_length {
            if n > limit {
                return Err(HttpError::TooLarge { limit });
            }
            Body::Length { left: n }
        } else {
            Body::UntilClose
        };
        Ok(Decoder { body, limit, received: 0 })
    }

    pub fn done(&self) -> bool {
        matches!(self.body, Body::Length { left: 0 } | Body::Chunked(Chunk::Done))
    }

    /// The peer closed: fine for "until close", a truncated body otherwise.
    pub fn eof(&self) -> Result<(), HttpError> {
        match self.body {
            Body::UntilClose => Ok(()),
            _ if self.done() => Ok(()),
            _ => Err(HttpError::BadResponse("connection closed inside the body")),
        }
    }

    fn emit(&mut self, d: &[u8], out: &mut dyn FnMut(&[u8]) -> Result<(), HttpError>) -> Result<(), HttpError> {
        if d.is_empty() {
            return Ok(());
        }
        self.received += d.len() as u64;
        if self.received > self.limit {
            return Err(HttpError::TooLarge { limit: self.limit });
        }
        out(d)
    }

    /// Feeds bytes; returns how many belonged to the body (the rest follows the end of the body).
    pub fn feed(&mut self, mut d: &[u8], out: &mut dyn FnMut(&[u8]) -> Result<(), HttpError>) -> Result<usize, HttpError> {
        let total = d.len();
        while !d.is_empty() && !self.done() {
            match self.body {
                Body::UntilClose => {
                    self.emit(d, out)?;
                    d = &[];
                }
                Body::Length { left } => {
                    let n = left.min(d.len() as u64) as usize;
                    self.emit(&d[..n], out)?;
                    self.body = Body::Length { left: left - n as u64 };
                    d = &d[n..];
                }
                Body::Chunked(Chunk::Data { left }) => {
                    let n = left.min(d.len() as u64) as usize;
                    self.emit(&d[..n], out)?;
                    self.body = Body::Chunked(if left == n as u64 { Chunk::DataEnd { cr: false } } else { Chunk::Data { left: left - n as u64 } });
                    d = &d[n..];
                }
                Body::Chunked(c) => {
                    let b = d[0];
                    d = &d[1..];
                    self.body = Body::Chunked(chunk_byte(c, b)?);
                }
            }
        }
        Ok(total - d.len())
    }
}

/// One byte of chunk framing.
fn chunk_byte(c: Chunk, b: u8) -> Result<Chunk, HttpError> {
    Ok(match c {
        Chunk::Size { val, digits, ext, len } => {
            if len >= LINE_MAX {
                return Err(HttpError::BadResponse("chunk size line too long"));
            }
            match b {
                b'\n' if digits == 0 => return Err(HttpError::BadResponse("chunk size missing")),
                b'\n' if val == 0 => Chunk::Trailer { len: 0 },
                b'\n' => Chunk::Data { left: val },
                b'\r' => Chunk::Size { val, digits, ext, len: len + 1 },
                _ if ext => Chunk::Size { val, digits, ext, len: len + 1 },
                b';' | b' ' | b'\t' if digits > 0 => Chunk::Size { val, digits, ext: true, len: len + 1 },
                _ => {
                    let v = (b as char).to_digit(16).ok_or(HttpError::BadResponse("chunk size"))? as u64;
                    if digits >= 16 {
                        return Err(HttpError::BadResponse("chunk size too large"));
                    }
                    Chunk::Size { val: (val << 4) | v, digits: digits + 1, ext, len: len + 1 }
                }
            }
        }
        Chunk::DataEnd { cr } => match b {
            b'\r' if !cr => Chunk::DataEnd { cr: true },
            b'\n' => Chunk::Size { val: 0, digits: 0, ext: false, len: 0 },
            _ => return Err(HttpError::BadResponse("no line end after chunk data")),
        },
        Chunk::Trailer { len } => match b {
            b'\n' if len == 0 => Chunk::Done,
            b'\n' => Chunk::Trailer { len: 0 },
            b'\r' if len == 0 => Chunk::Trailer { len: 0 },
            _ if len >= LINE_MAX => return Err(HttpError::BadResponse("trailer line too long")),
            _ => Chunk::Trailer { len: len + 1 },
        },
        Chunk::Data { .. } | Chunk::Done => c,
    })
}

// ---------------------------------------------------------------------------------------------------------
// The client

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Largest body accepted (the caller's limit).
    pub max_body: u64,
    /// Whole transfer, from connect to the last byte.
    pub timeout_ns: u64,
    /// Longest pause between two pieces of the answer.
    pub idle_ns: u64,
    /// Whose connection the view shows on the kernel's stack (the kernel by default; MCP fetch names its caller).
    pub owner: policy::Subject,
}

impl Options {
    pub fn new(max_body: u64) -> Self {
        Options { max_body, timeout_ns: 300 * SEC, idle_ns: 20 * SEC, owner: policy::Subject::KERNEL }
    }
}

/// GET `url`, streaming the body into `sink` (normally `sock` = `net::stack()`). Non-2xx answers end as
/// `Status(code)` without reading the body.
pub fn get_to(sock: &dyn KSocket, url: &str, opts: &Options, sink: &mut dyn FnMut(&[u8]) -> Result<(), ()>) -> Result<Head, HttpError> {
    get_to_with(sock, url, opts, &mut |_| Ok(()), sink)
}

/// `get_to`, but the 2xx head goes to `on_head` before the first body byte reaches `sink` (RB-5a: Reload sizes
/// its initramfs block once from Content-Length instead of collecting pieces). The head has passed every check
/// by then: 2xx, a Content-Length within `opts.max_body`, a known transfer coding. `on_head` refusing ends the
/// transfer with `HeadRefused` (the connection is reset, no body is read).
pub fn get_to_with(
    sock: &dyn KSocket,
    url: &str,
    opts: &Options,
    on_head: &mut dyn FnMut(&Head) -> Result<(), ()>,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), ()>,
) -> Result<Head, HttpError> {
    let u = parse_url(url)?;
    let start = now_ns();
    let r = connect(sock, &u, opts).and_then(|s| {
        let r = transfer(&*s, &u, opts, start, on_head, sink);
        match &r {
            Ok(_) => s.close(),
            Err(_) => s.shutdown(),
        }
        r
    });
    let ms = now_ns().saturating_sub(start) / 1_000_000;
    match &r {
        Ok(h) => klog::event!(Info, "net", "http.get", url = url, status = h.status, ms = ms),
        Err(e) => klog::event!(Warn, "net", "http.get_failed", url = url, ms = ms, error = alloc::format!("{e:?}")),
    }
    r
}

/// GET `url` into memory (with Content-Length the whole body is reserved once, up front).
pub fn get(sock: &dyn KSocket, url: &str, opts: &Options) -> Result<(Head, Vec<u8>), HttpError> {
    let body = core::cell::RefCell::new(Vec::new());
    let h = get_to_with(
        sock,
        url,
        opts,
        &mut |h| match h.content_length {
            Some(n) => body.borrow_mut().try_reserve_exact(usize::try_from(n).map_err(|_| ())?).map_err(|_| ()),
            None => Ok(()),
        },
        &mut |d| {
            let mut b = body.borrow_mut();
            b.try_reserve(d.len()).map_err(|_| ())?;
            b.extend_from_slice(d);
            Ok(())
        },
    )
    .map_err(|e| if matches!(e, HttpError::Sink | HttpError::HeadRefused) { HttpError::NoMemory } else { e })?;
    Ok((h, body.into_inner()))
}

fn now_ns() -> u64 {
    #[cfg(target_os = "none")]
    {
        time::now_ns()
    }
    #[cfg(not(target_os = "none"))]
    {
        extern crate std;
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
    }
}

fn connect(sock: &dyn KSocket, u: &Url<'_>, opts: &Options) -> Result<alloc::boxed::Box<dyn TcpStream>, HttpError> {
    let t = Some(opts.idle_ns.min(opts.timeout_ns));
    // On the kernel's own stack the connection gets a wide receive window (16 MB in < 0.5 s on the LAN).
    #[cfg(target_os = "none")]
    if let Some(st) = crate::stack::kernel()
        && core::ptr::addr_eq(sock as *const dyn KSocket, st as *const crate::stack::Stack)
    {
        return st.tcp_connect_owned(u.addr, t, crate::stack::tcp::FETCH_RX, crate::stack::tcp::CONNECT_TX, opts.owner).map_err(HttpError::Connect);
    }
    sock.tcp_connect(u.addr, t).map_err(HttpError::Connect)
}

/// A read with a timeout that found nothing is a timeout, whatever the socket calls it.
fn io(e: NetError) -> HttpError {
    if matches!(e, NetError::TimedOut | NetError::WouldBlock) { HttpError::TimedOut } else { HttpError::Io(e) }
}

fn transfer(
    s: &dyn TcpStream,
    u: &Url<'_>,
    opts: &Options,
    start: u64,
    on_head: &mut dyn FnMut(&Head) -> Result<(), ()>,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), ()>,
) -> Result<Head, HttpError> {
    let deadline = start.saturating_add(opts.timeout_ns);
    let wait = || -> Result<u64, HttpError> {
        let now = now_ns();
        if now >= deadline { Err(HttpError::TimedOut) } else { Ok((deadline - now).min(opts.idle_ns)) }
    };
    s.write_all(request(u).as_bytes(), Some(wait()?)).map_err(io)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(READ_BUF).map_err(|_| HttpError::NoMemory)?;
    buf.resize(READ_BUF, 0u8);
    let read = |buf: &mut [u8]| -> Result<usize, HttpError> { s.read(buf, Some(wait()?)).map_err(io) };
    // Head (1xx answers are skipped).
    let mut head: Vec<u8> = Vec::new();
    let (h, rest) = loop {
        if let Some(end) = head_end(&head) {
            let h = parse_head(&head[..end])?;
            if (100..200).contains(&h.status) {
                head.drain(..end);
                continue;
            }
            break (h, head.split_off(end));
        }
        if head.len() > HEAD_MAX {
            return Err(HttpError::BadResponse("head larger than 64 KiB"));
        }
        let n = read(&mut buf)?;
        if n == 0 {
            return Err(HttpError::BadResponse("closed before the head was complete"));
        }
        head.try_reserve(n).map_err(|_| HttpError::NoMemory)?;
        head.extend_from_slice(&buf[..n]);
    };
    if !(200..300).contains(&h.status) {
        return Err(HttpError::Status(h.status));
    }
    let mut dec = Decoder::new(&h, opts.max_body)?;
    on_head(&h).map_err(|_| HttpError::HeadRefused)?;
    let mut out = |d: &[u8]| sink(d).map_err(|_| HttpError::Sink);
    dec.feed(&rest, &mut out)?;
    while !dec.done() {
        let n = read(&mut buf)?;
        if n == 0 {
            dec.eof()?;
            break;
        }
        dec.feed(&buf[..n], &mut out)?;
    }
    Ok(h)
}

#[cfg(test)]
mod tests;
