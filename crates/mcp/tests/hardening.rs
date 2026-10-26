//! MC hardening (A1): slow-loris peers (a head or body trickled in byte by byte never gets more time than
//! `slow_ns` from its first byte, and holds no one else up), a client that never reads its big answer, and text
//! answers above 4 MB (cut with a note instead of lost).
mod common;
use common::*;
use std::time::Instant;

fn slow(c: &mut mcp::ServerConfig) {
    c.slow_ns = 1_500_000_000;
}

#[test]
fn a_trickled_head_gets_no_more_time() {
    let (p, srv) = serve(slow);
    let full = req();
    let mut c = Client::new(p);
    let t0 = Instant::now();
    // One byte every 100 ms: each byte would keep a per-read timeout alive for ever.
    let mut sent = 0;
    while !c.ended() && sent < full.len() - 1 && t0.elapsed().as_millis() < 6000 {
        c.write(&full.as_bytes()[sent..sent + 1]);
        sent += 1;
        sleep(100);
        // Meanwhile another client is served at once.
        if sent == 5 {
            assert!(ask(p, &req(), 3000).unwrap().0.mcp_ok(), "a trickling peer holds no one up");
        }
    }
    let a = c.answer(3000).expect("an answer");
    let ms = t0.elapsed().as_millis();
    assert!(a.status == 408 && (1400..4000).contains(&ms), "408 at slow_ns from the first byte, not later ({ms} ms, {} of {} bytes)", sent, full.len());
    assert!(c.wait_ended(3000));
    c.close();
    stop(srv);
}

#[test]
fn a_trickled_body_gets_no_more_time() {
    let (p, srv) = serve(slow);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"pad":"..............................................."}}"#;
    let mut c = Client::new(p);
    let t0 = Instant::now();
    c.write(Req { body: None, headers: vec![format!("Content-Length: {}", body.len())], ..Default::default() }.text());
    for b in body.as_bytes().iter().take(body.len() - 1) {
        if c.ended() {
            break;
        }
        c.write([*b]);
        sleep(100);
    }
    let a = c.answer(3000).expect("an answer");
    let ms = t0.elapsed().as_millis();
    assert!(a.status == 408 && (1400..4000).contains(&ms), "{ms} ms: {}", a.status);
    c.close();
    stop(srv);
}

#[test]
fn a_client_that_never_reads_holds_no_one_up() {
    let (p, srv) = serve(|_| {});
    // Asks for 3.5 MB and reads nothing (the raw client reads in its thread; this one is a bare socket).
    let mut lazy = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
    std::io::Write::write_all(&mut lazy, tool_call(1, "t_text", r#"{"bytes":3500000}"#).as_bytes()).unwrap();
    sleep(300);
    let t = Instant::now();
    for i in 0..5 {
        let (a, _) = ask(p, &tool_call(10 + i, "t_echo", r#"{"text":"here"}"#), 3000).expect("served");
        assert_eq!(texts(&a), "here");
    }
    assert!(t.elapsed().as_millis() < 3000, "{} ms", t.elapsed().as_millis());
    drop(lazy);
    stop(srv);
}

#[test]
fn text_above_4_mb_is_cut_not_lost() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    for (unit, n) in [("x", 5_000_000), ("\u{1}", 1_000_000), ("ä", 4_400_000)] {
        c.write(tool_call(1, "t_text", &format!(r#"{{"bytes":{n},"unit":"{}"}}"#, unit.chars().map(|ch| format!("\\u{:04x}", ch as u32)).collect::<String>())));
        let a = c.answer(30_000).expect("an answer");
        let t = texts(&a);
        assert!(!is_error(&a) && a.body.len() <= mcp::rpc::ANSWER_MAX, "{unit:?}: {} bytes", a.body.len());
        assert!(t.starts_with(&unit.repeat(1000)) && t.contains("[... cut: the last "), "{unit:?}");
        assert!(a.mcp_ok() && !c.ended(), "the connection goes on");
    }
    assert!(events("answer.cut").len() >= 3);
    c.close();
    stop(srv);
}
