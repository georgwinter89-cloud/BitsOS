//! HTTP/1.1 GET on one connection (`Connection: close`): the request, the response head and a body reader for
//! the three framings (Content-Length, chunked, until close). Server data never panics: anything malformed is a
//! `Bad` error the caller maps to the protocol exit code.

use std::io::{self, BufRead, Read};

/// Caps on server-controlled sizes so a hostile server cannot make fetch hold unbounded memory: a response
/// head (curl allows 100 KiB per header, 300 KiB in all; 256 KiB covers every real server) and one chunk-size
/// or trailer line. The whole chunked trailer counts against `MAX_HEAD` too (curl caps it alike).
const MAX_HEAD: usize = 256 * 1024;
const MAX_LINE: usize = 8 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Bad(String),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

pub fn request(host_header: &str, target: &str) -> Vec<u8> {
    format!(
        "GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: fetch/{} (BitsOS)\r\nAccept: */*\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    )
    .into_bytes()
}

#[derive(Debug)]
pub struct Head {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// RFC 9112 section 6.3: 204, 205 and 304 never have a body whatever the headers say (205: RFC 9110 15.3.6);
    /// chunked wins over Content-Length; differing Content-Length values are an error.
    pub fn framing(&self) -> Result<Framing, Error> {
        if matches!(self.status, 204 | 205 | 304) {
            return Ok(Framing::Length(0));
        }
        let te: Vec<String> =
            self.all("transfer-encoding").flat_map(|v| v.split(',')).map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()).collect();
        if let Some(last) = te.last() {
            if last != "chunked" || te.iter().any(|t| t != "chunked" && t != "identity") {
                return Err(Error::Bad(format!("unsupported Transfer-Encoding: {}", te.join(", "))));
            }
            return Ok(Framing::Chunked);
        }
        let mut len: Option<u64> = None;
        for v in self.all("content-length").flat_map(|v| v.split(',')) {
            let v = v.trim();
            let n: u64 = v
                .parse()
                .ok()
                .filter(|_| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| Error::Bad(format!("bad Content-Length: {v:?}")))?;
            if len.is_some_and(|l| l != n) {
                return Err(Error::Bad("differing Content-Length headers".into()));
            }
            len = Some(n);
        }
        Ok(match len {
            Some(n) => Framing::Length(n),
            None => Framing::Close,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Framing {
    Length(u64),
    Chunked,
    Close,
}

/// Reads one CRLF- (or LF-) terminated line of at most `max` bytes, without the line end. `Ok(None)` at a clean
/// end of stream before any byte.
fn read_line<R: BufRead>(r: &mut R, max: usize, what: &str) -> Result<Option<String>, Error> {
    let mut buf = Vec::new();
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(Error::Bad(format!("connection closed inside the {what}")));
        }
        let (take, done) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (avail.len(), false),
        };
        buf.extend_from_slice(&avail[..take]);
        r.consume(take);
        if buf.len() > max {
            return Err(Error::Bad(format!("{what} longer than {max} bytes")));
        }
        if done {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return String::from_utf8(buf).map(Some).map_err(|_| Error::Bad(format!("{what} is not text")));
        }
    }
}

/// The response head; interim 1xx responses (other than 101) are skipped.
pub fn read_head<R: BufRead>(r: &mut R) -> Result<Head, Error> {
    // Interim responses count toward the cap too, so endless 100s end as an error.
    let mut used = 0usize;
    loop {
        let mut line = |r: &mut R| -> Result<String, Error> {
            let l = read_line(r, MAX_HEAD.saturating_sub(used), "response head")?.ok_or_else(|| Error::Bad("connection closed before a response".into()))?;
            used += l.len() + 2;
            Ok(l)
        };
        let status_line = line(r)?;
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        let code = parts.next().unwrap_or("");
        if !version.starts_with("HTTP/1.") || code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Bad(format!("not an HTTP/1.x status line: {:?}", trunc(&status_line))));
        }
        let status: u16 = code.parse().unwrap_or(0);
        let reason = parts.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        loop {
            let l = line(r)?;
            if l.is_empty() {
                break;
            }
            if l.starts_with([' ', '\t']) {
                return Err(Error::Bad("folded header lines are obsolete (RFC 9112 5.2)".into()));
            }
            let (n, v) = l.split_once(':').ok_or_else(|| Error::Bad(format!("header without a colon: {:?}", trunc(&l))))?;
            if n.is_empty() || n.ends_with([' ', '\t']) {
                return Err(Error::Bad(format!("bad header name: {:?}", trunc(n))));
            }
            headers.push((n.to_string(), v.trim().to_string()));
        }
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        return Ok(Head { status, reason, headers });
    }
}

fn trunc(s: &str) -> &str {
    let mut end = s.len().min(80);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The body as a `Read`. A connection that ends early is `UnexpectedEof`; malformed chunk framing is
/// `InvalidData` carrying the reason.
pub struct Body<R> {
    inner: R,
    framing: Framing,
    /// Bytes left in the body (Length) or in the current chunk (Chunked).
    left: u64,
    /// Chunked: the next chunk's size line is still to come; `done`: the body has ended.
    need_size: bool,
    done: bool,
}

impl<R: BufRead> Body<R> {
    pub fn new(inner: R, framing: Framing) -> Body<R> {
        let left = if let Framing::Length(n) = framing { n } else { 0 };
        Body { inner, framing, left, need_size: true, done: framing == Framing::Length(0) }
    }

    fn next_chunk(&mut self) -> io::Result<()> {
        let line = read_line(&mut self.inner, MAX_LINE, "chunk size line").map_err(to_io)?.ok_or_else(|| eof("chunked body ended without its last chunk"))?;
        let size = line.split(';').next().unwrap_or("").trim();
        if size.is_empty() || size.len() > 16 || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad(format!("bad chunk size line: {:?}", trunc(&line))));
        }
        let n = u64::from_str_radix(size, 16).map_err(|_| bad("chunk size out of range".into()))?;
        self.need_size = false;
        if n == 0 {
            let mut used = 0usize;
            loop {
                let max = MAX_LINE.min(MAX_HEAD - used);
                match read_line(&mut self.inner, max, "chunked trailer").map_err(to_io)? {
                    None => return Err(eof("connection closed inside the chunked trailer")),
                    Some(l) if l.is_empty() => break,
                    Some(l) => used += l.len() + 2,
                }
                if used >= MAX_HEAD {
                    return Err(bad(format!("chunked trailer longer than {MAX_HEAD} bytes")));
                }
            }
            self.done = true;
        }
        self.left = n;
        Ok(())
    }

    fn end_of_chunk(&mut self) -> io::Result<()> {
        match read_line(&mut self.inner, 2, "chunk end").map_err(to_io)? {
            Some(l) if l.is_empty() => {
                self.need_size = true;
                Ok(())
            }
            Some(_) => Err(bad("chunk data longer than its size".into())),
            None => Err(eof("connection closed after chunk data")),
        }
    }
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.done {
                return Ok(0);
            }
            match self.framing {
                Framing::Close => return self.inner.read(buf),
                Framing::Length(_) => {
                    let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(eof(&format!("connection closed with {} body bytes missing", self.left)));
                    }
                    self.left -= n as u64;
                    self.done = self.left == 0;
                    return Ok(n);
                }
                Framing::Chunked => {
                    if self.need_size {
                        self.next_chunk()?;
                        continue;
                    }
                    if self.left == 0 {
                        self.end_of_chunk()?;
                        continue;
                    }
                    let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(eof("connection closed inside a chunk"));
                    }
                    self.left -= n as u64;
                    return Ok(n);
                }
            }
        }
    }
}

fn to_io(e: Error) -> io::Error {
    match e {
        Error::Io(e) => e,
        Error::Bad(s) if s.starts_with("connection closed") => eof(&s),
        Error::Bad(s) => bad(s),
    }
}

fn eof(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, what.to_string())
}

fn bad(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(s: &str) -> Result<(Head, Vec<u8>), String> {
        let mut r = Cursor::new(s.as_bytes().to_vec());
        let h = read_head(&mut r).map_err(|e| format!("{e:?}"))?;
        let mut out = Vec::new();
        let f = h.framing().map_err(|e| format!("{e:?}"))?;
        Body::new(&mut r, f).read_to_end(&mut out).map_err(|e| format!("{:?}: {e}", e.kind()))?;
        Ok((h, out))
    }

    #[test]
    fn length_chunked_and_close() {
        let (h, b) = parse("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX: y\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!((h.status, h.reason.as_str(), h.get("x"), b.as_slice()), (200, "OK", Some("y"), &b"hello"[..]));
        let (_, b) =
            parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 99\r\n\r\n3;ext=1\r\nabc\r\nA\r\n0123456789\r\n0\r\nT: 1\r\n\r\n").unwrap();
        assert_eq!(b, b"abc0123456789");
        let (_, b) = parse("HTTP/1.0 200 OK\n\nuntil close").unwrap();
        assert_eq!(b, b"until close");
        let (h, b) = parse("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").unwrap();
        assert_eq!((h.status, b.len()), (404, 0));
        // 204/205/304 have no body even with Content-Length or an open connection (RFC 9112 6.3 rule 1).
        for st in ["204 No Content", "205 Reset Content", "304 Not Modified"] {
            let (h, b) = parse(&format!("HTTP/1.1 {st}\r\nContent-Length: 7\r\n\r\nignored")).unwrap();
            assert_eq!((h.framing().unwrap(), b.len()), (Framing::Length(0), 0), "{st}");
        }
    }

    #[test]
    fn trailer_is_capped_in_total() {
        let line = format!("X: {}\r\n", "a".repeat(MAX_LINE - 10));
        let ok = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nz\r\n0\r\n{}\r\n", line.repeat(3));
        assert_eq!(parse(&ok).unwrap().1, b"z");
        let flood = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n{}\r\n", line.repeat(MAX_HEAD / MAX_LINE + 2));
        let e = parse(&flood).unwrap_err();
        assert!(e.contains("InvalidData") && e.contains("trailer"), "{e}");
    }

    #[test]
    fn malformed_is_an_error_not_a_panic() {
        for s in [
            "",
            "SSH-2.0-x\r\n\r\n",
            "HTTP/1.1 2000 OK\r\n\r\n",
            "HTTP/1.1 200 OK\r\nno colon\r\n\r\n",
            "HTTP/1.1 200 OK\r\n folded\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc",
            "HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabcd",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcd\r\n0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFF\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n",
            "HTTP/1.1 200 OK\r\nX: \u{0}\u{ff}",
        ] {
            assert!(parse(s).is_err(), "{s:?}");
        }
        let huge = format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(parse(&huge).is_err());
        // Lines that end exactly at the cap, then more: no underflow, still an error.
        let edge = format!("HTTP/1.1 200 OK\r\nX: {}\r\nY: z\r\n\r\n", "a".repeat(MAX_HEAD - 22));
        assert!(parse(&edge).is_err());
        assert!(parse(&"HTTP/1.1 100 Continue\r\n\r\n".repeat(MAX_HEAD / 20)).is_err());
    }

    #[test]
    fn request_is_http11_with_close() {
        let r = String::from_utf8(request("h:81", "/a?b")).unwrap();
        assert!(r.starts_with("GET /a?b HTTP/1.1\r\nHost: h:81\r\n") && r.contains("Connection: close\r\n") && r.ends_with("\r\n\r\n"));
    }
}
