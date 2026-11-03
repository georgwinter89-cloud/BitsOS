//! MC-11: the text console as the fallback confirmer, with a scripted keyboard and its own clock: Erlauben by
//! the typed code, the 2 s arm (moved by every earlier real key), real keys only, the access choices, the
//! repainting, the handover to a confirmer that connects while the console asks, and no console = no session.

use bitsinstall::access::Client;
use bitsinstall::confirm::Input;
use bitsinstall::console::{self, Console, Dialog, Fallback, KEY_1, KEY_2, KEY_ESC, KEY_J, KEY_N, KEY_P, Key, Session, Step, CLEAR};
use bitsinstall::decide::{Confirmer, Outcome, State};
use bitsinstall::grant::Grants;
use mcp_wire::Json;
use mcp_wire::installer::{Asked, Kind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct World {
    /// The clock in ms.
    ms: AtomicU64,
    /// (at ms, key): pressed in this order.
    keys: Mutex<Vec<(u64, Key)>>,
    shown: Mutex<String>,
    usable: AtomicBool,
    opened: AtomicUsize,
    /// The screen is taken at this ms (0: never).
    taken_at: AtomicU64,
    /// The primary connects at this ms (0: never).
    connects_at: AtomicU64,
}

impl World {
    fn now(&self) -> u64 {
        self.ms.load(Ordering::SeqCst)
    }
}

struct FakeSession(Arc<World>);
impl Session for FakeSession {
    fn show(&mut self, text: &str) {
        self.0.shown.lock().unwrap().push_str(text);
    }
    fn key(&mut self, wait: Duration) -> Option<Key> {
        let w = &self.0;
        let until = w.now() + wait.as_millis() as u64;
        let mut keys = w.keys.lock().unwrap();
        match keys.first().copied() {
            Some((at, k)) if at <= until => {
                keys.remove(0);
                w.ms.store(at.max(w.now()), Ordering::SeqCst);
                Some(k)
            }
            _ => {
                w.ms.store(until, Ordering::SeqCst);
                None
            }
        }
    }
    fn visible(&mut self) -> bool {
        let t = self.0.taken_at.load(Ordering::SeqCst);
        t == 0 || self.0.now() < t
    }
}

struct FakeConsole(Arc<World>);
impl Console for FakeConsole {
    fn usable(&self) -> bool {
        self.0.usable.load(Ordering::SeqCst)
    }
    fn open(&self) -> Result<Box<dyn Session>, String> {
        self.0.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeSession(self.0.clone())))
    }
}

/// bitscomp's stand-in: connected from `connects_at`, answers yes (and 8 h for access), records the time it got.
struct Primary {
    w: Arc<World>,
    asked: Mutex<Vec<Duration>>,
}
impl Confirmer for Primary {
    fn connected(&self) -> bool {
        let t = self.w.connects_at.load(Ordering::SeqCst);
        t != 0 && self.w.now() >= t
    }
    fn ask(&self, _: u64, _: Kind, _: &str, _: &str, timeout: Duration) -> Outcome {
        self.asked.lock().unwrap().push(timeout);
        Outcome::Answer { yes: true, input: Input::Physical }
    }
    fn ask_access(&self, _: u64, _: &Client, _: i64, _: &str, _: &str, timeout: Duration) -> Outcome {
        self.asked.lock().unwrap().push(timeout);
        Outcome::Access { hours: 8, nvme: false, input: Input::Physical }
    }
}

fn real(code: u16) -> Key {
    Key { code, physical: true }
}

fn setup(keys: &[(u64, Key)]) -> (Arc<World>, Arc<Primary>, Fallback<Primary>) {
    let w = Arc::new(World::default());
    w.usable.store(true, Ordering::SeqCst);
    *w.keys.lock().unwrap() = keys.to_vec();
    let p = Arc::new(Primary { w: w.clone(), asked: Mutex::new(Vec::new()) });
    let wc = w.clone();
    let ws = w.clone();
    let f = Fallback::new(p.clone(), Some(Box::new(FakeConsole(w.clone()))), Box::new(move || Duration::from_millis(wc.now())))
        .with_sleep(Box::new(move |d| {
            ws.ms.fetch_add(d.as_millis() as u64, Ordering::SeqCst);
        }))
        .with_random(Box::new(|| Some(R)));
    (w, p, f)
}

/// The random number whose code is B C D E (base 19: 1, 2, 3, 4).
const R: u64 = 1 + 2 * 19 + 3 * 361 + 4 * 6859;
const B: u16 = 48;
const C: u16 = 46;
const D: u16 = 32;
const E: u16 = 18;
const X: u16 = 45;

/// The code typed from `at`, 100 ms apart.
fn code_at(at: u64) -> Vec<(u64, Key)> {
    [B, C, D, E].iter().enumerate().map(|(i, &c)| (at + 100 * i as u64, real(c))).collect()
}

fn keys(parts: &[Vec<(u64, Key)>]) -> Vec<(u64, Key)> {
    parts.concat()
}

const MIN: Duration = Duration::from_secs(60);

fn ask(f: &Fallback<Primary>) -> Outcome {
    f.ask(7, Kind::Pair, "Neuen Gaming-PC koppeln", "Werkzeug: pair_client", MIN)
}

#[test]
fn erlauben_is_the_typed_code_armed_two_seconds_after_the_dialog_and_after_every_earlier_key() {
    assert_eq!(console::code_from(R), vec![B, C, D, E]);
    // B at 0.5 s is early and moves the arm to 2.5 s; the code typed from 2.6 s counts.
    let (w, _, f) = setup(&keys(&[vec![(500, real(B))], code_at(2600)]));
    assert!(f.connected(), "the console can ask");
    assert_eq!(ask(&f), Outcome::Answer { yes: true, input: Input::Physical });
    assert_eq!(w.now(), 2900);
    let shown = w.shown.lock().unwrap().clone();
    assert!(shown.contains("BitsOS fragt (Frage 7): Neuen Gaming-PC koppeln") && shown.contains("Werkzeug: pair_client"), "{shown}");
    assert!(shown.contains("Erlauben: tippe den Code  B C D E") && shown.contains("N oder Esc = Ablehnen") && shown.contains("Ohne Antwort in 60 s"), "{shown}");
    assert!(shown.contains("Getippt: B C _ _") && shown.contains("Noch nicht scharf"), "{shown}");
    // Any other real key before the arm moves it too: 1.9 s -> 3.9 s; the code from 3.8 s is early letter by letter
    // (each moves the arm again, the last to 6.1 s), the code from 6.2 s counts.
    let (w, _, f) = setup(&keys(&[vec![(1900, real(KEY_1))], code_at(3800), code_at(6200)]));
    assert_eq!(ask(&f), Outcome::Answer { yes: true, input: Input::Physical });
    assert_eq!(w.now(), 6500);
}

#[test]
fn a_single_key_or_a_wrong_letter_never_says_yes() {
    // J (the old yes key), Enter, a word typed for the shell: no yes, the minute runs out.
    let (_, _, f) = setup(&[(3000, real(KEY_J)), (3100, real(28)), (3200, real(B)), (3300, real(C)), (3400, real(X)), (3500, real(D)), (3600, real(E))]);
    assert_eq!(ask(&f), Outcome::Timeout);
    // A wrong letter starts the code again; typed again in full it counts.
    let (w, _, f) = setup(&keys(&[vec![(3000, real(B)), (3100, real(X))], code_at(3200)]));
    assert_eq!(ask(&f), Outcome::Answer { yes: true, input: Input::Physical });
    assert!(w.shown.lock().unwrap().contains("Falscher Buchstabe"));
    // Every dialog gets its own code.
    assert_ne!(console::code_from(1), console::code_from(2));
}

#[test]
fn the_dialog_paints_itself_again_every_second() {
    let (w, _, f) = setup(&[]);
    assert_eq!(ask(&f), Outcome::Timeout);
    let shown = w.shown.lock().unwrap().clone();
    assert!(shown.matches(CLEAR).count() >= 55, "{}", shown.matches(CLEAR).count());
    assert!(shown.contains("Ohne Antwort in 30 s"), "the time left counts down");
}

#[test]
fn without_a_random_code_the_console_does_not_ask() {
    let (w, _, f) = setup(&code_at(2500));
    let f = f.with_random(Box::new(|| None));
    assert_eq!(ask(&f), Outcome::NoSession);
    assert_eq!(w.opened.load(Ordering::SeqCst), 0);
}

#[test]
fn n_or_esc_refuse_at_once_and_no_answer_is_a_timeout() {
    let (_, _, f) = setup(&[(100, real(KEY_N))]);
    assert_eq!(ask(&f), Outcome::Answer { yes: false, input: Input::Physical });
    let (_, _, f) = setup(&[(100, real(KEY_ESC))]);
    assert_eq!(ask(&f), Outcome::Answer { yes: false, input: Input::Physical });
    let (w, _, f) = setup(&[]);
    assert_eq!(ask(&f), Outcome::Timeout);
    assert_eq!(w.now(), 60_000);
    assert!(w.shown.lock().unwrap().contains("das heißt Ablehnen"));
}

#[test]
fn injected_keys_neither_confirm_nor_refuse() {
    let inj = |code| Key { code, physical: false };
    let (w, _, f) = setup(&[(3000, inj(B)), (3100, inj(C)), (3200, inj(D)), (3300, inj(E)), (3400, inj(KEY_N)), (3500, inj(KEY_ESC))]);
    assert_eq!(ask(&f), Outcome::Timeout, "only the real keyboard answers");
    assert_eq!(w.now(), 60_000);
    // And through the installer's rule: an injected key never makes a yes.
    let mut d = Dialog::new(false, Duration::ZERO, vec![B, C, D, E]);
    let at = Duration::from_secs(9);
    assert_eq!(d.key(inj(B), at), Step::Ignored);
    assert_eq!((d.key(real(B), at), d.key(real(C), at), d.key(real(D), at)), (Step::Stay(true), Step::Stay(true), Step::Stay(true)));
    assert_eq!(d.key(inj(E), at), Step::Ignored);
    assert_eq!(d.key(real(E), at), Step::Yes);
}

#[test]
fn the_access_dialog_takes_the_duration_and_the_nvme_box() {
    let (w, _, f) = setup(&keys(&[vec![(300, real(KEY_1)), (600, real(KEY_P))], code_at(2700)]));
    let c = Client { fp: "ab".repeat(32), name: String::from("bitsos-desk test") };
    assert_eq!(f.ask_access(9, &c, 24, "Vollzugriff für den Gaming-PC", "Der Gaming-PC bittet um Vollzugriff für 24 h.", MIN), Outcome::Access { hours: 1, nvme: true, input: Input::Physical });
    let shown = w.shown.lock().unwrap().clone();
    assert!(shown.contains("Dauer: 1 h  8 h  [24 h]  7 Tage") && shown.contains("Dauer: [1 h]  8 h  24 h  7 Tage"), "{shown}");
    assert!(shown.contains("(NVMe) beschreiben: ja"), "{shown}");
    // Preselected as in bitscomp: 24 h, NVMe off; 2 keeps 24 h.
    let (_, _, f) = setup(&keys(&[vec![(2500, real(KEY_2))], code_at(4600)]));
    assert_eq!(f.ask_access(9, &c, 1, "t", "x", MIN), Outcome::Access { hours: 24, nvme: false, input: Input::Physical });
}

#[test]
fn a_confirmer_that_connects_takes_the_question_over_with_the_time_left() {
    let (w, p, f) = setup(&[]);
    w.connects_at.store(10_000, Ordering::SeqCst);
    assert_eq!(ask(&f), Outcome::Answer { yes: true, input: Input::Physical });
    assert_eq!(*p.asked.lock().unwrap(), vec![Duration::from_secs(50)], "the rest of the minute");
    assert!(w.shown.lock().unwrap().contains("geht an die Oberfläche weiter"));
    // Connected before the question: the console is not even opened.
    let (w, p, f) = setup(&[(100, real(KEY_N))]);
    w.connects_at.store(1, Ordering::SeqCst);
    w.ms.store(5, Ordering::SeqCst);
    assert_eq!(ask(&f), Outcome::Answer { yes: true, input: Input::Physical });
    assert_eq!((w.opened.load(Ordering::SeqCst), p.asked.lock().unwrap().len()), (0, 1));
}

#[test]
fn a_program_taking_the_screen_ends_the_console_dialog() {
    // The screen is taken at 5 s and no confirmer comes: the dialog ends as a timeout, keys after that are not read.
    let (w, p, f) = setup(&[(20_000, real(KEY_J))]);
    w.taken_at.store(5_000, Ordering::SeqCst);
    assert_eq!(ask(&f), Outcome::Timeout);
    assert!(p.asked.lock().unwrap().is_empty());
    assert_eq!(w.keys.lock().unwrap().len(), 1, "the J after the screen was taken was not taken as an answer");
}

fn asked(tool: &str) -> Asked {
    Asked { id: 4, tool: tool.into(), kind: Kind::of(tool), args: Json::Obj(Vec::new()), timeout_ms: 1000, client: String::new(), peer: "10.0.2.2:4000".into() }
}

#[test]
fn without_a_usable_console_nothing_changes_and_with_one_the_installer_asks_there() {
    let s = State::new(Grants::default(), None, MIN);
    let (w, _, f) = setup(&code_at(2500));
    w.usable.store(false, Ordering::SeqCst);
    assert!(!f.connected());
    let d = s.decide(&f, &asked("reload"), 1_800_000_000);
    assert_eq!((d.yes, d.by.as_str()), (false, "no_session"));
    assert_eq!(w.opened.load(Ordering::SeqCst), 0);
    w.usable.store(true, Ordering::SeqCst);
    let d = s.decide(&f, &asked("reload"), 1_800_000_000);
    assert_eq!((d.yes, d.by.as_str()), (true, "dialog"), "{d:?}");
    // No console configured (mcp_confirmer=test): as before.
    let p = Arc::new(Primary { w: w.clone(), asked: Mutex::new(Vec::new()) });
    let none = Fallback::new(p, None, Box::new(|| Duration::ZERO));
    assert!(!none.connected());
    assert_eq!(none.ask(1, Kind::Reload, "t", "x", MIN), Outcome::NoSession);
}

#[test]
fn the_text_carries_no_control_characters_and_the_device_rule_is_bitscomps() {
    assert_eq!(console::clean("a\x1b[2Jb\nc\rd\u{9b}e"), "a?[2Jb\nc?d?e");
    let d = Dialog::new(false, Duration::ZERO, vec![B, C, D, E]);
    let r = console::render(3, "T\x1b]0;x\x07", "Text vom Gaming-PC: \x1b[31mrot", &d, MIN, "\x1b[2J");
    let body = r.strip_prefix(CLEAR).expect("each painting starts from a clear screen");
    assert!(!body.contains('\x1b') && !body.contains('\x07'), "{r:?}");
    assert_eq!(console::injected_why(console::BUS_VIRTUAL, "BitsOS MCP keyboard", "mcp/input0"), Some("MCP device"));
    assert_eq!(console::injected_why(0x03, "QEMU USB Keyboard", "usb-0000:00:04.0-1/input0"), None);
    assert_eq!(console::injected_why(console::BUS_VIRTUAL, "virtio keyboard", ""), Some("virtual bus"));
    assert_eq!(console::injected_why(0x11, "AT Translated Set 2 keyboard", "isa0060/serio0/input0"), None);
}
