//! `--max-size` end to end (bitspkg's bound against a hostile mirror, PK-6a): a local HTTP server on 127.0.0.1
//! in lane PK's port range answers once; a Content-Length above the bound and a chunked body that grows past it
//! both end with exit 12 and leave nothing in the output folder; a body inside the bound is kept.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

fn listener() -> (TcpListener, u16) {
    // Lane PK's ports (18500-18999); the top of the range, away from the QEMU scenarios.
    for port in 18900..19000 {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            return (l, port);
        }
    }
    panic!("no free port in 18900-18999");
}

fn serve_once(l: TcpListener, response: Vec<u8>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = l.accept() {
            s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut req = [0u8; 4096];
            let _ = s.read(&mut req);
            let _ = s.write_all(&response);
        }
    })
}

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(name: &str, response: Vec<u8>) -> (Option<i32>, Vec<String>) {
    let (l, port) = listener();
    let t = serve_once(l, response);
    let d = dir(name);
    let st = Command::new(env!("CARGO_BIN_EXE_fetch"))
        .args(["-q", "--timeout", "5", "--max-time", "20", "--max-size", "1000", "-o"])
        .arg(d.join("out"))
        .arg(format!("http://127.0.0.1:{port}/db"))
        .status()
        .unwrap();
    t.join().unwrap();
    let left = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    (st.code(), left)
}

#[test]
fn max_size_refuses_long_and_chunked_bodies_and_keeps_nothing() {
    let mut big = b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n".to_vec();
    big.extend([b'x'; 5000]);
    assert_eq!(run("fetch-size-length", big), (Some(12), vec![]));

    let mut chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for _ in 0..10 {
        chunked.extend(b"200\r\n");
        chunked.extend([b'y'; 0x200]);
        chunked.extend(b"\r\n");
    }
    chunked.extend(b"0\r\n\r\n");
    assert_eq!(run("fetch-size-chunked", chunked), (Some(12), vec![]));

    let mut small = b"HTTP/1.1 200 OK\r\nContent-Length: 600\r\nConnection: close\r\n\r\n".to_vec();
    small.extend([b'z'; 600]);
    assert_eq!(run("fetch-size-ok", small), (Some(0), vec!["out".to_string()]));
}
