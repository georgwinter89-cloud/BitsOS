//! MC-10 over real sockets against a fake gate: the TLS client's key goes with every list and call (a grant the
//! gate holds opens more tools to that key only), `request_access`/`access_status` are the service's own tools,
//! under mcp=service (strict) a token over plain HTTP opens nothing but the pairing and the pairing waits for the
//! installer's yes; a recorded TLS session replayed on a new connection gets no HTTP byte.

use bitsmcp::link::{EACCES, EPERM, Link, Pool};
use bitsmcp::serve::{self, Limits, Service};
use bitsmcp::{cert, keys, tls::Tls};
use mcp_wire::access as acc;
use mcp_wire::gate::{self, Command, Listed};
use mcp_wire::{Json, Safety, ToolError, ToolOutput};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "access-test-token";

#[derive(Default)]
struct World {
    /// Clients with a grant (what the kernel's table would hold).
    granted: Mutex<Vec<String>>,
    /// Every client key a list or call named.
    seen: Mutex<Vec<String>>,
    /// The installer's answer to pair_client.
    pair_yes: AtomicBool,
    pair_asked: AtomicBool,
}

struct FakeGate {
    authed: bool,
    w: Arc<World>,
}

fn tool(name: &str) -> Listed {
    Listed { name: name.into(), description: format!("{name} tool"), schema: r#"{"type":"object"}"#.into(), owner: "MC".into(), safety: Safety::Read, verified: true, pid: None, service: false, installer: false }
}

impl FakeGate {
    fn granted(&self, c: &str) -> bool {
        !c.is_empty() && self.w.granted.lock().unwrap().iter().any(|g| g == c)
    }
}

impl Link for FakeGate {
    fn command(&mut self, line: &[u8]) -> Result<String, i32> {
        let ok = |t: &str| Ok(gate::encode_result(&Ok(Ok(vec![ToolOutput::Text(t.into())]))));
        match gate::parse_command(line).ok_or(22)? {
            Command::List { client } => {
                self.w.seen.lock().unwrap().push(client.clone());
                let mut v = vec![tool("hw_info")];
                if self.granted(&client) {
                    v.push(tool("run"));
                }
                Ok(gate::encode_list_answer(&v))
            }
            Command::Auth { token, .. } => {
                self.authed = token == TOKEN.as_bytes();
                if self.authed { Ok("ok\n".into()) } else { Err(EACCES) }
            }
            Command::Answered => Ok("ok\n".into()),
            Command::Access { client } if self.authed => {
                let g = self.granted(&client);
                Ok(acc::encode_status(&acc::Status { granted: g, until: if g { 1_800_003_600 } else { 0 }, left_s: if g { 3600 } else { 0 }, nvme: false, local: if g { "2027-01-15 09:00".into() } else { String::new() }, loaded: true }))
            }
            Command::Call(r) if self.authed => {
                self.w.seen.lock().unwrap().push(r.client.clone());
                match r.tool.as_str() {
                    "hw_info" => ok("cpu facts"),
                    "run" if self.granted(&r.client) => ok("ran as root"),
                    _ => Err(EPERM),
                }
            }
            Command::Install(r) if self.authed && r.tool == acc::REQUEST_ACCESS => {
                assert!(acc::is_client(&r.client), "only a TLS client asks: {r:?}");
                self.w.granted.lock().unwrap().push(r.client.clone());
                ok("full access for this client until 2027-01-15 09:00")
            }
            Command::Install(r) if self.authed && r.tool == acc::PAIR_CLIENT => {
                self.w.pair_asked.store(true, Ordering::SeqCst);
                assert!(r.args.get("fingerprint").and_then(Json::as_str).is_some_and(acc::is_client), "{r:?}");
                if self.w.pair_yes.load(Ordering::SeqCst) { ok("yes: confirmed at the machine") } else { Ok(gate::encode_result(&Ok(Err(ToolError::Refused("pair_client was not done: the person at the machine said no".into()))))) }
            }
            _ => Err(EPERM),
        }
    }
}

fn start(tls: Arc<Tls>, strict: bool) -> (u16, Arc<Service>, Arc<World>) {
    let w = Arc::new(World::default());
    let links: Vec<Box<dyn Link>> = (0..5).map(|_| Box::new(FakeGate { authed: false, w: w.clone() }) as Box<dyn Link>).collect();
    let svc = Service::with_tls(Pool::new(links), Limits::new(4), Some(tls));
    svc.set_strict(strict);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let s2 = svc.clone();
    std::thread::spawn(move || serve::run(l, s2));
    (port, svc, w)
}

fn key_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bitsmcp-access-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[derive(Debug)]
struct PinnedServer {
    cert: Vec<u8>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinnedServer {
    fn verify_server_cert(&self, ee: &rustls::pki_types::CertificateDer<'_>, _: &[rustls::pki_types::CertificateDer<'_>], _: &rustls::pki_types::ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if ee.as_ref() == self.cert.as_slice() { Ok(rustls::client::danger::ServerCertVerified::assertion()) } else { Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)) }
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

/// A TCP stream that keeps a copy of every byte the client wrote (for the replay).
struct Rec {
    tcp: TcpStream,
    sent: Arc<Mutex<Vec<u8>>>,
}
impl Read for Rec {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.tcp.read(b)
    }
}
impl Write for Rec {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.tcp.write(b)?;
        self.sent.lock().unwrap().extend_from_slice(&b[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

type Client = rustls::StreamOwned<rustls::ClientConnection, Rec>;

fn tls_client(port: u16, server: &[u8], key: &(Vec<u8>, Vec<u8>)) -> (Client, Arc<Mutex<Vec<u8>>>) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    let p = bitsmcp::tls::provider();
    let pin = Arc::new(PinnedServer { cert: server.to_vec(), algs: p.signature_verification_algorithms });
    let cfg = rustls::ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(pin)
        .with_client_auth_cert(vec![CertificateDer::from(key.1.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.0.clone())))
        .unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(cfg), ServerName::try_from("bitsos").unwrap()).unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    (rustls::StreamOwned::new(conn, Rec { tcp, sent: sent.clone() }), sent)
}

fn request(token: &str, body: &str) -> String {
    format!("POST /mcp HTTP/1.1\r\nHost: bitsos\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
}

/// One answer's (status, body) from `s`.
fn answer(s: &mut impl Read) -> (u16, String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(h) = mcp_wire::http::head_end(&buf, 0) {
            let head = String::from_utf8_lossy(&buf[..h]).into_owned();
            let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
            if buf.len() >= h + len {
                return (head[9..12].parse().unwrap(), String::from_utf8_lossy(&buf[h..h + len]).into_owned());
            }
        }
        let n = s.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return (0, String::from_utf8_lossy(&buf).into_owned());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn rpc(s: &mut (impl Read + Write), token: &str, method: &str, params: &str) -> (u16, Json) {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#);
    if s.write_all(request(token, &body).as_bytes()).and_then(|()| s.flush()).is_err() {
        return (0, Json::Null);
    }
    let (st, b) = answer(s);
    (st, Json::parse(&b).unwrap_or(Json::Null))
}

fn call(s: &mut (impl Read + Write), name: &str, args: &str) -> (bool, String) {
    let (_, j) = rpc(s, TOKEN, "tools/call", &format!(r#"{{"name":"{name}","arguments":{args}}}"#));
    let r = j.get("result").cloned().unwrap_or(Json::Null);
    let text = r.get("content").and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string();
    (r.get("isError").and_then(Json::as_bool) != Some(true), text)
}

fn names(s: &mut (impl Read + Write)) -> Vec<String> {
    let (_, j) = rpc(s, TOKEN, "tools/list", "{}");
    j.get("result").and_then(|r| r.get("tools")).and_then(Json::as_array).map(|t| t.iter().filter_map(|x| x.get("name").and_then(Json::as_str).map(String::from)).collect()).unwrap_or_default()
}

fn paired(name: &str) -> (Arc<Tls>, (Vec<u8>, Vec<u8>), (Vec<u8>, Vec<u8>), std::path::PathBuf) {
    let dir = key_dir(name);
    let rng = ring::rand::SystemRandom::new();
    let desk = cert::generate(&rng, "bitsos-desk").unwrap();
    let stranger = cert::generate(&rng, "bitsos-desk").unwrap();
    let tls = Arc::new(Tls::new(keys::open(&dir).unwrap()).unwrap());
    tls.pair(&desk.1).unwrap();
    (tls, desk, stranger, dir)
}

#[test]
fn the_client_key_goes_along_and_a_grant_opens_more_for_it_only() {
    let (tls, desk, _, dir) = paired("grant");
    let server = tls.cert().to_vec();
    let fp = cert::fingerprint(&desk.1);
    let (port, svc, w) = start(tls, true);
    let (mut t, _) = tls_client(port, &server, &desk);
    assert_eq!(names(&mut t), ["hw_info", "access_status", "request_access"]);
    assert!(call(&mut t, "access_status", "{}").1.starts_with("no full access"));
    let (ok, why) = call(&mut t, "run", "{}");
    assert!(!ok && why.contains("not offered"), "{why}");
    let (ok, said) = call(&mut t, "request_access", r#"{"hours":24,"reason":"Welle 21"}"#);
    assert!(ok && said.contains("full access"), "{said}");
    assert_eq!(names(&mut t), ["hw_info", "run", "access_status", "request_access"]);
    assert_eq!(call(&mut t, "run", "{}"), (true, String::from("ran as root")));
    assert!(call(&mut t, "access_status", "{}").1.contains("until 2027-01-15 09:00"));
    assert!(w.seen.lock().unwrap().iter().all(|c| *c == fp), "every list and call named the client's key");
    svc.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn under_service_a_token_alone_opens_nothing_and_pairing_waits_for_a_yes() {
    let dir = key_dir("strict");
    let rng = ring::rand::SystemRandom::new();
    let desk = cert::generate(&rng, "bitsos-desk").unwrap();
    let tls = Arc::new(Tls::new(keys::open(&dir).unwrap()).unwrap());
    let (port, svc, w) = start(tls.clone(), true);
    // Unpaired, the right token, plain HTTP: 403 for every MCP method, nothing reaches a tool.
    for m in ["initialize", "tools/list", "tools/call"] {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        assert_eq!(rpc(&mut c, TOKEN, m, r#"{"name":"hw_info"}"#).0, 403, "{m}");
    }
    assert!(w.seen.lock().unwrap().is_empty());
    let mut b = String::new();
    mcp_wire::b64::encode_into(&desk.1, &mut b);
    let pair = format!(r#"{{"certificate":"{b}"}}"#);
    // Georg says no: nothing paired.
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let (_, j) = rpc(&mut c, TOKEN, "bitsos/pair", &pair);
    assert!(w.pair_asked.load(Ordering::SeqCst), "asked at the keyboard");
    assert!(j.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).is_some_and(|m| m.contains("said no")), "{}", j.to_string());
    assert!(!tls.on());
    // A wrong token is not even asked about.
    w.pair_asked.store(false, Ordering::SeqCst);
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    assert_eq!(rpc(&mut c, "wrong-token", "bitsos/pair", &pair).0, 401);
    assert!(!w.pair_asked.load(Ordering::SeqCst));
    // Yes: paired, and then TLS only.
    w.pair_yes.store(true, Ordering::SeqCst);
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let (_, j) = rpc(&mut c, TOKEN, "bitsos/pair", &pair);
    assert!(j.get("result").is_some(), "{}", j.to_string());
    assert!(tls.on());
    let (mut t, _) = tls_client(port, tls.cert(), &desk);
    assert_eq!(call(&mut t, "hw_info", "{}"), (true, String::from("cpu facts")));
    svc.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A recorded TLS session (handshake, request) sent again on a new connection: the server's fresh random makes
/// the old Finished and CertificateVerify worthless; no HTTP byte comes back. A wrong client key gets nothing.
#[test]
fn a_replayed_session_and_a_wrong_key_get_nothing() {
    let (tls, desk, stranger, dir) = paired("replay");
    let server = tls.cert().to_vec();
    let (port, svc, w) = start(tls, true);
    let (mut t, sent) = tls_client(port, &server, &desk);
    assert_eq!(call(&mut t, "hw_info", "{}"), (true, String::from("cpu facts")));
    drop(t);
    let recorded = sent.lock().unwrap().clone();
    assert!(recorded.len() > 200 && recorded[0] == 0x16);
    let calls_before = w.seen.lock().unwrap().len();
    let mut r = TcpStream::connect(("127.0.0.1", port)).unwrap();
    r.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let _ = r.write_all(&recorded);
    let mut got = Vec::new();
    let _ = r.read_to_end(&mut got);
    assert!(!String::from_utf8_lossy(&got).contains("HTTP/1.1"), "the replay got an HTTP answer");
    assert_eq!(w.seen.lock().unwrap().len(), calls_before, "nothing reached the gate");
    let (mut s, _) = tls_client(port, &server, &stranger);
    assert!(call(&mut s, "hw_info", "{}").1.is_empty(), "an unpaired key gets nothing");
    // The paired key with a wrong token: 401 (the key alone is not enough either).
    let (mut t, _) = tls_client(port, &server, &desk);
    assert_eq!(rpc(&mut t, "wrong-token", "tools/list", "{}").0, 401);
    svc.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
