//! HTTP/1.1 for the MCP server, V1's rules (`mcp/httpreq.asm`, `httpout.asm`): only `POST /mcp` with the
//! bearer token goes on; every other request is answered here with its status and the connection closes.
//! Pure functions over byte slices; the connection loop (`conn.rs`) feeds them.

use alloc::string::String;
use core::fmt::Write;

/// A request head (request line, headers, empty line) longer than this is answered 431: a bound against
/// hostile input, V1's `HEAD_MAX`.
pub const HEAD_MAX: usize = 16 * 1024;
/// A body announced longer than this is answered 413 (V1's `MCP_REQ_MAX`).
pub const BODY_MAX: usize = 256 * 1024;
/// Method and path in the log line are cut here (V1: 60 characters).
pub const WHAT_MAX: usize = 60;

/// What the head says about a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// 0 = a POST to /mcp with the token and a body we take; else the status to refuse it with.
    pub status: u16,
    /// The body's length (valid when `status` is 0).
    pub length: usize,
    /// The client wants the connection closed after the answer (`Connection: close`, HTTP/1.0).
    pub close: bool,
    /// The client waits for `100 Continue` before it sends the body (`Expect: 100-continue`, curl does so for
    /// large bodies).
    pub expect: bool,
}

/// Leading CR and LF bytes (empty lines before a request are allowed, RFC 9112 2.2).
pub fn empty_lines(buf: &[u8]) -> usize {
    buf.iter().take_while(|&&b| b == b'\r' || b == b'\n').count()
}

/// The index just past the head's empty line ("\n\n" or "\n\r\n"), searching from `from`.
pub fn head_end(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < buf.len() {
        if buf[i] == b'\n' {
            match buf.get(i + 1) {
                Some(b'\n') => return Some(i + 2),
                Some(b'\r') if buf.get(i + 2) == Some(&b'\n') => return Some(i + 3),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn word(h: &[u8], mut i: usize) -> usize {
    while i < h.len() && !matches!(h[i], b' ' | b'\r' | b'\n' | 0) {
        i += 1;
    }
    i
}

/// Past a line end (CR LF or LF), or None.
fn eol(h: &[u8], i: usize) -> Option<usize> {
    match (h.get(i), h.get(i + 1)) {
        (Some(b'\r'), Some(b'\n')) => Some(i + 2),
        (Some(b'\n'), _) => Some(i + 1),
        _ => None,
    }
}

fn same(text: &[u8], lower: &[u8]) -> bool {
    text.eq_ignore_ascii_case(lower)
}

/// Every byte of a token as long as ours is compared: the time says nothing about where it differs.
fn token_ok(value: &[u8], token: &[u8]) -> bool {
    if value.len() < 7 || !same(&value[..6], b"bearer") || value[6] != b' ' {
        return false;
    }
    let mut i = 6;
    while value.get(i) == Some(&b' ') {
        i += 1;
    }
    let got = &value[i..];
    if got.is_empty() || token.is_empty() || got.len() != token.len() {
        return false;
    }
    got.iter().zip(token).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

const BAD: Head = Head { status: 400, length: 0, close: true, expect: false };

/// `Connection` is a comma-separated list of options (RFC 9110 7.6.1): close if one of them is `close`.
fn says_close(value: &[u8]) -> bool {
    value.split(|&b| b == b',').any(|t| same(t.trim_ascii(), b"close"))
}

/// Reads a whole head (as `head_end` delimits it). Order of the answers as V1: 400 for a malformed head,
/// then 404, 401, 403, 405, 411, 413; V2 adds 417 for an `Expect` other than `100-continue` last.
pub fn parse_head(h: &[u8], token: &[u8]) -> Head {
    let (mut auth, mut origin, mut coded, mut big, mut close) = (false, false, false, false, false);
    let (mut expect, mut odd_expect) = (false, false);
    let mut length: Option<usize> = None;
    let m_end = word(h, 0);
    if h.get(m_end) != Some(&b' ') {
        return BAD;
    }
    let post = &h[..m_end] == b"POST";
    let p_end = word(h, m_end + 1);
    let mcp = &h[m_end + 1..p_end] == b"/mcp";
    let mut i = p_end;
    if h.get(i) == Some(&b' ') {
        let v_end = word(h, i + 1);
        close = &h[i + 1..v_end] == b"HTTP/1.0";
        i = v_end;
    }
    let Some(mut i) = eol(h, i) else { return BAD };
    while i < h.len() && h[i] != b'\r' && h[i] != b'\n' {
        let name_start = i;
        while i < h.len() && h[i] != b':' {
            if h[i] <= b' ' {
                return BAD;
            }
            i += 1;
        }
        if i >= h.len() {
            return BAD;
        }
        let name = &h[name_start..i];
        i += 1;
        while matches!(h.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        let v_start = i;
        while i < h.len() && h[i] != b'\r' && h[i] != b'\n' {
            if h[i] == 0 {
                return BAD;
            }
            i += 1;
        }
        let mut v_end = i;
        while v_end > v_start && matches!(h[v_end - 1], b' ' | b'\t') {
            v_end -= 1;
        }
        let value = &h[v_start..v_end];
        let Some(next) = eol(h, i) else { return BAD };
        i = next;
        if same(name, b"content-length") {
            if length.is_some() || value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return BAD;
            }
            // Digits beyond BODY_MAX only count as big: no wrap-around for 2^64 + 5.
            let n = value.iter().try_fold(0usize, |acc, d| acc.checked_mul(10)?.checked_add((d - b'0') as usize));
            big = n.is_none_or(|n| n > BODY_MAX);
            length = Some(n.unwrap_or(usize::MAX));
        } else if same(name, b"authorization") {
            auth = auth || token_ok(value, token);
        } else if same(name, b"connection") {
            close = close || says_close(value);
        } else if same(name, b"expect") {
            if same(value, b"100-continue") {
                expect = true;
            } else {
                odd_expect = true;
            }
        } else if same(name, b"transfer-encoding") {
            coded = true;
        } else if same(name, b"origin") {
            origin = true;
        }
    }
    let status = if !mcp {
        404
    } else if !auth {
        401
    } else if origin {
        403
    } else if !post {
        405
    } else if coded || length.is_none() {
        411
    } else if big {
        413
    } else if odd_expect {
        417
    } else {
        0
    };
    Head { status, length: if status == 0 { length.unwrap_or(0) } else { 0 }, close: close || status != 0, expect: expect && status == 0 }
}

/// Method and path of the request line for the log: printable ASCII, '?' for anything else, at most
/// `WHAT_MAX` characters.
pub fn what(h: &[u8]) -> String {
    let mut s = String::new();
    let mut blanks = 0;
    for &b in h.iter().take_while(|&&b| !matches!(b, b'\r' | b'\n' | 0)) {
        if b == b' ' {
            blanks += 1;
            if blanks == 2 {
                break;
            }
        }
        if s.len() == WHAT_MAX {
            break;
        }
        s.push(if (b' '..=b'~').contains(&b) { b as char } else { '?' });
    }
    s
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "",
    }
}

/// The answer's head; `Connection` is always its last header (V1's clients look for it there).
pub fn answer_head(status: u16, body_len: usize, close: bool) -> String {
    let mut s = String::with_capacity(160);
    let _ = write!(s, "HTTP/1.1 {status} {}\r\n", reason(status));
    if status == 401 {
        s.push_str("WWW-Authenticate: Bearer\r\n");
    }
    if status == 405 {
        s.push_str("Allow: POST\r\n");
    }
    if body_len > 0 {
        s.push_str("Content-Type: application/json\r\n");
    }
    let _ = write!(s, "Content-Length: {body_len}\r\nConnection: {}\r\n\r\n", if close { "close" } else { "keep-alive" });
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::format;
    use std::string::String;

    const T: &[u8] = b"test-token-1234";

    fn req(method: &str, path: &str, headers: &[&str], eol: &str) -> String {
        let mut s = format!("{method} {path} HTTP/1.1{eol}Host: bitsos{eol}");
        for h in headers {
            s.push_str(h);
            s.push_str(eol);
        }
        s.push_str(eol);
        s
    }
    fn st(r: &str) -> u16 {
        let b = r.as_bytes();
        assert_eq!(head_end(b, 0), Some(b.len()), "{r:?}");
        parse_head(b, T).status
    }
    const A: &str = "Authorization: Bearer test-token-1234";
    const L: &str = "Content-Length: 5";

    #[test]
    fn statuses_in_v1_order() {
        assert_eq!(st(&req("POST", "/mcp", &[A, L], "\r\n")), 0);
        assert_eq!(st(&req("POST", "/mcp", &[A, L], "\n")), 0, "LF line ends");
        assert_eq!(st(&req("POST", "/other", &[A, L], "\r\n")), 404);
        assert_eq!(st(&req("POST", "/mcp/", &[A, L], "\r\n")), 404);
        assert_eq!(st(&req("POST", "/mcp", &[L], "\r\n")), 401);
        assert_eq!(st(&req("GET", "/mcp", &[], "\r\n")), 401, "the token before the method");
        assert_eq!(st(&req("POST", "/mcp", &[A, L, "Origin: http://evil.example"], "\r\n")), 403);
        assert_eq!(st(&req("GET", "/mcp", &[A], "\r\n")), 405);
        assert_eq!(st(&req("DELETE", "/mcp", &[A], "\r\n")), 405);
        assert_eq!(st(&req("POST", "/mcp", &[A], "\r\n")), 411);
        assert_eq!(st(&req("POST", "/mcp", &[A, "Transfer-Encoding: chunked"], "\r\n")), 411);
        assert_eq!(st(&req("POST", "/mcp", &[A, "Content-Length: 262145"], "\r\n")), 413);
        assert_eq!(st(&req("POST", "/mcp", &[A, "Content-Length: 18446744073709551621"], "\r\n")), 413);
        assert_eq!(st(&req("POST", "/mcp", &[A, "Content-Length: 262144"], "\r\n")), 0);
    }

    #[test]
    fn malformed_heads_are_400() {
        for h in [&["No colon here"][..], &["X-Name : 1"], &["Content-Length: 12a"], &[L, L], &["Content-Length:"], &["X: a\0b"]] {
            let mut v: std::vec::Vec<&str> = std::vec![A];
            v.extend_from_slice(h);
            assert_eq!(st(&req("POST", "/mcp", &v, "\r\n")), 400, "{h:?}");
        }
        assert_eq!(parse_head(b"POST\r\n\r\n", T).status, 400);
        assert_eq!(parse_head(b"POST /mcp HTTP/1.1\rX: 1\r\n\r\n", T).status, 400, "a CR alone");
    }

    #[test]
    fn token_rules() {
        for (h, ok) in [
            ("authorization:   bEaReR test-token-1234  ", true),
            ("Authorization: Bearer  test-token-1234", true),
            ("Authorization: Basic test-token-1234", false),
            ("Authorization: Bearer test-token-1235", false),
            ("Authorization: Bearer test-token-12345", false),
            ("Authorization: Bearer est-token-1234", false),
            ("Authorization: Bearer TEST-TOKEN-1234", false),
            ("Authorization: Bearer", false),
            ("Authorization: Bearertest-token-1234", false),
        ] {
            assert_eq!(st(&req("POST", "/mcp", &[h, L], "\r\n")) == 0, ok, "{h}");
        }
    }

    #[test]
    fn close_and_lengths() {
        let h = parse_head(req("POST", "/mcp", &[A, L, "Connection: Close"], "\r\n").as_bytes(), T);
        assert!(h.close && h.length == 5 && h.status == 0);
        let r = "POST /mcp HTTP/1.0\r\nAuthorization: Bearer test-token-1234\r\nContent-Length: 2\r\n\r\n";
        assert!(parse_head(r.as_bytes(), T).close);
        assert!(!parse_head(req("POST", "/mcp", &[A, L], "\r\n").as_bytes(), T).close);
        assert!(parse_head(req("POST", "/x", &[A, L], "\r\n").as_bytes(), T).close, "refusals close");
        for v in ["keep-alive, close", "TE,close", " Close ", "upgrade, CLOSE, x"] {
            let c = format!("Connection: {v}");
            assert!(parse_head(req("POST", "/mcp", &[A, L, &c], "\r\n").as_bytes(), T).close, "{v}");
        }
        for v in ["keep-alive", "closed", "keep-alive, te", "close-ish"] {
            let c = format!("Connection: {v}");
            assert!(!parse_head(req("POST", "/mcp", &[A, L, &c], "\r\n").as_bytes(), T).close, "{v}");
        }
        let both = req("POST", "/mcp", &[A, L, "Connection: keep-alive", "Connection: close"], "\r\n");
        assert!(parse_head(both.as_bytes(), T).close, "two Connection headers");
    }

    #[test]
    fn expect_continue() {
        let h = parse_head(req("POST", "/mcp", &[A, L, "Expect: 100-Continue"], "\r\n").as_bytes(), T);
        assert!(h.status == 0 && h.expect);
        assert!(!parse_head(req("POST", "/mcp", &[A, L], "\r\n").as_bytes(), T).expect);
        assert_eq!(st(&req("POST", "/mcp", &[A, L, "Expect: something-else"], "\r\n")), 417);
        let h = parse_head(req("POST", "/mcp", &[L, "Expect: 100-continue"], "\r\n").as_bytes(), T);
        assert!(h.status == 401 && !h.expect, "no 100 Continue for a refused request");
        assert_eq!(reason(417), "Expectation Failed");
    }

    #[test]
    fn hostile_lengths_and_lines() {
        for l in ["-1", "1e3", "+5", " ", "0x10", "5 5", "5,5"] {
            let c = format!("Content-Length: {l}");
            assert_eq!(st(&req("POST", "/mcp", &[A, &c], "\r\n")), 400, "{l:?}");
        }
        assert_eq!(st(&req("POST", "/mcp", &[A, "Content-Length: 5", "Content-Length: 7"], "\r\n")), 400, "conflicting lengths");
        assert_eq!(st(&req("POST", "/mcp", &[A, "Content-Length: 5", "content-length: 5"], "\r\n")), 400, "even equal ones");
        assert_eq!(st(&req("POST", "/mcp", &[A, L, "X-A: 1", " folded"], "\r\n")), 400, "obs-fold");
        assert_eq!(st(&req("POST", "/mcp", &[A, L, "X-A: 1", "\tfolded"], "\r\n")), 400, "obs-fold with a tab");
        assert_eq!(st(&req("POST", "/mcp", &[A, L, "X-\0A: 1"], "\r\n")), 400, "NUL in a name");
        assert_eq!(st(&req("POST", "/mcp", &[A, L, "X-A: 1\r2"], "\r\n")), 400, "CR inside a value");
        assert_eq!(st(&req("POST", "/mcp", &[A, L, "Content-Length : 5"], "\r\n")), 400, "blank before the colon");
    }

    #[test]
    fn head_end_and_what() {
        assert_eq!(head_end(b"POST / HTTP/1.1\r\nA: b\r\n\r\nbody", 0), Some(25));
        assert_eq!(head_end(b"POST / HTTP/1.1\nA: b\n\nbody", 0), Some(22));
        assert_eq!(head_end(b"POST / HTTP/1.1\r\nA: b\r\n", 0), None);
        assert_eq!(empty_lines(b"\r\n\nPOST"), 3);
        assert_eq!(what(b"POST /mcp HTTP/1.1\r\n"), "POST /mcp");
        let long = format!("POST /{} HTTP/1.1\r\n", "a".repeat(2000));
        assert_eq!(what(long.as_bytes()).len(), 60);
        assert_eq!(what("GET /ä HTTP/1.1".as_bytes()), "GET /??");
    }

    #[test]
    fn answer_heads() {
        let h = answer_head(401, 0, true);
        assert!(h.contains("\r\nWWW-Authenticate: Bearer\r\n") && h.ends_with("Content-Length: 0\r\nConnection: close\r\n\r\n"));
        assert!(answer_head(405, 0, true).contains("\r\nAllow: POST\r\n"));
        let ok = answer_head(200, 12, false);
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n") && ok.ends_with("Connection: keep-alive\r\n\r\n"));
        assert!(!answer_head(202, 0, false).contains("Content-Type"));
    }
}
