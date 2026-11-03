//! MC-11: the text console as the fallback confirmer. After a cold start no desktop runs until Georg types
//! `bitscomp`, so a pairing or an access request found no confirmer and was refused ("no desktop session").
//! Now, when no confirmer holds the place, the question is shown on the text console (/dev/tty0) and answered
//! with the real keyboard only:
//! - the dialog says the same as bitscomp's (the installer's title and text, Erlauben/Ablehnen); N or Esc =
//!   Ablehnen; for full access the duration (1/8/2/7 = 1 h, 8 h, 24 h preselected, 7 Tage) and the NVMe box (P),
//!   as in bitscomp's access dialog;
//! - Erlauben is typing a code of `CODE_LEN` random letters the dialog shows (fixer, MC-11 review): no word typed
//!   for the shell and no look-alike prompt another program prints can produce it, and the code is new for
//!   every dialog;
//! - the code only counts once armed: 2 s after the dialog appeared and again 2 s after every real key pressed
//!   before that (bitscomp's `ACCESS_ARM`, here for every kind);
//! - only keys of real keyboards count (the device rule of bitscomp's `origin_of`: MCP's devices and every
//!   BUS_VIRTUAL device are injected, and the kernel drops events programs wrote into a real one,
//!   EVIOC_BITSOS_PHYSICAL); injected keys are ignored and logged, they neither confirm nor refuse;
//! - while the dialog runs it holds the real keyboards (EVIOCGRAB), so the shell gets none of its keys, and it
//!   repaints itself every `REPAINT` (clear screen, the whole dialog): text a program writes over it is gone again;
//! - a confirmer that connects while the console asks (bitscomp started) takes the question over with the time
//!   left, and so does a program taking the screen (the console cannot be seen any more): one asker at a time.
//!
//! The console's devices are the binary's (`tty.rs`, through `Console`/`Session`); this module is the rule.

use crate::access::Client;
use crate::decide::{Confirmer, Outcome};
use crate::confirm::Input;
use crate::log;
use mcp_wire::installer::Kind;
use std::sync::Arc;
use std::time::Duration;

/// Erlauben is armed this long after the dialog appeared and after every earlier real key (bitscomp's ACCESS_ARM).
pub const ARM: Duration = Duration::from_millis(2000);
/// How often a waiting console dialog looks whether a confirmer connected or the screen was taken.
pub const STEP: Duration = Duration::from_millis(250);
/// How often the dialog paints itself again (whatever a program wrote over it is gone after this).
pub const REPAINT: Duration = Duration::from_millis(1000);
/// Letters in the Erlauben code.
pub const CODE_LEN: usize = 4;
/// What starts every painting: cursor home, clear the screen (ECMA-48 CUP, ED).
pub const CLEAR: &str = "\x1b[H\x1b[2J";

/// Evdev key codes (linux-6.6/include/uapi/linux/input-event-codes.h): positions, the same on a German and a US
/// layout for these keys.
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_P: u16 = 25;
pub const KEY_J: u16 = 36;
pub const KEY_N: u16 = 49;
/// linux-6.6/include/uapi/linux/input.h.
pub const BUS_VIRTUAL: u16 = 0x06;

/// The code's letters and their evdev codes: the same key on a German and a US layout (no Y/Z), none of the
/// dialog's other keys (N, P, digits), no J (the old yes key) and no I/O (read as 1/0).
pub const CODE_LETTERS: [(u16, char); 19] = [
    (30, 'A'), (48, 'B'), (46, 'C'), (32, 'D'), (18, 'E'), (33, 'F'), (34, 'G'), (35, 'H'), (37, 'K'), (38, 'L'),
    (50, 'M'), (16, 'Q'), (19, 'R'), (31, 'S'), (20, 'T'), (22, 'U'), (47, 'V'), (17, 'W'), (45, 'X'),
];

fn letter(code: u16) -> Option<char> {
    CODE_LETTERS.iter().find(|l| l.0 == code).map(|l| l.1)
}

/// A dialog's code from a random number.
pub fn code_from(mut r: u64) -> Vec<u16> {
    (0..CODE_LEN)
        .map(|_| {
            let c = CODE_LETTERS[(r % CODE_LETTERS.len() as u64) as usize].0;
            r /= CODE_LETTERS.len() as u64;
            c
        })
        .collect()
}

/// 64 random bits from the kernel (/dev/urandom); None: the console does not ask (no guessable code).
pub fn random() -> Option<u64> {
    use std::io::Read;
    let mut b = [0u8; 8];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).ok()?;
    Some(u64::from_le_bytes(b))
}

/// The duration choices of the access dialog (bitscomp's `access_dialog::CHOICES`), their keys; `DEFAULT` first.
pub const CHOICES: [(u16, i64, &str); 4] = [(KEY_1, 1, "1 h"), (KEY_8, 8, "8 h"), (KEY_2, 24, "24 h"), (KEY_7, 168, "7 Tage")];
pub const DEFAULT: usize = 2;

/// Why a device's keys count as injected (bitscomp's `evdev_map::injected_why`, the same rule), None for a real one.
pub fn injected_why(bus: u16, name: &str, phys: &str) -> Option<&'static str> {
    if phys.starts_with("mcp/") || name.starts_with("BitsOS MCP") {
        Some("MCP device")
    } else if bus == BUS_VIRTUAL {
        Some("virtual bus")
    } else {
        None
    }
}

/// One key press and whether it came from a real keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: u16,
    pub physical: bool,
}

/// One dialog's screen and keyboards (held while it lives; dropping it gives the keyboards back).
pub trait Session {
    fn show(&mut self, text: &str);
    /// The next key press within `wait` (None: none came).
    fn key(&mut self, wait: Duration) -> Option<Key>;
    /// Whether the console can still be seen (no program has taken the screen).
    fn visible(&mut self) -> bool;
}

/// The text console: whether it can ask now, and a session for one dialog.
pub trait Console: Send + Sync {
    /// The screen is the text console and a real keyboard is there.
    fn usable(&self) -> bool;
    fn open(&self) -> Result<Box<dyn Session>, String>;
}

/// What one key did in a console dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing decided; `true`: what the dialog shows changed (the choice, the letters typed).
    Stay(bool),
    /// A code letter typed before it was armed: armed again later, the code starts again.
    Early,
    /// A letter that is not the code's next one: the code starts again.
    Wrong,
    No,
    Yes,
    /// An injected key: ignored.
    Ignored,
}

/// The state of one console dialog (times since any fixed start).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dialog {
    pub access: bool,
    pub armed_at: Duration,
    pub choice: usize,
    pub nvme: bool,
    /// The Erlauben code (evdev codes of `CODE_LETTERS`) and how many of its letters are typed.
    pub code: Vec<u16>,
    pub typed: usize,
}

impl Dialog {
    pub fn new(access: bool, now: Duration, code: Vec<u16>) -> Dialog {
        Dialog { access, armed_at: now + ARM, choice: DEFAULT, nvme: false, code, typed: 0 }
    }

    /// The code as shown, and the typed part with blanks ("A B _ _").
    pub fn code_text(&self) -> (String, String) {
        let all: Vec<String> = self.code.iter().map(|&c| letter(c).unwrap_or('?').to_string()).collect();
        let typed: Vec<String> = all.iter().enumerate().map(|(i, l)| if i < self.typed { l.clone() } else { String::from("_") }).collect();
        (all.join(" "), typed.join(" "))
    }

    pub fn armed(&self, now: Duration) -> bool {
        now >= self.armed_at
    }

    pub fn hours(&self) -> i64 {
        CHOICES[self.choice].1
    }

    pub fn key(&mut self, k: Key, now: Duration) -> Step {
        if !k.physical {
            return Step::Ignored;
        }
        if k.code == KEY_ESC || k.code == KEY_N {
            return Step::No;
        }
        let armed = self.armed(now);
        if !armed {
            // Every real key before the arm moves it (bitscomp: "again after every earlier real key").
            self.armed_at = now + ARM;
        }
        if letter(k.code).is_some() {
            if !armed {
                self.typed = 0;
                return Step::Early;
            }
            if self.code.get(self.typed) == Some(&k.code) {
                self.typed += 1;
                return if self.typed == self.code.len() { Step::Yes } else { Step::Stay(true) };
            }
            self.typed = usize::from(self.code.first() == Some(&k.code));
            return Step::Wrong;
        }
        if !self.access {
            return Step::Stay(false);
        }
        if let Some(i) = CHOICES.iter().position(|c| c.0 == k.code) {
            let changed = i != self.choice;
            self.choice = i;
            return Step::Stay(changed);
        }
        if k.code == KEY_P {
            self.nvme = !self.nvme;
            return Step::Stay(true);
        }
        Step::Stay(false)
    }

    /// The access dialog's choice line.
    pub fn choice_line(&self) -> String {
        let all: Vec<String> = CHOICES.iter().enumerate().map(|(i, c)| if i == self.choice { format!("[{}]", c.2) } else { String::from(c.2) }).collect();
        format!(
            "Dauer: {} (Tasten 1, 8, 2, 7)   P = auch interne Platte (NVMe) beschreiben: {}",
            all.join("  "),
            if self.nvme { "ja" } else { "nein" }
        )
    }
}

/// The text a dialog shows, never a control character from a client (the reason of a request is the client's
/// own text: no escape sequence of it reaches the terminal).
pub fn clean(t: &str) -> String {
    t.chars().map(|c| if c == '\n' || !c.is_control() { c } else { '?' }).collect()
}

const RULE: &str = "========================================================================";

/// The whole dialog as the console paints it (from a clear screen); `left` the time until it ends, `note` the
/// line about the last key (empty: none).
pub fn render(id: u64, title: &str, text: &str, d: &Dialog, left: Duration, note: &str) -> String {
    let mut s = format!("{CLEAR}{RULE}\r\nBitsOS fragt (Frage {id}): {}\r\n\r\n", clean(title));
    for line in clean(text).lines() {
        s.push_str(line);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    if d.access {
        s.push_str(&d.choice_line());
        s.push_str("\r\n");
    }
    let (code, typed) = d.code_text();
    s.push_str(&format!(
        "Erlauben: tippe den Code  {code}  (erst 2 s nach dem Erscheinen und nach jeder Taste davor).   Getippt: {typed}\r\nN oder Esc = Ablehnen. Nur die echte Tastatur zählt. Ohne Antwort in {} s heißt es Ablehnen.\r\n",
        left.as_secs()
    ));
    if !note.is_empty() {
        s.push_str(&format!("{}\r\n", clean(note)));
    }
    s.push_str(RULE);
    s.push_str("\r\n");
    s
}

/// The confirmer the installer asks: the connected one (bitscomp, or the test confirmer), else the text console.
pub struct Fallback<P> {
    primary: Arc<P>,
    console: Option<Box<dyn Console>>,
    /// Monotonic time and a sleep on it (tests: their own clock).
    clock: Box<dyn Fn() -> Duration + Send + Sync>,
    sleep: Box<dyn Fn(Duration) + Send + Sync>,
    /// The code's random number (tests: a fixed one); None: the console does not ask.
    random: Box<dyn Fn() -> Option<u64> + Send + Sync>,
}

impl<P: Confirmer> Fallback<P> {
    /// `console` None: no fallback (CONFIG.TXT mcp_confirmer=test: the scenarios' confirmations are scripted).
    pub fn new(primary: Arc<P>, console: Option<Box<dyn Console>>, clock: Box<dyn Fn() -> Duration + Send + Sync>) -> Fallback<P> {
        Fallback { primary, console, clock, sleep: Box::new(std::thread::sleep), random: Box::new(random) }
    }

    /// Tests: a fixed random number for the code.
    pub fn with_random(mut self, random: Box<dyn Fn() -> Option<u64> + Send + Sync>) -> Fallback<P> {
        self.random = random;
        self
    }

    /// Tests: a sleep that moves their clock.
    pub fn with_sleep(mut self, sleep: Box<dyn Fn(Duration) + Send + Sync>) -> Fallback<P> {
        self.sleep = sleep;
        self
    }

    /// Asks on the console; `handover` asks the primary with the time left once it connected.
    fn on_console(&self, id: u64, access: bool, title: &str, text: &str, timeout: Duration, handover: &dyn Fn(Duration) -> Outcome) -> Outcome {
        let Some(c) = self.console.as_ref().filter(|c| c.usable()) else { return Outcome::NoSession };
        let Some(r) = (self.random)() else {
            log::warn(&format!("dialog {id}: the text console cannot ask: no random code (/dev/urandom)"));
            return Outcome::NoSession;
        };
        let mut s = match c.open() {
            Ok(s) => s,
            Err(e) => {
                log::warn(&format!("dialog {id}: the text console cannot ask: {e}"));
                return Outcome::NoSession;
            }
        };
        let start = (self.clock)();
        let end = start + timeout;
        let mut d = Dialog::new(access, start, code_from(r));
        log::info(&format!("dialog {id}: no confirmer connected: asking at the text console ({} s, real keyboard only, Erlauben by a typed code)", timeout.as_secs()));
        let mut note = "";
        let mut painted = start;
        s.show(&render(id, title, text, &d, timeout, note));
        let mut injected = 0u32;
        loop {
            let now = (self.clock)();
            if now >= end {
                s.show("Keine Antwort in der Zeit: das heißt Ablehnen.\r\n");
                log::info(&format!("dialog {id}: no answer at the text console in time"));
                return Outcome::Timeout;
            }
            if self.primary.connected() || !s.visible() {
                let why = if self.primary.connected() { "a confirmer connected" } else { "a program took the screen" };
                s.show("Die Frage geht an die Oberfläche weiter.\r\n");
                drop(s);
                log::info(&format!("dialog {id}: {why}: the text console hands the question over"));
                return self.wait_primary(end, handover);
            }
            if now >= painted + REPAINT {
                painted = now;
                s.show(&render(id, title, text, &d, end - now, note));
            }
            let wait = STEP.min(end - now).min((painted + REPAINT).saturating_sub(now).max(Duration::from_millis(1)));
            let Some(k) = s.key(wait) else { continue };
            let now = (self.clock)();
            let step = d.key(k, now);
            match step {
                Step::Ignored => {
                    injected += 1;
                    if injected == 1 {
                        log::warn(&format!("dialog {id}: injected key {} at the text console ignored: only the real keyboard answers", k.code));
                    }
                }
                Step::Early | Step::Wrong | Step::Stay(true) => {
                    note = match step {
                        Step::Early => "Noch nicht scharf: den Code erst 2 s nach der letzten Taste tippen.",
                        Step::Wrong => "Falscher Buchstabe: den Code von vorn tippen.",
                        _ => "",
                    };
                    painted = now;
                    s.show(&render(id, title, text, &d, end.saturating_sub(now), note));
                }
                Step::Stay(false) => {}
                Step::No => {
                    s.show("Abgelehnt.\r\n");
                    log::info(&format!("dialog {id}: Ablehnen at the text console (physical)"));
                    return Outcome::Answer { yes: false, input: Input::Physical };
                }
                Step::Yes => {
                    log::info(&format!("dialog {id}: Erlauben at the text console (physical, code typed){}", if access { format!(", {} h, NVMe {}", d.hours(), d.nvme) } else { String::new() }));
                    s.show("Erlaubt.\r\n");
                    return if access { Outcome::Access { hours: d.hours(), nvme: d.nvme, input: Input::Physical } } else { Outcome::Answer { yes: true, input: Input::Physical } };
                }
            }
        }
    }

    /// After a handover: the primary asks with the time left, once it holds the place (polled every `STEP`).
    fn wait_primary(&self, end: Duration, handover: &dyn Fn(Duration) -> Outcome) -> Outcome {
        loop {
            let now = (self.clock)();
            if now >= end {
                return Outcome::Timeout;
            }
            if self.primary.connected() {
                return handover(end - now);
            }
            (self.sleep)(STEP.min(end - now));
        }
    }
}

impl<P: Confirmer> Confirmer for Fallback<P> {
    fn connected(&self) -> bool {
        self.primary.connected() || self.console.as_ref().is_some_and(|c| c.usable())
    }

    fn ask(&self, id: u64, kind: Kind, title: &str, text: &str, timeout: Duration) -> Outcome {
        if self.primary.connected() {
            return self.primary.ask(id, kind, title, text, timeout);
        }
        self.on_console(id, false, title, text, timeout, &|rest| self.primary.ask(id, kind, title, text, rest))
    }

    fn ask_access(&self, id: u64, client: &Client, hours: i64, title: &str, text: &str, timeout: Duration) -> Outcome {
        if self.primary.connected() {
            return self.primary.ask_access(id, client, hours, title, text, timeout);
        }
        self.on_console(id, true, title, text, timeout, &|rest| self.primary.ask_access(id, client, hours, title, text, rest))
    }

    fn access_changed(&self) {
        self.primary.access_changed();
    }
}
