//! RB-6 CR1 (taken over in MC-4b): `mcp.answered` counts only answers to requests that passed the token and
//! Origin checks, so a peer without the token cannot make a new generation's kernel confirm its own boot. Its own
//! test binary: the counter is global and no other test may answer requests meanwhile.
mod common;
use common::*;

fn answered() -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == "mcp.answered").map(|(_, v)| v).expect("mcp.answered is registered")
}
fn requests() -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == "mcp.requests").map(|(_, v)| v).expect("mcp.requests is registered")
}

#[test]
fn answered_counts_only_authenticated_answers() {
    let (p, srv) = serve(|_| {});
    let (a0, r0) = (answered(), requests());
    // A wrong token, no token, a foreign Origin: answered by HTTP (401/403) but never counted as answered.
    for bad in [
        Req { token: Some("wrong-token"), ..Default::default() }.text(),
        Req { token: None, ..Default::default() }.text(),
        Req { headers: vec!["Origin: http://evil.example".into()], ..Default::default() }.text(),
    ] {
        let (a, _) = ask(p, &bad, 5000).expect("an answer");
        assert!(a.status == 401 || a.status == 403, "{a:?}");
    }
    assert_eq!(answered(), a0, "a refused request counted as answered");
    assert!(requests() >= r0 + 3, "mcp.requests counts every request ({} -> {})", r0, requests());
    // Three requests with the token on one connection: each one counted once.
    let mut c = Client::new(p);
    for _ in 0..3 {
        c.write(req());
        let a = c.answer(5000).expect("an answer");
        assert_eq!(a.status, 200, "{a:?}");
    }
    c.close();
    assert!(until(2000, || answered() == a0 + 3), "answered {} -> {}, want +3", a0, answered());
    stop(srv);
}
