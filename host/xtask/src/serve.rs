//! `xtask serve`: a tiny static GET server for the lane's port range (guest downloads in scenario tests).
use crate::refuse;
use crate::util::*;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// Maps a request target to a file below `dir`; None for anything that could leave it.
pub fn map_path(dir: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next()?;
    let dec = percent_decode(path)?;
    let mut p = dir.to_path_buf();
    for seg in dec.split('/') {
        match seg {
            "" | "." => {}
            ".." => return None,
            s if s.contains('\\') || s.contains('\0') => return None,
            s => p.push(s),
        }
    }
    Some(p)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn mime(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "log" | "md" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "js" => "text/javascript",
        "css" => "text/css",
        _ => "application/octet-stream",
    }
}

fn respond(s: &mut TcpStream, code: u16, reason: &str, ctype: &str, body: &[u8], head_only: bool) {
    let h = format!("HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(h.as_bytes());
    if !head_only {
        let _ = s.write_all(body);
    }
}

fn handle(mut s: TcpStream, dir: &Path) {
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut line = String::new();
    if BufReader::new(&s).read_line(&mut line).is_err() {
        return;
    }
    let mut it = line.split_whitespace();
    let (method, target) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    let (code, size) = match (method, map_path(dir, target)) {
        ("GET" | "HEAD", Some(mut p)) => {
            if p.is_dir() {
                p.push("index.html");
            }
            match std::fs::read(&p) {
                Ok(body) => {
                    respond(&mut s, 200, "OK", mime(&p), &body, method == "HEAD");
                    (200, body.len())
                }
                Err(_) => {
                    respond(&mut s, 404, "Not Found", "text/plain", b"not found\n", method == "HEAD");
                    (404, 0)
                }
            }
        }
        ("GET" | "HEAD", None) => {
            respond(&mut s, 400, "Bad Request", "text/plain", b"bad path\n", false);
            (400, 0)
        }
        _ => {
            respond(&mut s, 405, "Method Not Allowed", "text/plain", b"GET only\n", false);
            (405, 0)
        }
    };
    println!("serve: {peer} {method} {target} -> {code} ({size} bytes)");
}

pub fn cmd_serve(root: &Path, mut a: Args) -> Result<(), String> {
    let dir = PathBuf::from(a.value("--dir")?.ok_or("serve needs --dir D")?);
    let port: u32 = a.num("--port")?.ok_or("serve needs --port P")?;
    a.positionals()?;
    let lane = lane(root);
    refuse::check_port(&lane, port, "serve --port").map_err(|r| r.to_string())?;
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let l = TcpListener::bind(("0.0.0.0", port as u16)).map_err(|e| format!("bind 0.0.0.0:{port}: {e}"))?;
    println!("serve: {} on 0.0.0.0:{port} (lane {lane})", dir.display());
    for c in l.incoming().flatten() {
        let d = dir.clone();
        std::thread::spawn(move || handle(c, &d));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_cannot_escape() {
        let d = Path::new("/srv/x");
        assert_eq!(map_path(d, "/a/b.txt?x=1"), Some(PathBuf::from("/srv/x/a/b.txt")));
        assert_eq!(map_path(d, "/"), Some(PathBuf::from("/srv/x")));
        assert_eq!(map_path(d, "/../etc/passwd"), None);
        assert_eq!(map_path(d, "/%2e%2e/etc"), None);
        assert_eq!(map_path(d, "/a%5C..%5Cb"), None);
        assert_eq!(map_path(d, "/bad%zz"), None);
    }

    #[test]
    fn serves_a_file_and_404() {
        use std::io::Read;
        let base = std::env::temp_dir().join(format!("xt-serve-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("hi.txt"), "hello").unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let d = base.clone();
        let t = std::thread::spawn(move || {
            for _ in 0..2 {
                let (c, _) = l.accept().unwrap();
                handle(c, &d);
            }
        });
        for (path, want) in [("/hi.txt", "200 OK"), ("/nope", "404")] {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(c, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut r = String::new();
            c.read_to_string(&mut r).unwrap();
            assert!(r.contains(want), "{r}");
            if want.starts_with("200") {
                assert!(r.ends_with("hello"));
            }
        }
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}
