//! bitsmcp over real sockets against a fake gate (the kernel's gate commands with two fake tools): the HTTP
//! rules, the token through `auth`, answers built by the kernel's own rpc code, dropped and installer tools,
//! `answered` after each answer, eviction of the idlest connection.

use bitsmcp::link::{EACCES, EPERM, Link, Pool};
use bitsmcp::serve::{self, Limits, Service};
use mcp_wire::gate::{self, Command, Listed};
use mcp_wire::{Json, Safety, ToolOutput};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "service-test-token";

#[derive(Default)]
struct Seen {
    answered: AtomicUsize,
    calls: Mutex<Vec<String>>,
}

struct FakeGate {
    authed: bool,
    seen: Arc<Seen>,
}

fn listed() -> Vec<Listed> {
    let t = |name: &str, schema: &str, safety, installer| Listed { name: name.into(), description: format!("{name} tool"), schema: schema.into(), owner: "MC".into(), safety, verified: true, pid: None, service: false, installer };
    vec![t("hw_info", r#"{"type":"object","properties":{"section":{"type":"string"}}}"#, Safety::Read, false), t("reload", r#"{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}"#, Safety::Change, true)]
}

impl Link for FakeGate {
    fn command(&mut self, line: &[u8]) -> Result<String, i32> {
        match gate::parse_command(line).ok_or(22)? {
            Command::List { .. } => Ok(gate::encode_list_answer(&listed())),
            Command::Auth { token, .. } => {
                self.authed = token == TOKEN.as_bytes();
                if self.authed { Ok("ok\n".into()) } else { Err(EACCES) }
            }
            Command::Answered if self.authed => {
                self.authed = false;
                self.seen.answered.fetch_add(1, Ordering::SeqCst);
                Ok("ok\n".into())
            }
            Command::Answered => Err(EPERM),
            Command::Call(r) if self.authed && r.tool == "hw_info" => {
                self.seen.calls.lock().unwrap().push(format!("{} {} {}", r.tool, r.args.to_string(), r.peer));
                let section = r.args.get("section").and_then(Json::as_str).unwrap_or("all").to_string();
                if section == "slow" {
                    std::thread::sleep(Duration::from_millis(1000));
                }
                Ok(gate::encode_result(&Ok(Ok(vec![ToolOutput::Text(format!("cpu facts for {section}"))]))))
            }
            Command::Call(_) | Command::Install(_) => Err(EPERM),
            Command::Hello => Err(22),
            // The installer's own commands: bitsmcp never sends them, and the real gate refuses them to it.
            Command::Claim | Command::Next { .. } | Command::Decide(_) | Command::Grants(_) => Err(EPERM),
            Command::Access { .. } => Err(EACCES),
        }
    }
}

fn start(conns: usize, tweak: impl FnOnce(&mut Limits)) -> (u16, Arc<Service>, Arc<Seen>) {
    start_with(conns, conns + 1, tweak)
}

fn start_with(conns: usize, sessions: usize, tweak: impl FnOnce(&mut Limits)) -> (u16, Arc<Service>, Arc<Seen>) {
    start_tls(conns, sessions, tweak, None)
}

fn start_tls(conns: usize, sessions: usize, tweak: impl FnOnce(&mut Limits), tls: Option<Arc<bitsmcp::tls::Tls>>) -> (u16, Arc<Service>, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let links: Vec<Box<dyn Link>> = (0..sessions).map(|_| Box::new(FakeGate { authed: false, seen: seen.clone() }) as Box<dyn Link>).collect();
    let mut limits = Limits::new(conns);
    tweak(&mut limits);
    let svc = Service::with_tls(Pool::new(links), limits, tls);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let s2 = svc.clone();
    std::thread::spawn(move || serve::run(l, s2));
    (port, svc, seen)
}

fn request(token: &str, body: &str) -> String {
    format!("POST /mcp HTTP/1.1\r\nHost: bitsos\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
}

/// A plain or a TLS connection of the tests.
trait Wire: Read + Write {
    fn tcp(&self) -> &TcpStream;
}

impl Wire for TcpStream {
    fn tcp(&self) -> &TcpStream {
        self
    }
}

impl Wire for rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    fn tcp(&self) -> &TcpStream {
        &self.sock
    }
}

/// One answer: (status, head, body).
fn answer(s: &mut impl Wire) -> (u16, String, String) {
    s.tcp().set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(h) = mcp_wire::http::head_end(&buf, 0) {
            let head = String::from_utf8_lossy(&buf[..h]).into_owned();
            let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
            if buf.len() >= h + len {
                let status = head[9..12].parse().unwrap();
                return (status, head, String::from_utf8_lossy(&buf[h..h + len]).into_owned());
            }
        }
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "closed before the answer: {:?}", String::from_utf8_lossy(&buf));
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn post(s: &mut impl Wire, token: &str, body: &str) -> (u16, String, Json) {
    s.write_all(request(token, body).as_bytes()).unwrap();
    let (st, head, b) = answer(s);
    (st, head, if b.is_empty() { Json::Null } else { Json::parse(&b).unwrap() })
}

fn text(j: &Json) -> String {
    j.get("result").and_then(|r| r.get("content")).and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string()
}

fn is_error(j: &Json) -> Option<bool> {
    j.get("result").and_then(|r| r.get("isError")).and_then(Json::as_bool)
}

/// The `answered` count once it reached `n` (or after 5 s): the service writes the answer first and only then
/// sends `answered` to the gate (serve.rs, RB-6), so a client can read the answer a moment before the count moves.
fn answered_reaches(seen: &Seen, n: usize) -> usize {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let v = seen.answered.load(Ordering::SeqCst);
        if v >= n || std::time::Instant::now() > end {
            return v;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn protocol_tools_and_token() {
    let (port, svc, seen) = start(3, |_| {});
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (st, head, j) = post(&mut c, TOKEN, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#);
    assert!(st == 200 && head.contains("Connection: keep-alive"), "{st} {head}");
    assert_eq!(j.get("result").and_then(|r| r.get("protocolVersion")).and_then(Json::as_str), Some(mcp_wire::rpc::PROTOCOL));
    let (st, _, j) = post(&mut c, TOKEN, r#"{"jsonrpc":"2.0","id":"two","method":"tools/list"}"#);
    let tools = j.get("result").and_then(|r| r.get("tools")).and_then(Json::as_array).unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t.get("name").and_then(Json::as_str)).collect();
    assert!(st == 200 && names == ["hw_info", "reload", "access_status", "request_access"], "{names:?}");
    assert_eq!(tools[0].get("annotations").unwrap().to_string(), r#"{"readOnlyHint":true,"destructiveHint":false,"owner":"MC","safety":"read"}"#);
    let (_, _, j) = post(&mut c, TOKEN, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"hw_info","arguments":{"section":"cpu"}}}"#);
    assert_eq!((text(&j).as_str(), is_error(&j)), ("cpu facts for cpu", Some(false)));
    assert!(seen.calls.lock().unwrap()[0].starts_with("hw_info {\"section\":\"cpu\"} 127.0.0.1:"), "{:?}", seen.calls.lock().unwrap());
    // The argument check is the kernel server's: a wrong type never reaches the gate.
    let (_, _, j) = post(&mut c, TOKEN, r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"hw_info","arguments":{"section":5}}}"#);
    assert_eq!(j.get("error").and_then(|e| e.get("code")).and_then(Json::as_f64), Some(-32602.0));
    let (_, _, j) = post(&mut c, TOKEN, r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"reload","arguments":{"url":"http://10.0.2.2/"}}}"#);
    assert!(is_error(&j) == Some(true) && text(&j).starts_with("Refused: reload goes to the installer") && text(&j).contains("EPERM"), "{}", text(&j));
    let (_, _, j) = post(&mut c, TOKEN, r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"run","arguments":{"path":"/bin/sh"}}}"#);
    assert!(is_error(&j) == Some(true) && text(&j).starts_with("Refused: run is not offered by the user-space MCP service"), "{}", text(&j));
    let (_, _, j) = post(&mut c, TOKEN, r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"typo","arguments":{}}}"#);
    assert!(j.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).unwrap().starts_with("Unknown tool: typo"));
    assert_eq!(answered_reaches(&seen, 7), 7, "answered after each authenticated answer");
    // A wrong token: 401 and closed, nothing answered.
    let mut bad = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (st, head, _) = post(&mut bad, "wrong-token-12345", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert!(st == 401 && head.contains("WWW-Authenticate: Bearer") && head.contains("Connection: close"), "{st} {head}");
    let mut nf = TcpStream::connect(("127.0.0.1", port)).unwrap();
    nf.write_all(b"GET /other HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(answer(&mut nf).0, 404);
    // Pipelining: two requests in one write, answered in order.
    let two = format!("{}{}", request(TOKEN, r#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#), request(TOKEN, r#"{"jsonrpc":"2.0","id":11,"method":"ping"}"#));
    c.write_all(two.as_bytes()).unwrap();
    assert!(answer(&mut c).2.contains("\"id\":10") && answer(&mut c).2.contains("\"id\":11"));
    assert_eq!(answered_reaches(&seen, 9), 9);
    svc.stop();
}

#[test]
fn the_idlest_gives_way() {
    let (port, svc, _) = start(3, |l| l.first_ns = 200_000_000);
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let mut cs: Vec<TcpStream> = (0..3).map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap()).collect();
    for c in cs.iter_mut() {
        assert_eq!(post(c, TOKEN, ping).0, 200);
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let mut fourth = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(post(&mut fourth, TOKEN, ping).0, 200);
    // The first (idle longest) was closed; the others still answer.
    cs[0].set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut b = [0u8; 16];
    assert!(matches!(cs[0].read(&mut b), Ok(0) | Err(_)), "the idlest connection is still open");
    assert_eq!(post(&mut cs[2], TOKEN, ping).0, 200);
    svc.stop();
}

#[test]
fn a_request_waiting_for_a_session_is_not_evicted() {
    // One gate session, two connections: the second request waits for the session the slow first one holds.
    let (port, svc, _) = start_with(2, 1, |l| {
        l.conns = 2;
        l.first_ns = 100_000_000;
    });
    let slow = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hw_info","arguments":{"section":"slow"}}}"#;
    let ping = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
    let mut a = TcpStream::connect(("127.0.0.1", port)).unwrap();
    a.write_all(request(TOKEN, slow).as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut b = TcpStream::connect(("127.0.0.1", port)).unwrap();
    b.write_all(request(TOKEN, ping).as_bytes()).unwrap();
    // b's head is complete and it waits for the session; a third connection must not push it out.
    std::thread::sleep(Duration::from_millis(300));
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(answer(&mut a).0, 200);
    assert_eq!(answer(&mut b).0, 200, "the waiting request was dropped");
    // a is idle now and gave way to c.
    assert_eq!(post(&mut c, TOKEN, ping).0, 200);
    svc.stop();
}

#[test]
fn public_peers_and_private_ones() {
    use std::net::IpAddr;
    for ok in ["10.1.2.3", "127.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.0.7", "169.254.1.1"] {
        assert!(serve::private(ok.parse::<IpAddr>().unwrap()), "{ok}");
    }
    for no in ["8.8.8.8", "172.32.0.1", "192.169.0.1", "100.64.0.1"] {
        assert!(!serve::private(no.parse::<IpAddr>().unwrap()), "{no}");
    }
}

// --- MC-8b: mutual TLS with pinned keys and the one-time pairing ------------------------------------------------

/// The client side of a pinned key: only the server certificate it paired with passes.
#[derive(Debug)]
struct PinnedServer {
    cert: Vec<u8>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinnedServer {
    fn verify_server_cert(&self, ee: &rustls::pki_types::CertificateDer<'_>, _: &[rustls::pki_types::CertificateDer<'_>], _: &rustls::pki_types::ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if ee.as_ref() == self.cert.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.3 only".into()))
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &rustls::pki_types::CertificateDer<'_>, d: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// A TLS client: pinned to `server`, showing `client` (key, cert) if given.
fn tls_client(port: u16, server: &[u8], client: Option<&(Vec<u8>, Vec<u8>)>) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    let p = bitsmcp::tls::provider();
    let pin = Arc::new(PinnedServer { cert: server.to_vec(), algs: p.signature_verification_algorithms });
    let b = rustls::ClientConfig::builder_with_provider(p).with_protocol_versions(&[&rustls::version::TLS13]).unwrap().dangerous().with_custom_certificate_verifier(pin);
    let cfg = match client {
        Some((k, c)) => b.with_client_auth_cert(vec![CertificateDer::from(c.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.clone()))).unwrap(),
        None => b.with_no_client_auth(),
    };
    let conn = rustls::ClientConnection::new(Arc::new(cfg), ServerName::try_from("bitsos").unwrap()).unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    rustls::StreamOwned::new(conn, tcp)
}

/// Everything the server sends after a request, until it closes (an error counts as the end): the bytes.
fn all_after(s: &mut impl Wire, req: &[u8]) -> Vec<u8> {
    s.tcp().set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut got = Vec::new();
    if s.write_all(req).and_then(|()| s.flush()).is_err() {
        return got;
    }
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return got,
            Ok(n) => got.extend_from_slice(&chunk[..n]),
        }
    }
}

fn pair_body(cert: &[u8]) -> String {
    let mut b = String::new();
    mcp_wire::b64::encode_into(cert, &mut b);
    format!(r#"{{"jsonrpc":"2.0","id":7,"method":"bitsos/pair","params":{{"certificate":"{b}"}}}}"#)
}

fn key_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bitsmcp-tls-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

const CALL: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"hw_info","arguments":{"section":"cpu"}}}"#;

#[test]
fn pairing_once_then_tls_only() {
    use bitsmcp::{cert, keys, tls::Tls};
    let dir = key_dir("pair");
    let rng = ring::rand::SystemRandom::new();
    let desk = cert::generate(&rng, "bitsos-desk").unwrap();
    let stranger = cert::generate(&rng, "bitsos-desk").unwrap();
    let tls = Arc::new(Tls::new(keys::open(&dir).unwrap()).unwrap());
    let server = tls.cert().to_vec();
    let (port, svc, _) = start_tls(3, 4, |_| {}, Some(tls.clone()));
    // Unpaired: plain HTTP with the token, as MC-8a; a pairing needs the token.
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(text(&post(&mut c, TOKEN, CALL).2), "cpu facts for cpu");
    let mut w = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(post(&mut w, "wrong", &pair_body(&desk.1)).0, 401);
    assert!(!tls.on());
    let (st, head, j) = post(&mut c, TOKEN, &pair_body(&desk.1));
    let r = j.get("result").unwrap_or_else(|| panic!("{}", j.to_string()));
    assert!(st == 200 && head.contains("Connection: close"), "{st} {head}");
    assert_eq!(r.get("paired").and_then(Json::as_str), Some(cert::fingerprint(&desk.1).as_str()));
    assert_eq!(r.get("server_certificate").and_then(Json::as_str).and_then(mcp_wire::b64::decode), Some(server.clone()));
    assert_eq!(r.get("server_fingerprint").and_then(Json::as_str), Some(tls.fingerprint().as_str()));
    assert!(tls.on());
    // From now: plain HTTP gets no byte, not even a status.
    let mut p = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(all_after(&mut p, request(TOKEN, CALL).as_bytes()), b"", "plain HTTP after the pairing");
    // The paired key: MCP as before, the token still the kernel's to judge.
    let mut t = tls_client(port, &server, Some(&desk));
    assert_eq!(text(&post(&mut t, TOKEN, CALL).2), "cpu facts for cpu");
    assert_eq!(post(&mut t, "wrong", CALL).0, 401);
    // A second pairing over TLS is refused (MC-9 confirms more), and the stranger's key stays unpaired.
    let mut t = tls_client(port, &server, Some(&desk));
    let (_, _, j) = post(&mut t, TOKEN, &pair_body(&stranger.1));
    assert!(j.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).is_some_and(|m| m.starts_with("pairing is closed")), "{}", j.to_string());
    // A key that is not paired, or none: the handshake fails and no HTTP byte comes.
    for who in [Some(&stranger), None] {
        let mut s = tls_client(port, &server, who);
        let got = all_after(&mut s, request(TOKEN, CALL).as_bytes());
        assert!(got.is_empty(), "an unpaired client got {:?}", String::from_utf8_lossy(&got));
    }
    // A client pinned to another server key refuses this one.
    let mut s = tls_client(port, &stranger.1, Some(&desk));
    assert!(s.write_all(request(TOKEN, CALL).as_bytes()).and_then(|()| s.flush()).is_err() || s.read(&mut [0u8; 16]).is_err());
    svc.stop();
    // The next start: the pairing kept, TLS from the first connection, no second one-time pairing.
    let k = keys::open(&dir).unwrap();
    assert!(k.paired == vec![desk.1.clone()] && k.slot.is_none() && k.cert == server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_tls_pairing_is_refused_and_plain_stays() {
    let (port, svc, _) = start(3, |_| {});
    let rng = ring::rand::SystemRandom::new();
    let desk = bitsmcp::cert::generate(&rng, "bitsos-desk").unwrap();
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (st, _, j) = post(&mut c, TOKEN, &pair_body(&desk.1));
    assert!(st == 200 && j.get("error").is_some(), "{}", j.to_string());
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(text(&post(&mut c, TOKEN, CALL).2), "cpu facts for cpu");
    svc.stop();
}

/// host/mcp's own tools against the service (node:tls against rustls): pair.mjs pairs and pins, mcp-call.mjs
/// --tls calls, plain HTTP is off, another client key gets nothing, another server key is refused loudly.
#[test]
fn the_host_tools_pair_pin_and_refuse() {
    use bitsmcp::{keys, tls::Tls};
    use std::process::Command;
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skip: node is not installed (host/mcp's tools not tried)");
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let (dir, store, other) = (key_dir("node"), key_dir("node-store"), key_dir("node-other"));
    let s = store.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let o = Command::new("node").current_dir(&root).env("BITSOS_TOKEN", TOKEN).args(args).output().unwrap();
        (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    };
    let tls = Arc::new(Tls::new(keys::open(&dir).unwrap()).unwrap());
    let (port, svc, _) = start_tls(3, 4, |_| {}, Some(tls.clone()));
    let host = format!("127.0.0.1:{port}");
    let (rc, out) = run(&["host/mcp/mcp-call.mjs", &host, "hw_info", "{}", "--tls", s]);
    assert!(rc == 2 && out.contains("pair first"), "--tls before the pairing: {rc} {out}");
    let (rc, out) = run(&["host/mcp/pair.mjs", &host, "--store", s]);
    assert!(rc == 2 && out.contains("--expect <sha256> is needed") && !tls.on(), "pair.mjs without --expect: {rc} {out}");
    let (rc, out) = run(&["host/mcp/pair.mjs", &host, "--store", s, "--expect", "00"]);
    assert!(rc == 1 && out.contains("not the expected 00") && !tls.on(), "a wrong --expect pairs nothing: {rc} {out}");
    let (rc, out) = run(&["host/mcp/pair.mjs", &host, "--store", s, "--expect", &tls.fingerprint()]);
    assert!(rc == 0 && out.contains(&tls.fingerprint()) && tls.on(), "pair.mjs: {rc} {out}");
    let (rc, out) = run(&["host/mcp/mcp-call.mjs", &host, "hw_info", r#"{"section":"cpu"}"#, "--tls", s]);
    assert!(rc == 0 && out.contains("cpu facts for cpu"), "mcp-call.mjs --tls: {rc} {out}");
    let (rc, out) = run(&["host/mcp/mcp-call.mjs", &host, "hw_info", "{}"]);
    assert!(rc == 2 && !out.contains("cpu facts"), "plain after the pairing: {rc} {out}");
    let (rc, out) = run(&["host/mcp/pair.mjs", &host, "--store", s]);
    assert!(rc == 2 && out.contains("paired already"), "a second pairing: {rc} {out}");
    // Another client key with the right pin: the handshake fails, nothing answers.
    std::fs::create_dir_all(other.join("pins")).unwrap();
    for e in std::fs::read_dir(store.join("pins")).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), other.join("pins").join(e.file_name())).unwrap();
    }
    let o = other.to_str().unwrap();
    let (rc, out) = run(&["-e", "import('./host/mcp/mcp-keys.mjs').then((m) => m.clientKey(process.argv[1], { make: true }))", o]);
    assert_eq!(rc, 0, "{out}");
    let (rc, out) = run(&["host/mcp/mcp-call.mjs", &host, "hw_info", "{}", "--tls", o]);
    assert!(rc == 2 && !out.contains("cpu facts"), "an unpaired client key: {rc} {out}");
    svc.stop();
    // The service's key made anew (the paired client kept): the host tool refuses it before sending anything.
    let dir2 = key_dir("node-new");
    std::fs::create_dir_all(dir2.join(keys::PAIRED)).unwrap();
    std::fs::copy(dir.join(keys::PAIRED).join(keys::FIRST), dir2.join(keys::PAIRED).join(keys::FIRST)).unwrap();
    let tls2 = Arc::new(Tls::new(keys::open(&dir2).unwrap()).unwrap());
    assert!(tls2.on() && tls2.cert() != tls.cert());
    let (port, svc, seen) = start_tls(3, 4, |_| {}, Some(tls2));
    let (rc, out) = run(&["host/mcp/mcp-call.mjs", &format!("127.0.0.1:{port}"), "hw_info", "{}", "--tls", s]);
    assert!(rc == 2 && out.contains("ANOTHER KEY") && out.contains("Nothing was sent"), "a changed server key: {rc} {out}");
    assert!(seen.calls.lock().unwrap().is_empty(), "nothing reached the gate");
    svc.stop();
    for d in [&dir, &dir2, &store, &other] {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn pair_error(j: &Json) -> String {
    j.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).map(String::from).unwrap_or_else(|| format!("no error: {}", j.to_string()))
}

/// Hostile or unusable pairing requests are answered with a JSON-RPC error and do not spend the one pairing; a
/// failed write spends it and says so (no false "a client is paired").
#[test]
fn hostile_pairings_spend_nothing() {
    use bitsmcp::{cert, keys, tls::Tls};
    let dir = key_dir("hostile");
    let rng = ring::rand::SystemRandom::new();
    let desk = cert::generate(&rng, "bitsos-desk").unwrap();
    let tls = Arc::new(Tls::new(keys::open(&dir).unwrap()).unwrap());
    let (port, svc, _) = start_tls(3, 4, |_| {}, Some(tls.clone()));
    let mut forged = desk.1.clone();
    *forged.last_mut().unwrap() ^= 1;
    let mut huge = desk.1.clone();
    huge.resize(cert::CERT_MAX + 1, 0);
    let own = tls.cert().to_vec();
    let bodies = [
        String::from(r#"{"jsonrpc":"2.0","id":7,"method":"bitsos/pair"}"#),
        String::from(r#"{"jsonrpc":"2.0","id":7,"method":"bitsos/pair","params":{"certificate":"%%% not base64 %%%"}}"#),
        String::from(r#"{"jsonrpc":"2.0","id":7,"method":"bitsos/pair","params":{"certificate":""}}"#),
        String::from(r#"{"jsonrpc":"2.0","id":7,"method":"bitsos/pair","params":{"certificate":17}}"#),
        pair_body(b"no certificate at all"),
        pair_body(&forged),
        pair_body(&huge),
        pair_body(&own),
    ];
    for b in &bodies {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (st, _, j) = post(&mut c, TOKEN, b);
        let m = pair_error(&j);
        assert!(st == 200 && !m.starts_with("no error"), "{st} {m}");
        assert!(!tls.on(), "a hostile pairing paired: {}", &b[..b.len().min(120)]);
    }
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (_, _, j) = post(&mut c, TOKEN, &pair_body(&desk.1));
    assert!(j.get("result").is_some() && tls.on(), "the right pairing after the hostile ones: {}", j.to_string());
    svc.stop();
    let _ = std::fs::remove_dir_all(&dir);

    // A slot that cannot be written (opened read-only here): the write fails, the slot is spent and the next
    // pairing is told that, not that a client is paired.
    let dir = key_dir("hostile-write");
    let mut k = keys::open(&dir).unwrap();
    k.slot = Some(std::fs::File::open(dir.join(keys::PAIRED).join(keys::FIRST)).unwrap());
    let tls = Tls::new(k).unwrap();
    let e = tls.pair(&desk.1).unwrap_err();
    assert!(matches!(e, bitsmcp::tls::Refused::Write(_)), "{e:?}");
    assert_eq!(tls.pair(&desk.1).unwrap_err(), bitsmcp::tls::Refused::Spent);
    assert!(!tls.on() && tls.pair(&desk.1).unwrap_err().text().contains("restart"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// After the pairing: clients that stall in the handshake (3 bytes of a TLS record and silence) give way like
/// idle plain ones, garbage that starts like TLS is closed silently, and the paired client is still served.
#[test]
fn stalled_and_garbage_handshakes_give_way() {
    use bitsmcp::{cert, keys, tls::Tls};
    let dir = key_dir("stall");
    let rng = ring::rand::SystemRandom::new();
    let desk = cert::generate(&rng, "bitsos-desk").unwrap();
    let tls = Arc::new(Tls::new(keys::open(&dir).unwrap()).unwrap());
    tls.pair(&desk.1).unwrap();
    let server = tls.cert().to_vec();
    let (port, svc, _) = start_tls(3, 4, |l| l.first_ns = 200_000_000, Some(tls.clone()));
    let stalled: Vec<TcpStream> = (0..4)
        .map(|_| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(&[0x16, 0x03, 0x01]).unwrap();
            s
        })
        .collect();
    std::thread::sleep(Duration::from_millis(400));
    let mut t = tls_client(port, &server, Some(&desk));
    assert_eq!(text(&post(&mut t, TOKEN, CALL).2), "cpu facts for cpu", "the paired client behind stalled handshakes");
    drop(t);
    for _ in 0..3 {
        let mut g = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut junk = vec![0x16u8];
        let mut more = [0u8; 300];
        ring::rand::SecureRandom::fill(&rng, &mut more).unwrap();
        junk.extend_from_slice(&more);
        // At most a TLS alert record (content type 21), never an HTTP byte.
        let got = all_after(&mut g, &junk);
        assert!(got.is_empty() || (got[0] == 0x15 && got.len() <= 64), "garbage got {got:?}");
    }
    let mut t = tls_client(port, &server, Some(&desk));
    assert_eq!(text(&post(&mut t, TOKEN, CALL).2), "cpu facts for cpu", "the service after the garbage");
    assert!(svc.connections() <= 3, "more connections than allowed: {}", svc.connections());
    drop(stalled);
    svc.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plain request whose head came in before another connection paired, and whose body only after: no answer.
#[test]
fn a_plain_request_in_flight_ends_with_the_pairing() {
    use bitsmcp::{cert, keys, tls::Tls};
    let dir = key_dir("inflight");
    let rng = ring::rand::SystemRandom::new();
    let desk = cert::generate(&rng, "bitsos-desk").unwrap();
    let tls = Arc::new(Tls::new(keys::open(&dir).unwrap()).unwrap());
    let (port, svc, seen) = start_tls(3, 4, |_| {}, Some(tls.clone()));
    let req = request(TOKEN, CALL);
    let cut = req.find("\r\n\r\n").unwrap() + 4;
    let mut p = TcpStream::connect(("127.0.0.1", port)).unwrap();
    p.write_all(&req.as_bytes()[..cut]).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert!(post(&mut c, TOKEN, &pair_body(&desk.1)).2.get("result").is_some() && tls.on());
    let calls = seen.calls.lock().unwrap().len();
    assert_eq!(all_after(&mut p, &req.as_bytes()[cut..]), b"" as &[u8], "the plain request was answered after the pairing");
    assert_eq!(seen.calls.lock().unwrap().len(), calls, "the plain request reached the gate");
    svc.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
