//! V1's transport cases (BitsOS-Plattform/mcp-http-test.mjs, boot 1) against the host build on 127.0.0.1.
//! Changed from V1 only where V1 was single-threaded: a second connection no longer waits while the first has
//! half a request in (each connection has its own thread). Timing: lower bounds only, upper bounds generous.
mod common;
use common::*;
use std::time::Instant;

#[test]
fn status_codes_and_closing() {
    let (p, srv) = serve(|_| {});
    let (a, closed) = ask(p, &Req { token: None, ..Default::default() }.text(), 5000).unwrap();
    assert!(a.status == 401 && a.header("www-authenticate").as_deref() == Some("Bearer") && a.header("content-length").as_deref() == Some("0") && closed, "{a:?}");
    for t in ["test-token-1235", "test-token-1234x", "est-token-1234", "test-token-123", "t", "TEST-TOKEN-1234"] {
        assert_eq!(ask(p, &Req { token: Some(t), ..Default::default() }.text(), 5000).unwrap().0.status, 401, "{t}");
    }
    // A refused request's body must not be read as the next request.
    let mut c = Client::new(p);
    c.write(Req { token: None, body: Some("GET /x HTTP/1.1\r\nHost: b\r\n\r\n"), ..Default::default() }.text());
    assert_eq!(c.answer(5000).unwrap().status, 401);
    assert!(c.wait_ended(5000) && c.answer(100).is_none());
    let blanks = Req { token: None, headers: vec![format!("authorization:   bEaReR {TOKEN}  ")], ..Default::default() };
    assert!(ask(p, &blanks.text(), 5000).unwrap().0.mcp_ok(), "scheme in any case, blanks around");
    assert_eq!(ask(p, &Req { token: None, headers: vec![format!("Authorization: Basic {TOKEN}")], ..Default::default() }.text(), 5000).unwrap().0.status, 401);
    assert!(ask(p, &req(), 5000).unwrap().0.mcp_ok());
    let (a, closed) = ask(p, &Req { path: "/other", ..Default::default() }.text(), 5000).unwrap();
    assert!(a.status == 404 && closed);
    assert_eq!(ask(p, &Req { path: "/mcp/", ..Default::default() }.text(), 5000).unwrap().0.status, 404);
    for m in ["GET", "DELETE"] {
        let (a, closed) = ask(p, &Req { method: m, body: None, ..Default::default() }.text(), 5000).unwrap();
        assert!(a.status == 405 && a.header("allow").as_deref() == Some("POST") && closed, "{m}");
    }
    let (a, closed) = ask(p, &Req { headers: vec!["Origin: http://evil.example".into()], ..Default::default() }.text(), 5000).unwrap();
    assert!(a.status == 403 && closed);
    let (a, closed) = ask(p, &format!("{}5\r\nhello\r\n0\r\n\r\n", Req { body: None, headers: vec!["Transfer-Encoding: chunked".into()], ..Default::default() }.text()), 5000).unwrap();
    assert!(a.status == 411 && closed);
    assert_eq!(ask(p, &Req { body: None, ..Default::default() }.text(), 5000).unwrap().0.status, 411);
    for l in ["262145", "18446744073709551621"] {
        let (a, closed) = ask(p, &Req { body: None, headers: vec![format!("Content-Length: {l}")], ..Default::default() }.text(), 5000).unwrap();
        assert!(a.status == 413 && closed, "{l}");
    }
    let shell = format!("{},\"params\":{{\"pad\":\"\"}}}}", &PING[..PING.len() - 1]);
    let full = shell.replace("\"\"", &format!("\"{}\"", "x".repeat(262_144 - shell.len())));
    assert_eq!(full.len(), 262_144);
    assert!(ask(p, &rpc(&full), 30_000).unwrap().0.mcp_ok(), "a body of exactly 256 KB is taken");
    let (a, closed) = ask(p, &Req { headers: vec![format!("X-Long: {}", "a".repeat(17_000))], ..Default::default() }.text(), 5000).unwrap();
    assert!(a.status == 431 && closed);
    let bad = [
        Req { headers: vec!["No colon here".into()], ..Default::default() }.text(),
        Req { headers: vec!["X-Name : 1".into()], ..Default::default() }.text(),
        Req { body: None, headers: vec!["Content-Length: 12a".into()], ..Default::default() }.text(),
        Req { headers: vec!["Content-Length: 5".into()], ..Default::default() }.text(),
        "POST\r\n\r\n".into(),
    ];
    for b in bad {
        let (a, closed) = ask(p, &b, 5000).unwrap();
        assert!(a.status == 400 && closed, "{b:?}");
    }
    stop(srv);
}

#[test]
fn one_log_event_per_request_with_the_path_cut() {
    let (p, srv) = serve(|_| {});
    let path = format!("/{}", "q".repeat(2000));
    let (a, _) = ask(p, &Req { path: &path, ..Default::default() }.text(), 5000).unwrap();
    assert_eq!(a.status, 404);
    assert!(ask(p, &req(), 5000).unwrap().0.mcp_ok(), "then MCP as usual");
    let ev = request_events(&format!("POST /{}", "q".repeat(54)));
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert!(ev[0].contains("status=404") && ev[0].contains("bytes=0") && !ev[0].contains(&"q".repeat(55)), "{}", ev[0]);
    let call = rpc(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"t_echo","arguments":{"text":"log-marker-77"}}}"#);
    let mut c = Client::new(p);
    c.write(call);
    let a = c.answer(5000).unwrap();
    assert!(a.mcp_ok());
    c.close();
    assert!(until(2000, || events("http.request").iter().any(|e| e.contains("rpc=tools/call tool=t_echo") && e.contains(&format!("bytes={}", a.body.len())))));
    assert!(events("http.request").iter().all(|e| !e.contains(TOKEN)), "the token is never logged");
    stop(srv);
}

#[test]
fn ways_a_request_arrives() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    for b in req().bytes() {
        c.write([b]);
        sleep(1);
    }
    assert!(c.answer(15_000).unwrap().mcp_ok(), "a request in 1-byte writes");
    c.write(Req { eol: "\n", ..Default::default() }.text());
    assert!(c.answer(5000).unwrap().mcp_ok(), "LF line ends");
    c.write(format!("\r\n{}", req()));
    assert!(c.answer(5000).unwrap().mcp_ok(), "an empty line before a request");
    c.write(format!("{}{}{}", req(), req(), req()));
    for _ in 0..3 {
        assert!(c.answer(5000).unwrap().mcp_ok(), "pipelining");
    }
    assert!(!c.ended());
    c.write(Req { headers: vec!["Connection: close".into()], ..Default::default() }.text());
    let a = c.answer(5000).unwrap();
    assert!(a.status == 200 && a.closes() && c.wait_ended(5000), "Connection: close");
    c.close();
    let mut c = Client::new(p);
    c.write(format!("{}{}", Req { path: "/x", body: None, ..Default::default() }.text(), req()));
    let a = c.answer(5000).unwrap();
    assert!(a.status == 404 && a.closes() && c.wait_ended(5000) && c.answer(100).is_none(), "refused among several: then closed");
    c.close();
    let (a, closed) = ask(p, &Req { version: "1.0", ..Default::default() }.text(), 5000).unwrap();
    assert!(a.status == 200 && closed, "HTTP/1.0 closes after the answer");
    stop(srv);
}

#[test]
fn connections_side_by_side() {
    let (p, srv) = serve(|_| {});
    let (mut a, mut b) = (Client::new(p), Client::new(p));
    let half = req();
    a.write(&half[..30]);
    sleep(300);
    b.write(req());
    let rb = b.answer(5000).expect("V2: the second connection is served while the first has half a request in");
    a.write(&half[30..]);
    let ra = a.answer(5000).unwrap();
    assert!(ra.mcp_ok() && rb.mcp_ok() && !a.ended());
    a.close();
    b.close();
    // A peer without the token sending 50 requests cannot hold the server.
    let mut pipe = Client::new(p);
    let get = Req { method: "GET", token: None, body: None, ..Default::default() }.text();
    pipe.write(&get[..30]);
    sleep(300);
    let mut c = Client::new(p);
    c.write(req());
    sleep(300);
    let t = Instant::now();
    pipe.write(format!("{}{}", &get[30..], get.repeat(49)));
    let r = c.answer(5000).unwrap();
    assert!(r.mcp_ok() && r.at.duration_since(t).as_millis() < 1000);
    assert_eq!(pipe.answer(2000).unwrap().status, 401);
    assert!(pipe.wait_ended(2000) && pipe.answer(100).is_none(), "one 401, then closed");
    pipe.close();
    c.close();
    let fresh = Client::new(p);
    sleep(200);
    let mut c = Client::new(p);
    c.write(req());
    sleep(200);
    let mut fresh = fresh;
    fresh.write(req());
    assert!(fresh.answer(5000).unwrap().mcp_ok() && c.answer(5000).unwrap().mcp_ok());
    fresh.close();
    c.close();
    stop(srv);
}

#[test]
fn the_idlest_gives_way_to_a_knocking_peer() {
    let (p, srv) = serve(|c| c.conns = 3);
    let t_first = Instant::now();
    let silents: Vec<Client> = (0..3)
        .map(|_| {
            let c = Client::new(p);
            sleep(200);
            c
        })
        .collect();
    assert!(until(2000, || srv.connections() == 3));
    let mut c = Client::new(p);
    c.write(req());
    let r = c.answer(5000).unwrap();
    assert!(r.mcp_ok());
    assert!(r.at.duration_since(t_first).as_millis() >= 900, "a silent new connection keeps its place for 1 s");
    assert!(silents[0].wait_ended(2000), "the oldest silent one gave way");
    sleep(300);
    assert_eq!(silents.iter().filter(|s| s.ended()).count(), 1, "only one");
    assert!(events("conn.evicted").iter().any(|e| e.contains("for_peer=127.0.0.1")));
    for s in silents {
        s.close();
    }
    c.close();
    // When nothing may give way (heads accepted, bodies coming) the new connection waits for a place.
    let full = req();
    let cut = full.len() - 5;
    let mut busy: Vec<Client> = (0..3).map(|_| Client::new(p)).collect();
    for b in busy.iter_mut() {
        b.write(&full[..cut]);
    }
    sleep(1200);
    let mut late = Client::new(p);
    late.write(req());
    assert!(late.answer(500).is_none(), "no connection may give way");
    for b in busy.iter_mut() {
        b.write(&full[cut..]);
        assert!(b.answer(5000).unwrap().mcp_ok(), "a busy connection is never evicted");
    }
    assert!(late.answer(5000).is_some_and(|a| a.mcp_ok()), "taken once the others went idle");
    for b in busy {
        b.close();
    }
    late.close();
    // ... but not for ever: after full_ns it is closed.
    let mut busy: Vec<Client> = (0..3).map(|_| Client::new(p)).collect();
    for b in busy.iter_mut() {
        b.write(&full[..cut]);
    }
    sleep(300);
    let t = Instant::now();
    let late = Client::new(p);
    assert!(late.wait_ended(8000), "closed after full_ns");
    assert!(t.elapsed().as_millis() >= 1800, "waited {} ms", t.elapsed().as_millis());
    assert!(events("conn.full").iter().any(|e| e.contains("waited_ms=")));
    for b in busy {
        b.close();
    }
    stop(srv);
}

#[test]
fn a_peer_without_the_token_cannot_hold_the_places() {
    let (p, srv) = serve(|c| c.conns = 3);
    // One byte on every place: heads that never finish (V1's slow-loris case for the evict rule).
    let mut holders: Vec<Client> = (0..3).map(|_| Client::new(p)).collect();
    for h in holders.iter_mut() {
        h.write("P");
        sleep(50);
    }
    assert!(until(2000, || srv.connections() == 3));
    let t = Instant::now();
    let mut c = Client::new(p);
    c.write(req());
    let r = c.answer(5000).expect("a real client gets a place");
    assert!(r.mcp_ok());
    let ms = r.at.duration_since(t).as_millis();
    assert!((700..4000).contains(&ms), "a half head keeps its place for first_ns, then gives way ({ms} ms)");
    assert!(holders[0].wait_ended(2000), "the oldest half head gave way");
    assert!(events("conn.evicted").iter().any(|e| e.contains("state=head")));
    // An idle connection gives way before a half head does.
    let mut next = Client::new(p);
    assert!(c.wait_ended(3000), "the idle connection gave way");
    assert!(!holders[1].ended() && !holders[2].ended(), "the half heads stay while something is idle");
    next.write(req());
    assert!(next.answer(5000).unwrap().mcp_ok());
    c.close();
    next.close();
    for h in holders {
        h.close();
    }
    stop(srv);
}

#[test]
fn hostile_heads_over_the_socket() {
    let (p, srv) = serve(|_| {});
    // Conflicting lengths (request smuggling on a pipelined keep-alive connection): 400, closed, the rest never
    // read as a request.
    let mut c = Client::new(p);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    c.write(format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\nContent-Length: 5\r\n\r\n{body}{}", body.len(), req()));
    let a = c.answer(5000).unwrap();
    assert!(a.status == 400 && a.closes() && c.wait_ended(5000) && c.answer(100).is_none(), "{a:?}");
    for bad in ["Content-Length: -1", "Content-Length: 1e3", "X-A: 1\r\n folded", "X-\0: 1"] {
        let (a, closed) = ask(p, &Req { body: None, headers: vec![bad.into()], ..Default::default() }.text(), 5000).unwrap();
        assert!(a.status == 400 && closed, "{bad:?}");
    }
    // A Connection list with close in it closes.
    let (a, closed) = ask(p, &Req { headers: vec!["Connection: keep-alive, close".into()], ..Default::default() }.text(), 5000).unwrap();
    assert!(a.status == 200 && closed);
    // Expect: 100-continue gets its interim answer before the body is sent.
    let mut c = Client::new(p);
    let full = Req { headers: vec!["Expect: 100-continue".into()], ..Default::default() }.text();
    let split = full.find("\r\n\r\n").unwrap() + 4;
    c.write(&full[..split]);
    let interim = c.answer(5000).unwrap();
    assert!(interim.status == 100 && interim.body.is_empty(), "{interim:?}");
    c.write(&full[split..]);
    assert!(c.answer(5000).unwrap().mcp_ok());
    c.close();
    let (a, closed) = ask(p, &Req { headers: vec!["Expect: tea".into()], ..Default::default() }.text(), 5000).unwrap();
    assert!(a.status == 417 && closed);
    stop(srv);
}

#[test]
fn idle_and_slow_request_timeouts() {
    let (p, srv) = serve(|c| {
        c.idle_ns = 1_500_000_000;
        c.slow_ns = 2_000_000_000;
    });
    let mut c = Client::new(p);
    c.write(req());
    assert!(c.answer(5000).unwrap().mcp_ok());
    let t0 = Instant::now();
    assert!(c.wait_ended(10_000));
    assert!(t0.elapsed().as_millis() >= 1400, "idle close after idle_ns ({} ms)", t0.elapsed().as_millis());
    c.close();
    let mut c = Client::new(p);
    let t0 = Instant::now();
    c.write("POST /mcp HTTP/1.1\r\nHost: bitsos\r\n");
    let a = c.answer(10_000).unwrap();
    assert!(a.status == 408 && t0.elapsed().as_millis() >= 1900 && c.wait_ended(5000), "408 after slow_ns");
    c.close();
    // A body that never completes is the same 408.
    let mut c = Client::new(p);
    c.write(Req { body: None, headers: vec!["Content-Length: 50".into()], ..Default::default() }.text() + "{");
    assert_eq!(c.answer(10_000).unwrap().status, 408);
    // One event per request, also for a 408: the request line and how much had come.
    let ev = events("http.request");
    let timeouts: Vec<&String> = ev.iter().filter(|e| e.contains("status=408")).collect();
    assert!(timeouts.len() >= 2 && timeouts.iter().all(|e| e.contains("what=\"POST /mcp\"") && e.contains("got=")), "{timeouts:?}");
    assert!(events("http.timeout").is_empty(), "no second event for a 408");
    stop(srv);
}

#[test]
fn stop_ends_the_server() {
    let (p, srv) = serve(|_| {});
    let mut kept = Client::new(p);
    kept.write(req());
    assert!(kept.answer(5000).unwrap().mcp_ok());
    srv.stop();
    assert!(srv.wait_stopped(5_000_000_000));
    assert!(kept.wait_ended(2000), "the connection kept alive is closed");
    assert!(std::net::TcpStream::connect(("127.0.0.1", p)).is_err(), "nothing listens any more");
    let again = start(p, |_| {});
    assert!(ask(p, &req(), 5000).unwrap().0.mcp_ok(), "and it starts again on the same port");
    stop(again);
}
