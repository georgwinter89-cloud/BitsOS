//! One HTTP/1.1 exchange: a POST with a JSON body, then the answer's head and a body that is read piece by
//! piece (chunked, by length, or until the connection closes). `Connection: close` always: one request per
//! connection keeps the framing simple and a provider key never waits in an idle socket.

use super::conn::Conn;
use super::url::Url;
use super::{Ctl, NetError};

enum Framing {
    Close,
    Length(u64),
    /// Bytes left in the current chunk; None before the next size line.
    Chunked(Option<u64>),
    Done,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    conn: Conn,
    /// Bytes read from the connection and not handed out yet.
    buf: Vec<u8>,
    framing: Framing,
}

/// Header names and values a request may carry: no line breaks (a value from a setting must not add a header).
fn clean(s: &str) -> bool {
    !s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

pub fn post(conn: Conn, url: &Url, path: &str, headers: &[(String, String)], body: &str, ctl: &Ctl) -> Result<Response, NetError> {
    send(conn, url, "POST", &format!("{}{}", url.base, path), headers, Some(body), ctl)
}

/// A GET of `target` (path and query as they go on the request line), for `web_holen`.
pub fn get(conn: Conn, url: &Url, target: &str, headers: &[(String, String)], ctl: &Ctl) -> Result<Response, NetError> {
    send(conn, url, "GET", target, headers, None, ctl)
}

fn send(mut conn: Conn, url: &Url, method: &str, target: &str, headers: &[(String, String)], body: Option<&str>, ctl: &Ctl) -> Result<Response, NetError> {
    if !clean(target) || target.contains(' ') || headers.iter().any(|(k, v)| !clean(k) || !clean(v)) {
        return Err(NetError::Protocol("a header or the path contains a line break".into()));
    }
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", url.host_header());
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body.unwrap_or(""));
    conn.write_all(req.as_bytes(), ctl)?;
    let mut r = Response { status: 0, headers: Vec::new(), conn, buf: Vec::new(), framing: Framing::Close };
    loop {
        let head = r.take_until(b"\r\n\r\n", ctl)?.ok_or_else(|| NetError::Cut("the connection closed before an answer".into()))?;
        let text = String::from_utf8_lossy(&head).into_owned();
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let mut parts = status_line.split(' ');
        let status = match (parts.next(), parts.next().and_then(|s| s.parse::<u16>().ok())) {
            (Some(v), Some(s)) if v.starts_with("HTTP/1.") => s,
            _ => return Err(NetError::Protocol(format!("not an HTTP answer: {:?}", status_line.chars().take(80).collect::<String>()))),
        };
        // 1xx heads come before the real one.
        if (100..200).contains(&status) {
            continue;
        }
        r.status = status;
        r.headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        break;
    }
    r.framing = if r.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        Framing::Chunked(None)
    } else if let Some(n) = r.header("content-length") {
        Framing::Length(n.parse().map_err(|_| NetError::Protocol(format!("content-length {n:?}")))?)
    } else {
        Framing::Close
    };
    Ok(r)
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Reads more from the connection into the buffer; false when it closed.
    fn fill(&mut self, ctl: &Ctl) -> Result<bool, NetError> {
        if self.buf.len() > ctl.budget {
            return Err(NetError::TooBig);
        }
        let mut tmp = [0u8; 16 * 1024];
        let n = self.conn.read(&mut tmp, ctl)?;
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(n > 0)
    }

    /// Everything up to and including `mark`, without the mark; None when the connection closed first.
    fn take_until(&mut self, mark: &[u8], ctl: &Ctl) -> Result<Option<Vec<u8>>, NetError> {
        let mut from = 0;
        loop {
            if let Some(i) = self.buf[from..].windows(mark.len()).position(|w| w == mark) {
                let rest = self.buf.split_off(from + i + mark.len());
                let mut head = std::mem::replace(&mut self.buf, rest);
                head.truncate(from + i);
                return Ok(Some(head));
            }
            from = self.buf.len().saturating_sub(mark.len() - 1);
            if !self.fill(ctl)? {
                return Ok(None);
            }
        }
    }

    fn take(&mut self, max: usize) -> Vec<u8> {
        let n = max.min(self.buf.len());
        let rest = self.buf.split_off(n);
        std::mem::replace(&mut self.buf, rest)
    }

    /// The next piece of the body; empty at its end. A body that stops before its framing says so is `Cut`.
    pub fn read(&mut self, ctl: &Ctl) -> Result<Vec<u8>, NetError> {
        loop {
            match self.framing {
                Framing::Done => return Ok(Vec::new()),
                Framing::Close => {
                    if self.buf.is_empty() && !self.fill(ctl)? {
                        self.framing = Framing::Done;
                        return Ok(Vec::new());
                    }
                    return Ok(self.take(usize::MAX));
                }
                Framing::Length(0) => self.framing = Framing::Done,
                Framing::Length(left) => {
                    if self.buf.is_empty() && !self.fill(ctl)? {
                        return Err(NetError::Cut(format!("{left} bytes of the answer are missing")));
                    }
                    let piece = self.take(usize::try_from(left).unwrap_or(usize::MAX));
                    self.framing = Framing::Length(left - piece.len() as u64);
                    return Ok(piece);
                }
                Framing::Chunked(None) => {
                    let line = self.take_until(b"\r\n", ctl)?.ok_or_else(|| NetError::Cut("the answer stopped between two chunks".into()))?;
                    let text = String::from_utf8_lossy(&line);
                    let hex = text.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(hex, 16).map_err(|_| NetError::Protocol(format!("chunk size {hex:?}")))?;
                    if size == 0 {
                        // Trailers until the empty line; a server that closes right after the last chunk is fine.
                        while let Some(l) = self.take_until(b"\r\n", ctl)? {
                            if l.is_empty() {
                                break;
                            }
                        }
                        self.framing = Framing::Done;
                    } else {
                        self.framing = Framing::Chunked(Some(size));
                    }
                }
                Framing::Chunked(Some(0)) => {
                    match self.take_until(b"\r\n", ctl)? {
                        Some(l) if l.is_empty() => self.framing = Framing::Chunked(None),
                        Some(_) => return Err(NetError::Protocol("a chunk is longer than its size".into())),
                        None => return Err(NetError::Cut("the answer stopped at the end of a chunk".into())),
                    }
                }
                Framing::Chunked(Some(left)) => {
                    if self.buf.is_empty() && !self.fill(ctl)? {
                        return Err(NetError::Cut("the answer stopped inside a chunk".into()));
                    }
                    let piece = self.take(usize::try_from(left).unwrap_or(usize::MAX));
                    self.framing = Framing::Chunked(Some(left - piece.len() as u64));
                    return Ok(piece);
                }
            }
        }
    }

    /// The whole body (an error answer), bounded by the budget.
    pub fn read_all(&mut self, ctl: &Ctl) -> Result<Vec<u8>, NetError> {
        let mut all = Vec::new();
        loop {
            let piece = self.read(ctl)?;
            if piece.is_empty() {
                return Ok(all);
            }
            all.extend_from_slice(&piece);
            if all.len() > ctl.budget {
                return Err(NetError::TooBig);
            }
        }
    }
}
