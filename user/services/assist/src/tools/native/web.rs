//! `web_holen`: one web page as plain text. GET over the same connection code the providers use (TLS with the
//! system's certificates), redirects followed a few times, the page's markup taken out, the answer capped.

use super::{OUT_MAX, cap};
use crate::json::Json;
use crate::net::conn::{self, Conn};
use crate::net::url::Url;
use crate::net::{Ctl, http};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(30);
const REDIRECTS: usize = 5;
/// The most bytes of a page read.
const PAGE_MAX: usize = 8 << 20;

/// The address split for the request: the part `Url` takes, and the path with its query.
fn split(address: &str) -> Result<(Url, String), String> {
    let address = address.trim();
    let no_fragment = address.split('#').next().unwrap_or("");
    let scheme_end = no_fragment.find("://").map_or(0, |i| i + 3);
    let path_at = no_fragment[scheme_end..].find(['/', '?']).map_or(no_fragment.len(), |i| scheme_end + i);
    let url = Url::parse(&no_fragment[..path_at])?;
    let mut target = no_fragment[path_at..].to_string();
    if !target.starts_with('/') {
        target.insert(0, '/');
    }
    if target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err("the address has spaces or control characters in it".into());
    }
    Ok((url, target))
}

/// Where a redirect leads, from the page it came from.
fn joined(from: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    let scheme_end = from.find("://").map_or(0, |i| i + 3);
    let origin_end = from[scheme_end..].find('/').map_or(from.len(), |i| scheme_end + i);
    if location.starts_with('/') {
        return format!("{}{location}", &from[..origin_end]);
    }
    let dir_end = from.rfind('/').filter(|i| *i >= origin_end).map_or(origin_end, |i| i);
    format!("{}/{location}", &from[..dir_end])
}

fn decode(entity: &str) -> Option<char> {
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "nbsp" => ' ',
        e if e.starts_with("#x") => char::from_u32(u32::from_str_radix(&e[2..], 16).ok()?)?,
        e if e.starts_with('#') => char::from_u32(e[1..].parse().ok()?)?,
        _ => return None,
    })
}

/// A page's text: scripts and styles go, tags become breaks where they end a block, entities are read, blank
/// runs shrink.
pub fn text_of_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    let b = html.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            let rest = &lower[i..];
            for skip in ["script", "style", "noscript", "svg"] {
                if rest.starts_with(&format!("<{skip}")) {
                    let close = format!("</{skip}");
                    i = rest.find(&close).map_or(b.len(), |e| i + e + close.len());
                    break;
                }
            }
            let end = lower[i.min(b.len())..].find('>').map_or(b.len(), |e| i + e + 1);
            let tag = &lower[i.min(b.len())..end.min(b.len())];
            if ["<br", "<p", "</p", "<div", "</div", "<li", "<tr", "<h1", "<h2", "<h3", "<h4", "</h", "<table", "</table"].iter().any(|t| tag.starts_with(t)) {
                out.push('\n');
            }
            i = end;
            continue;
        }
        if b[i] == b'&'
            && let Some(semi) = html[i..].find(';').filter(|s| *s <= 10)
            && let Some(c) = decode(&html[i + 1..i + semi])
        {
            out.push(c);
            i += semi + 1;
            continue;
        }
        let c = html[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    let mut clean = String::new();
    for line in out.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            if !clean.ends_with("\n\n") && !clean.is_empty() {
                clean.push('\n');
            }
            continue;
        }
        clean.push_str(&words.join(" "));
        clean.push('\n');
    }
    clean.trim().to_string()
}

pub fn fetch(args: &Json, stop: &dyn Fn() -> bool) -> Result<String, String> {
    let mut address = args.str("url").unwrap_or("").trim().to_string();
    let budget = crate::limits::budget().min(PAGE_MAX);
    let deadline = Instant::now() + WAIT;
    for _ in 0..=REDIRECTS {
        let (url, target) = split(&address)?;
        let ctl = Ctl { deadline, budget, cancel: stop };
        let addrs = conn::resolve(&url, &ctl).map_err(|e| format!("{address}: {e:?}"))?;
        let c = Conn::open(&url, &addrs, &ctl).map_err(|e| format!("{address}: {e:?}"))?;
        let headers = vec![("User-Agent".to_string(), "BitsOS-assist".to_string()), ("Accept".to_string(), "text/html, text/plain, application/json, */*".to_string())];
        let mut resp = http::get(c, &url, &target, &headers, &ctl).map_err(|e| format!("{address}: {e:?}"))?;
        if (300..400).contains(&resp.status)
            && let Some(loc) = resp.header("location")
        {
            address = joined(&address, loc);
            continue;
        }
        let ctype = resp.header("content-type").unwrap_or("").to_ascii_lowercase();
        let body = resp.read_all(&ctl).map_err(|e| format!("{address}: {e:?}"))?;
        let text = String::from_utf8_lossy(&body);
        let shown = if ctype.contains("html") || text.trim_start().starts_with('<') { text_of_html(&text) } else { text.into_owned() };
        if !ctype.is_empty() && !(ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") || ctype.contains("javascript")) {
            return Ok(format!("HTTP {} from {address}: {ctype}, {} bytes; not a text page", resp.status, body.len()));
        }
        return Ok(format!("HTTP {} from {address}\n\n{}", resp.status, cap(&shown, OUT_MAX)));
    }
    Err(format!("more than {REDIRECTS} redirects, the last to {address}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_redirects_and_markup() {
        let (u, t) = split("https://example.org/suche?q=Georg#oben").unwrap();
        assert_eq!((u.host.as_str(), u.port, t.as_str()), ("example.org", 443, "/suche?q=Georg"));
        let (u, t) = split("http://localhost:8080").unwrap();
        assert_eq!((u.port, t.as_str()), (8080, "/"));
        assert!(split("ftp://x").is_err() && split("https://a.b/x y").is_err());
        assert_eq!(joined("https://a.b/c/d", "/e"), "https://a.b/e");
        assert_eq!(joined("https://a.b/c/d", "e"), "https://a.b/c/e");
        assert_eq!(joined("https://a.b", "e"), "https://a.b/e");
        assert_eq!(joined("https://a.b/c", "http://x.y/z"), "http://x.y/z");
        let html = "<html><head><style>p{x:y}</style><script>alert(1)</script></head><body><h1>Titel</h1><p>Erste &amp; zweite&#33;</p><p>Gr&uuml;&szlig;e</p></body></html>";
        assert_eq!(text_of_html(html), "Titel\n\nErste & zweite!\n\nGr&uuml;&szlig;e");
    }

    #[test]
    fn a_page_from_a_local_server() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for (n, s) in l.incoming().enumerate().take(2) {
                let mut s = s.unwrap();
                let mut buf = [0u8; 2048];
                let _ = std::io::Read::read(&mut s, &mut buf);
                let answer = if n == 0 { "HTTP/1.1 302 Found\r\nLocation: /seite\r\nContent-Length: 0\r\n\r\n".to_string() } else { "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 27\r\n\r\n<p>Hallo</p><p>Georg</p>   ".to_string() };
                std::io::Write::write_all(&mut s, answer.as_bytes()).unwrap();
            }
        });
        let r = fetch(&crate::json::parse(&format!(r#"{{"url":"http://127.0.0.1:{port}/"}}"#)).unwrap(), &|| false).unwrap();
        assert_eq!(r, format!("HTTP 200 from http://127.0.0.1:{port}/seite\n\nHallo\n\nGeorg"));
    }
}
