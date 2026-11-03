//! The confirmer's place over a real Unix socket: one confirmer at a time, dialogs answered, timed out and cut
//! off, grants set over the socket (only with real input), a peer the check refuses.

use bitsinstall::confirm::{self, Input};
use bitsinstall::decide::{Confirmer, Outcome, State};
use bitsinstall::grant::Grants;
use bitsinstall::hub::{Hub, PeerCheck};
use mcp_wire::installer::{Asked, Kind};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const T: i64 = 1_800_000_000;

fn clock() -> i64 {
    T
}

fn socket(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("bitsinstall-{name}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn hub(name: &str, check: PeerCheck) -> (Arc<Hub>, Arc<State>, PathBuf) {
    let p = socket(name);
    let state = Arc::new(State::new(Grants::default(), None, Duration::from_millis(800)));
    let h = Hub::new(state.clone(), check, clock);
    h.serve(UnixListener::bind(&p).unwrap()).unwrap();
    (h, state, p)
}

fn anyone() -> PeerCheck {
    Box::new(|_| Ok(String::from("test peer")))
}

struct Peer {
    w: UnixStream,
    r: BufReader<UnixStream>,
}

impl Peer {
    fn connect(p: &PathBuf) -> Peer {
        let s = UnixStream::connect(p).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Peer { r: BufReader::new(s.try_clone().unwrap()), w: s }
    }
    fn line(&mut self) -> String {
        let mut l = String::new();
        self.r.read_line(&mut l).unwrap();
        l
    }
    fn send(&mut self, l: &str) {
        self.w.write_all(l.as_bytes()).unwrap();
    }
}

fn wait_connected(h: &Hub, on: bool) {
    for _ in 0..200 {
        if h.connected() == on {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("connected never became {on}");
}

#[test]
fn one_confirmer_answers_dialogs() {
    let (h, _, p) = hub("answers", anyone());
    assert!(!h.connected());
    assert_eq!(h.ask(1, Kind::Reload, "t", "x", Duration::from_secs(1)), Outcome::NoSession);
    let mut c = Peer::connect(&p);
    assert!(c.line().contains("\"type\":\"hello\""));
    wait_connected(&h, true);
    let mut second = Peer::connect(&p);
    assert!(second.line().contains("another confirmer"), "one at a time");
    let h2 = h.clone();
    let t = std::thread::spawn(move || h2.ask(7, Kind::Install, "Neuen Kernel installieren", "Werkzeug: install_kernel", Duration::from_secs(5)));
    let ask = c.line();
    let (id, kind, title, text, ms) = confirm::parse_ask(ask.trim_end()).expect("an ask");
    assert_eq!((id, kind.as_str(), title.as_str(), text.as_str(), ms), (7, "install", "Neuen Kernel installieren", "Werkzeug: install_kernel", 5000));
    c.send(&confirm::answer(99, true, Input::Physical));
    assert!(c.line().contains("no dialog with this id"), "an answer to nothing is refused");
    c.send(&confirm::answer(7, true, Input::Physical));
    assert_eq!(t.join().unwrap(), Outcome::Answer { yes: true, input: Input::Physical });
    // A dialog nobody answers: timeout, then a cancel for the confirmer.
    assert_eq!(h.ask(8, Kind::Reload, "t", "x", Duration::from_millis(300)), Outcome::Timeout);
    assert!(c.line().trim_end().starts_with("{\"type\":\"ask\""));
    assert!(c.line().contains("\"type\":\"cancel\""));
    // The confirmer goes away while a dialog is open.
    let h3 = h.clone();
    let t = std::thread::spawn(move || h3.ask(9, Kind::Reboot, "t", "x", Duration::from_secs(5)));
    let _ = c.line();
    drop(c);
    assert_eq!(t.join().unwrap(), Outcome::Ended);
    wait_connected(&h, false);
    // The place is free again.
    let mut again = Peer::connect(&p);
    assert!(again.line().contains("hello"));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn grants_over_the_socket_need_real_input() {
    let (h, state, p) = hub("grants", anyone());
    let mut c = Peer::connect(&p);
    let _ = c.line();
    wait_connected(&h, true);
    c.send(&confirm::grant("install", T + 3600, Input::Injected));
    assert!(c.line().contains("only a real keyboard"));
    c.send(&confirm::grant("nvme", T + 3600, Input::Physical));
    assert!(c.line().contains("never granted"));
    c.send(&confirm::grant("install", T + 3600, Input::Physical));
    let g = c.line();
    assert!(g.contains("\"type\":\"grants\"") && g.contains("\"install\""), "{g}");
    assert_eq!(state.grants().len(), 1);
    let a = Asked { id: 3, tool: "install_kernel".into(), kind: Kind::Install, args: mcp_wire::Json::Obj(vec![]), timeout_ms: 1000, client: String::new(), peer: "10.0.2.2:1".into() };
    assert_eq!(state.decide(&*h, &a, T + 5).by, "grant");
    c.send(&confirm::revoke("all"));
    assert!(c.line().contains("\"grants\":[]"));
    c.send("{\"type\":\"frob\"}\n");
    assert!(c.line().contains("not understood"));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_refused_peer_is_told_why_and_never_holds_the_place() {
    let (h, _, p) = hub("refused", Box::new(|_| Err(String::from("pid 5 runs /tmp/bi, not /bin/bitscomp"))));
    let mut c = Peer::connect(&p);
    assert!(c.line().contains("not /bin/bitscomp"));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!h.connected());
    let _ = std::fs::remove_file(&p);
}

/// INT-W24 (WY-14 CR 3): the gpu build on the stick is a confirmer like /bin/bitscomp, under the same compositor
/// holder check; anything else, or either build without the grant, is refused.
#[test]
fn the_gpu_build_confirms_only_while_it_holds_the_compositor() {
    use bitsinstall::{judge_peer, CONFIRMERS};
    use std::path::Path;
    let gpu = Path::new("/boot/BITSOS2/bin/bitscomp-gpu");
    let ok = judge_peer(&CONFIRMERS, 7, 1000, gpu, Some("holder=7\n")).unwrap();
    assert!(ok.contains("bitscomp-gpu") && ok.contains("holds the compositor grant"), "{ok}");
    assert!(judge_peer(&CONFIRMERS, 7, 1000, gpu, Some("holder=9\n")).unwrap_err().contains("pid 9 holds"));
    assert!(judge_peer(&CONFIRMERS, 7, 1000, gpu, Some("holder=none\n")).unwrap_err().contains("no process holds"));
    assert!(judge_peer(&CONFIRMERS, 7, 1000, Path::new("/bin/bitscomp"), Some("holder=7")).is_ok());
    assert!(judge_peer(&CONFIRMERS, 7, 1000, Path::new("/bin/bitscomp"), Some("holder=8")).is_err());
    // No holder file: uid and executable decide, as before INT-W17.
    assert!(judge_peer(&CONFIRMERS, 7, 0, gpu, None).is_ok());
    assert!(judge_peer(&CONFIRMERS, 7, 1000, Path::new("/tmp/bitscomp-gpu"), Some("holder=7")).unwrap_err().contains("not /bin/bitscomp or"));
    assert!(judge_peer(&CONFIRMERS, 7, 1001, gpu, Some("holder=7")).unwrap_err().contains("uid 1001"));
    // The test confirmer has no holder check.
    assert!(judge_peer(&[bitsinstall::TEST_CONFIRMER], 7, 0, Path::new("/bin/bitsinstall"), Some("holder=none")).is_ok());
}

/// A peer that sends garbage, an unknown id, a replayed answer, a missing or spoofed `input` and finally a line
/// over the limit: none of it is a yes from a person, nothing panics, and the over-long line ends the peer.
#[test]
fn hostile_lines_are_never_a_yes() {
    let (h, _, p) = hub("hostile", anyone());
    let mut c = Peer::connect(&p);
    let _ = c.line();
    wait_connected(&h, true);
    for junk in ["\n", "{\n", "not json at all\n", "{\"type\":\"answer\",\"id\":\"7\",\"yes\":\"yes\"}\n", "\u{0}\u{1}\u{2}\n"] {
        c.send(junk);
        assert!(c.line().contains("not understood"), "{junk:?}");
    }
    c.send(&confirm::answer(5, true, Input::Physical));
    assert!(c.line().contains("no dialog with this id"), "an answer before any dialog");
    // Missing and spoofed `input` reach the decision as Input::None: not physical, so never a yes there.
    let h2 = h.clone();
    let t = std::thread::spawn(move || h2.ask(11, Kind::Pin, "t", "x", Duration::from_secs(5)));
    let _ = c.line();
    c.send("{\"type\":\"answer\",\"id\":11,\"yes\":true,\"input\":\"PHYSICAL\"}\n");
    assert_eq!(t.join().unwrap(), Outcome::Answer { yes: true, input: Input::None });
    // The same answer again, after its dialog closed: refused, not kept for a later dialog with that id.
    c.send(&confirm::answer(11, true, Input::Physical));
    assert!(c.line().contains("no dialog with this id"), "a replayed answer");
    let h3 = h.clone();
    let t = std::thread::spawn(move || h3.ask(12, Kind::Pin, "t", "x", Duration::from_millis(400)));
    let _ = c.line();
    assert_eq!(t.join().unwrap(), Outcome::Timeout, "the replay did not answer the next dialog");
    let _ = c.line(); // its cancel
    // Over the line limit: the peer is dropped, the place is free.
    let mut big = String::from("{\"type\":\"list\",\"pad\":\"");
    big.push_str(&"x".repeat(confirm::LINE_MAX + 10));
    big.push_str("\"}\n");
    let _ = c.w.write_all(big.as_bytes());
    wait_connected(&h, false);
    assert!(Peer::connect(&p).line().contains("hello"));
    let _ = std::fs::remove_file(&p);
}

/// A confirmer that stops reading: the dialog that does not fit into its socket ends within the write bound,
/// the confirmer counts as gone and the place takes the next one (B4: nothing waits on it for ever).
#[test]
fn a_confirmer_that_stops_reading_is_dropped() {
    let (h, _, p) = hub("stalled", anyone());
    let mut c = Peer::connect(&p);
    let _ = c.line();
    wait_connected(&h, true);
    let text = "y".repeat(8 << 20);
    let t0 = std::time::Instant::now();
    assert_eq!(h.ask(21, Kind::Install, "t", &text, Duration::from_secs(30)), Outcome::Ended);
    assert!(t0.elapsed() < Duration::from_secs(10), "bounded: {:?}", t0.elapsed());
    assert!(!h.connected());
    assert!(Peer::connect(&p).line().contains("hello"), "the place is free again");
    drop(c);
    let _ = std::fs::remove_file(&p);
}

/// MC-10: the access dialog over the socket: the question names the client, only an answer for that client
/// and id counts, Georg's own grant (no id) and its revoke reach the confirmer as `access` lines.
#[test]
fn full_access_over_the_socket() {
    use bitsinstall::access::{Access, Client};
    let p = socket("access");
    let a = fp_client('a');
    let paired = {
        let a = a.clone();
        move || vec![Client { fp: a.clone(), name: "bitsos-desk".into() }]
    };
    let access = Arc::new(Access::new(Vec::new(), None, Box::new(|| (T, 0)), Box::new(paired)));
    let state = Arc::new(State::new(Grants::default(), None, Duration::from_millis(800)).with_access(access.clone()));
    let h = Hub::new(state, anyone(), clock);
    h.serve(UnixListener::bind(&p).unwrap()).unwrap();
    let mut c = Peer::connect(&p);
    let hello = c.line();
    assert!(hello.contains("\"clients\":[{\"client\":\"") && hello.contains("bitsos-desk") && hello.contains("\"access\":[]"), "{hello}");
    wait_connected(&h, true);
    let client = Client { fp: a.clone(), name: "bitsos-desk".into() };
    let h2 = h.clone();
    let cl = client.clone();
    let asking = std::thread::spawn(move || h2.ask_access(11, &cl, 24, "Vollzugriff", "Text", Duration::from_secs(3)));
    let ask = c.line();
    assert!(ask.contains("\"kind\":\"access\"") && ask.contains(&a) && ask.contains("\"hours\":24"), "{ask}");
    // Another client's key or another id is refused, the right one counts.
    c.send(&confirm::access_answer(Some(11), &fp_client('b'), 24, false, Input::Physical));
    assert!(c.line().contains("refused"));
    c.send(&confirm::access_answer(Some(12), &a, 24, false, Input::Physical));
    assert!(c.line().contains("refused"));
    c.send(&confirm::access_answer(Some(11), &a, 8, true, Input::Physical));
    assert_eq!(asking.join().unwrap(), Outcome::Access { hours: 8, nvme: true, input: Input::Physical });
    // Georg's own grant from Strg+Alt+F, then injected (refused), then the revoke.
    c.send(&confirm::access_answer(None, &a, 1, false, Input::Physical));
    let line = c.line();
    assert!(line.contains("\"type\":\"access\"") && line.contains(&format!("\"until\":{}", T + 3600)), "{line}");
    assert_eq!(access.table().len(), 1);
    c.send(&confirm::access_answer(None, &a, 1, false, Input::Injected));
    assert!(c.line().contains("refused"));
    c.send(&confirm::revoke("access"));
    assert!(c.line().contains("\"access\":[]"));
    assert!(access.table().is_empty());
    // "all" ends full access too.
    c.send(&confirm::access_answer(None, &a, 24, false, Input::Physical));
    c.line();
    c.send(&confirm::revoke("all"));
    let (l1, l2) = (c.line(), c.line());
    assert!(l1.contains("\"access\":[]") && l2.contains("\"type\":\"grants\""), "{l1} {l2}");
    assert!(access.table().is_empty());
}

fn fp_client(c: char) -> String {
    c.to_string().repeat(64)
}
