//! MC-10 fixer: the gate loop keeps the kernel's full-access table current while a dialog waits for Georg. A
//! fake gate hands one request whose dialog stays open for seconds; a revoke at the keyboard in that time must
//! reach the kernel (`grants` without the client) within about a second, and the decision still arrives.

use bitsinstall::access::{Access, Client};
use bitsinstall::confirm::Input;
use bitsinstall::decide::{Confirmer, Outcome, State};
use bitsinstall::gate_loop;
use bitsinstall::grant::Grants;
use bitsinstall::link::Link;
use mcp_wire::Json;
use mcp_wire::installer::{Asked, Kind, encode_asked};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const T: i64 = 1_800_000_000;

fn now() -> i64 {
    T
}

#[derive(Default)]
struct Seen {
    /// (when, the `grants` command's text)
    tables: Vec<(Instant, String)>,
    decided: Vec<String>,
}

struct FakeGate {
    asked: bool,
    seen: Arc<Mutex<Seen>>,
}

impl Link for FakeGate {
    fn command(&mut self, line: &[u8]) -> Result<String, i32> {
        let t = String::from_utf8_lossy(line).into_owned();
        if let Some(ms) = t.strip_prefix("next ") {
            if !self.asked {
                self.asked = true;
                let a = Asked { id: 1, tool: "install".into(), kind: Kind::Install, args: Json::parse(r#"{"path":"/bin/x"}"#).unwrap(), timeout_ms: 60_000, peer: "10.0.2.2:4000".into(), client: String::new() };
                return Ok(encode_asked(&a));
            }
            std::thread::sleep(Duration::from_millis(ms.trim().parse().unwrap()));
            return Ok(String::from("none"));
        }
        if t.starts_with("grants ") {
            self.seen.lock().unwrap().tables.push((Instant::now(), t));
            return Ok(String::from("{\"taken\":0,\"refused\":[]}"));
        }
        if t.starts_with("decide ") {
            self.seen.lock().unwrap().decided.push(t);
            return Ok(String::new());
        }
        Ok(String::new())
    }
}

/// A confirmer whose dialog stays open for `hold` and then says no.
struct Slow {
    hold: Duration,
}
impl Confirmer for Slow {
    fn ask(&self, _: u64, _: Kind, _: &str, _: &str, _: Duration) -> Outcome {
        std::thread::sleep(self.hold);
        Outcome::Answer { yes: false, input: Input::Physical }
    }
    fn connected(&self) -> bool {
        true
    }
}

#[test]
fn a_revoke_reaches_the_kernel_while_a_dialog_is_open() {
    let fp = "a".repeat(64);
    let f2 = fp.clone();
    let access = Arc::new(Access::new(Vec::new(), None, Box::new(|| (T, 0)), Box::new(move || vec![Client { fp: f2.clone(), name: "bitsos-desk".into() }])));
    access.grant(&fp, 24, true, Input::Physical).unwrap();
    let state = Arc::new(State::new(Grants::default(), None, Duration::from_secs(10)).with_access(access.clone()));
    let seen = Arc::new(Mutex::new(Seen::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, st2, a2, stop2) = (seen.clone(), state.clone(), access.clone(), stop.clone());
    let t = std::thread::spawn(move || {
        let mut g = FakeGate { asked: false, seen: s2 };
        gate_loop::run(&mut g, st2, a2, Arc::new(Slow { hold: Duration::from_secs(4) }), now, &stop2)
    });
    // The grant reaches the kernel first, then the dialog opens (the decider waits 4 s).
    let until = Instant::now() + Duration::from_secs(3);
    while seen.lock().unwrap().tables.is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(seen.lock().unwrap().tables.last().is_some_and(|(_, t)| t.contains(&fp)), "the grant was pushed");
    std::thread::sleep(Duration::from_millis(500));
    assert!(seen.lock().unwrap().decided.is_empty(), "the dialog is still open");
    let revoked = Instant::now();
    assert_eq!(access.revoke(), 1);
    let until = revoked + Duration::from_secs(2);
    let reached = loop {
        if let Some((at, _)) = seen.lock().unwrap().tables.iter().find(|(at, t)| *at >= revoked && !t.contains(&fp)) {
            break Some(*at);
        }
        if Instant::now() >= until {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let at = reached.expect("the revoke did not reach the kernel while the dialog was open");
    assert!(at.duration_since(revoked) < Duration::from_millis(1000), "late: {:?}", at.duration_since(revoked));
    assert!(seen.lock().unwrap().decided.is_empty(), "the revoke came before the dialog's answer");
    // The dialog's answer still arrives.
    let until = Instant::now() + Duration::from_secs(6);
    while seen.lock().unwrap().decided.is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    let d = seen.lock().unwrap().decided.clone();
    assert!(d.len() == 1 && d[0].contains("\"id\":1") && d[0].contains("\"yes\":false"), "{d:?}");
    stop.store(true, Ordering::Release);
    assert_eq!(t.join().unwrap(), Ok(()));
}
