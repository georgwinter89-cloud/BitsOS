//! Host tests of the HTTP client: URL and head parsing, the body decoder fed in every split, and whole GETs
//! against small std servers over `host::StdSockets` (lengths, chunked, until-close, errors, limits, timeouts).

extern crate std;
use super::*;
use crate::host::StdSockets;
use std::io::{Read, Write};
use std::thread;

#[test]
fn urls() {
    let u = parse_url("http://192.168.100.20:8770/kernel.img?x=1#frag").unwrap();
    assert_eq!(u.addr, SockAddr { ip: Ipv4([192, 168, 100, 20]), port: 8770 });
    assert_eq!(u.authority, "192.168.100.20:8770");
    assert_eq!(u.path, "/kernel.img?x=1");
    let u = parse_url("HTTP://10.0.2.2").unwrap();
    assert_eq!((u.addr.port, u.path), (80, "/"));
    assert!(request(&u).starts_with("GET / HTTP/1.1\r\nHost: 10.0.2.2\r\n"));
    assert_eq!(parse_url("https://1.2.3.4/"), Err(HttpError::Https));
    assert_eq!(parse_url("http://example.com/"), Err(HttpError::NeedsAddress));
    for bad in ["10.0.2.2/x", "ftp://1.2.3.4/", "http://", "http://1.2.3.4:0/", "http://1.2.3.4:99999/", "http://u@1.2.3.4/",
        "http://1.2.3.4/a b", "http://1.2.3.4/a\r\nX: y", "http://1.2.3.256/", "http://1.2.3/", "http://[::1]/", "http://1.2.3.4?q"] {
        assert!(matches!(parse_url(bad), Err(HttpError::BadUrl(_)) | Err(HttpError::NeedsAddress)), "{bad}");
    }
}

#[test]
fn heads() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nX-A:  b \r\n\r\nbody";
    let end = head_end(raw).unwrap();
    assert_eq!(&raw[end..], b"body");
    let h = parse_head(&raw[..end]).unwrap();
    assert_eq!((h.status, h.content_length, h.chunked), (200, Some(12), false));
    assert_eq!(h.header("x-a"), Some("b"));
    let lf = b"HTTP/1.0 404 Not Found\nTransfer-Encoding: Chunked\n\nrest";
    let end = head_end(lf).unwrap();
    assert_eq!(&lf[end..], b"rest");
    let h = parse_head(&lf[..end]).unwrap();
    assert!(h.chunked && h.status == 404);
    assert_eq!(head_end(b"HTTP/1.1 200 OK\r\nA: b\r\n"), None);
    for bad in [&b"HTTP/2 200 OK\r\n\r\n"[..], b"HTTP/1.1 20 OK\r\n\r\n", b"HTTP/1.1 200 OK\r\nNoColon\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n", b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999\r\n\r\n", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nBad Name: x\r\n\r\n", b"\xff\xfe"] {
        assert!(parse_head(bad).is_err(), "{:?}", std::string::String::from_utf8_lossy(bad));
    }
    let same = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\ncontent-length: 3\r\n\r\n").unwrap();
    assert_eq!(same.content_length, Some(3));
}

fn decode(h: &Head, limit: u64, input: &[u8], split: usize) -> Result<(Vec<u8>, usize), HttpError> {
    let mut d = Decoder::new(h, limit)?;
    let mut got = Vec::new();
    let mut used = 0;
    for piece in input.chunks(split.max(1)) {
        if d.done() {
            break;
        }
        used += d.feed(piece, &mut |x| {
            got.extend_from_slice(x);
            Ok(())
        })?;
    }
    if !d.done() {
        d.eof()?;
    }
    Ok((got, used))
}

fn head(chunked: bool, len: Option<u64>) -> Head {
    Head { status: 200, headers: Vec::new(), content_length: len, chunked }
}

#[test]
fn chunked_bodies_in_every_split() {
    let wire = b"4;name=x\r\nWiki\r\n5\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\nTrailer: yes\r\n\r\nNEXT";
    for split in 1..wire.len() {
        let (body, used) = decode(&head(true, None), 1000, wire, split).unwrap();
        assert_eq!(body, b"Wikipedia in\r\n\r\nchunks.", "split {split}");
        assert_eq!(&wire[used..], b"NEXT", "split {split}");
    }
    // LF only, upper-case hex, no trailer.
    let (b, _) = decode(&head(true, None), 100, b"A\nabcdefghij\n0\n\n", 3).unwrap();
    assert_eq!(b, b"abcdefghij");
}

#[test]
fn broken_and_hostile_chunks() {
    let h = head(true, None);
    for bad in [&b"\r\n"[..], b"zz\r\n", b"4\r\nWikiX\r\n", b"11111111111111111\r\n", b"4\r\nWi"] {
        assert!(decode(&h, 1000, bad, 1).is_err(), "{:?}", std::string::String::from_utf8_lossy(bad));
    }
    let long_ext = [b"1;".as_slice(), &[b'x'; 5000]].concat();
    assert!(matches!(decode(&h, 1000, &long_ext, 7), Err(HttpError::BadResponse(_))));
    assert_eq!(decode(&h, 8, b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n", 4).err(), Some(HttpError::TooLarge { limit: 8 }));
}

#[test]
fn length_and_until_close() {
    let (b, used) = decode(&head(false, Some(5)), 10, b"helloEXTRA", 2).unwrap();
    assert_eq!((b.as_slice(), used), (&b"hello"[..], 5));
    assert_eq!(Decoder::new(&head(false, Some(11)), 10).err(), Some(HttpError::TooLarge { limit: 10 }));
    assert!(decode(&head(false, Some(5)), 10, b"hel", 1).is_err(), "truncated");
    let (b, _) = decode(&head(false, None), 10, b"all of it", 4).unwrap();
    assert_eq!(b, b"all of it");
    assert_eq!(decode(&head(false, None), 4, b"all of it", 4).err(), Some(HttpError::TooLarge { limit: 4 }));
    let mut h204 = head(false, Some(100));
    h204.status = 204;
    assert_eq!(decode(&h204, 0, b"", 1).unwrap().0, b"");
}

// ---------------------------------------------------------------------------------------------------------
// Whole GETs against std servers

/// Serves one connection: reads the request head, then writes `answer` in pieces of `piece` bytes.
fn server(answer: Vec<u8>, piece: usize, hold_open_ms: u64) -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut req = Vec::new();
        let mut b = [0u8; 1024];
        while head_end(&req).is_none() {
            let n = s.read(&mut b).unwrap();
            if n == 0 {
                return;
            }
            req.extend_from_slice(&b[..n]);
        }
        assert!(req.starts_with(b"GET /file HTTP/1.1\r\nHost: 127.0.0.1:"));
        for p in answer.chunks(piece.max(1)) {
            if s.write_all(p).is_err() {
                return;
            }
        }
        thread::sleep(std::time::Duration::from_millis(hold_open_ms));
    });
    port
}

fn data(n: usize) -> Vec<u8> {
    let mut x: u32 = 0x1234_5678;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn chunked(body: &[u8], size: usize) -> Vec<u8> {
    let mut w = Vec::new();
    for c in body.chunks(size) {
        w.extend_from_slice(alloc::format!("{:x}\r\n", c.len()).as_bytes());
        w.extend_from_slice(c);
        w.extend_from_slice(b"\r\n");
    }
    w.extend_from_slice(b"0\r\n\r\n");
    w
}

fn url(port: u16) -> String {
    alloc::format!("http://127.0.0.1:{port}/file")
}

#[test]
fn get_content_length_chunked_and_close() {
    let body = data(1 << 20);
    let mut a = alloc::format!("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    a.extend_from_slice(&body);
    let (h, got) = get(&StdSockets, &url(server(a, 7000, 0)), &Options::new(2 << 20)).unwrap();
    assert_eq!(h.status, 200);
    assert!(got == body, "content-length body differs");

    let mut a = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    a.extend_from_slice(&chunked(&body, 3000));
    let (_, got) = get(&StdSockets, &url(server(a, 1000, 0)), &Options::new(2 << 20)).unwrap();
    assert!(got == body, "chunked body differs");

    let mut a = b"HTTP/1.0 200 OK\r\n\r\n".to_vec();
    a.extend_from_slice(&body[..5000]);
    let (_, got) = get(&StdSockets, &url(server(a, 999, 0)), &Options::new(2 << 20)).unwrap();
    assert!(got == body[..5000], "until-close body differs");
}

#[test]
fn get_errors() {
    let a = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nno!".to_vec();
    assert_eq!(get(&StdSockets, &url(server(a, 100, 0)), &Options::new(100)).err(), Some(HttpError::Status(404)));
    let a = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n".to_vec();
    assert_eq!(get(&StdSockets, &url(server(a, 100, 0)), &Options::new(999)).err(), Some(HttpError::TooLarge { limit: 999 }));
    let mut a = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    a.extend_from_slice(&chunked(&data(5000), 1000));
    assert_eq!(get(&StdSockets, &url(server(a, 100, 0)), &Options::new(4000)).err(), Some(HttpError::TooLarge { limit: 4000 }));
    let a = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort".to_vec();
    assert!(matches!(get(&StdSockets, &url(server(a, 100, 0)), &Options::new(100)), Err(HttpError::BadResponse(_))));
    // The server sends half and then nothing: the idle limit ends it.
    let a = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhalf".to_vec();
    let mut o = Options::new(100);
    o.idle_ns = 200_000_000;
    assert_eq!(get(&StdSockets, &url(server(a, 100, 2000)), &o).err(), Some(HttpError::TimedOut));
    // Nobody listens.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    assert!(matches!(get(&StdSockets, &url(port), &Options::new(1)), Err(HttpError::Connect(_))));
    // A sink that refuses.
    let a = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec();
    assert_eq!(get_to(&StdSockets, &url(server(a, 100, 0)), &Options::new(100), &mut |_| Err(())).err(), Some(HttpError::Sink));
    assert_eq!(get(&StdSockets, "http://bitsos.local/x", &Options::new(1)).err(), Some(HttpError::NeedsAddress));
}

/// RB-5a request 3: `get_to_with` shows the checked head before the first body byte; the caller can size its
/// memory from Content-Length, and a refusal there ends the transfer before any body is read.
#[test]
fn head_comes_before_the_body() {
    let body = data(300_000);
    let answer = || {
        let mut a = alloc::format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Kind: kernel\r\n\r\n", body.len()).into_bytes();
        a.extend_from_slice(&body);
        a
    };
    let order = core::cell::RefCell::new(Vec::new());
    let got = core::cell::RefCell::new(Vec::new());
    let h = get_to_with(
        &StdSockets,
        &url(server(answer(), 4096, 0)),
        &Options::new(1 << 20),
        &mut |h| {
            order.borrow_mut().push("head");
            assert_eq!((h.status, h.content_length, h.header("x-kind")), (200, Some(body.len() as u64), Some("kernel")));
            got.borrow_mut().try_reserve_exact(h.content_length.unwrap() as usize).map_err(|_| ())
        },
        &mut |d| {
            if order.borrow().last() != Some(&"body") {
                order.borrow_mut().push("body");
            }
            got.borrow_mut().extend_from_slice(d);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(h.status, 200);
    assert_eq!(*order.borrow(), ["head", "body"]);
    assert!(*got.borrow() == body);

    let mut body_calls = 0;
    let r = get_to_with(&StdSockets, &url(server(answer(), 4096, 0)), &Options::new(1 << 20), &mut |_| Err(()), &mut |_| {
        body_calls += 1;
        Ok(())
    });
    assert_eq!(r.err(), Some(HttpError::HeadRefused));
    assert_eq!(body_calls, 0, "no body after a refused head");

    // Not 2xx or too large: the head callback is not asked.
    let a = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nno!".to_vec();
    let r = get_to_with(&StdSockets, &url(server(a, 100, 0)), &Options::new(100), &mut |_| panic!("asked for a 404"), &mut |_| Ok(()));
    assert_eq!(r.err(), Some(HttpError::Status(404)));
    let a = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n".to_vec();
    let r = get_to_with(&StdSockets, &url(server(a, 100, 0)), &Options::new(999), &mut |_| panic!("asked for too large"), &mut |_| Ok(()));
    assert_eq!(r.err(), Some(HttpError::TooLarge { limit: 999 }));
}

/// `get` reserves Content-Length once; memory it cannot get is `NoMemory`, not a panic.
#[test]
fn get_without_memory_is_no_memory() {
    let a = b"HTTP/1.1 200 OK\r\nContent-Length: 4611686018427387904\r\n\r\n".to_vec();
    assert_eq!(get(&StdSockets, &url(server(a, 100, 0)), &Options::new(u64::MAX)).err(), Some(HttpError::NoMemory));
}
