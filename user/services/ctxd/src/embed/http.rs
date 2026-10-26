//! Minimal HTTP/1.1 client over a Unix socket (llama-server listens on `<data>/tmp/llama.sock`, so no TCP
//! port is opened at all). One request per connection, Content-Length or chunked answers.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub fn request(sock: &Path, method: &str, path: &str, body: &[u8], timeout: Duration) -> io::Result<(u16, Vec<u8>)> {
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
    s.set_write_timeout(Some(timeout.max(Duration::from_millis(1))))?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 =
        line.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| io::Error::other("bad status line"))?;
    let mut len = None;
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "content-length" {
                len = v.trim().parse::<usize>().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| io::Error::other("bad chunk"))?;
            if n == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + n, 0);
            r.read_exact(&mut body[start..])?;
            line.clear();
            r.read_line(&mut line)?;
        }
    } else if let Some(n) = len {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
    }
    Ok((status, body))
}
