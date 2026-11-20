//! The measurement run (`gpu-messung`, Georg 19.11.2026): the whole chain on the real card once, started by Georg
//! at the real keyboard, every fact the desk needs to confirm or refute the work written to a protocol on the
//! boot partition, then the PC switched off so the card loses power before CachyOS starts
//! (plan/spec/gpu-rdna4.md section 6, "The measurement run").
//!
//! What the run does, in order: the preflight (`messcheck`: the card, its driver path, the attach, every firmware
//! file; the protocol written and read back; the look `alive` reading `C2PMSG_81` 0), the looks before the first
//! stage, then `Stage::CHAIN` and the engines' rungs one stage at a time through the one path `gx_bringup` uses
//! (`Card::stage`: `livestage::run_one` on the card, `stage::run` on the fake), each recorded whole and flushed to
//! the disk before the next begins, with the look `state` between two stages; the looks at the end, the last
//! line, and the power-off, which always comes once the run began.
//!
//! The stop rule (`next`): a stage that stopped, a call refused before its stage, a write the stage's own list
//! refused, a `linuxseq` line that says the stage's accesses differ from Linux's or could not be compared, or a
//! trail that lost accesses ends the run there, and no later stage touches the card; so does a protocol write that
//! fails (the next stage's evidence would be lost). Answers of the card that are not the trace's are recorded,
//! and the run goes on when the stage passed. The run is bounded: each stage by its own bounds (Linux's), the
//! whole by `LIMIT_US`, checked between stages. Nothing waits for a key: the screen may be dark from the firmware
//! load on. Wave 44: an alert of the card's thermal taker during the run, or a temperature near a limit, stops it
//! too (`messtherm`).

use super::chainseq::Said;
use super::depth;
use super::messcheck::Facts;
use super::messproto as proto;
use super::messrun::Run;
use super::regs14::Regs14;
use super::session::Session;
use super::stage::{Done, Stage};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The stages of the run: the chain, then the copy engines' and the rings' rungs.
pub const RUN: [Stage; 21] = [
    Stage::Nbif,
    Stage::Gmc,
    Stage::Ih,
    Stage::Sos,
    Stage::Ring,
    Stage::Toc,
    Stage::Tmr,
    Stage::Fw,
    Stage::Autoload,
    Stage::FwRest,
    Stage::Ta,
    Stage::Pmfw,
    Stage::GcBoot,
    Stage::GcHub,
    Stage::GcCp,
    Stage::Mes,
    Stage::MesQ,
    Stage::GcRing,
    Stage::Sdma,
    Stage::SdmaTest,
    Stage::RingTest,
];

/// The whole run's bound, checked before each stage (a stage cannot be interrupted; its own waits are Linux's).
/// A good card takes well under a second of card time and about 60 flushed writes (three per stage: its START,
/// its record with the next look's START, the look): the run with its 15 s countdown ends in about a minute.
pub const LIMIT_US: u64 = 90_000_000;
/// How long the screen says the PC switches itself off before it does.
pub const OFF_IN_S: u64 = 15;
/// The protocol's form; the evaluator refuses another.
pub const FORMAT: u32 = 1;

/// One stage call as the run records it.
pub struct Took {
    pub d: Done,
    /// The answer exactly as `gx_bringup` words it.
    pub text: String,
    /// The lowest stack address at which the stage reached the card (`usize::MAX`: never).
    pub lowest: usize,
    pub us: u64,
}

/// The card: on the real machine `messlive`, in the tests `messfake`.
pub trait Card {
    /// What the preflight judges, read only.
    fn facts(&mut self) -> Facts;
    /// One stage or look through the one path `gx_bringup` uses. `Err`: refused before the stage ran.
    fn stage(&mut self, st: Stage) -> Result<Took, String>;
    /// The read-only `gpu` snapshot.
    fn snapshot(&mut self) -> String;
    /// The card's register table and its VRAM size, once attached.
    fn regs(&mut self) -> Option<(Regs14, u64)>;
    /// The session between two stages (none runs then), read only.
    fn session(&mut self, f: &mut dyn FnMut(&Session));
    /// Wave 44: the card's limits and one read of its temperatures through the thermal taker's gate.
    fn thermal(&mut self) -> super::thermal::Look;
}

/// Where the protocol goes. Every call returns only once the text is on the disk.
pub trait Disk {
    /// A new file holding `head`, written, flushed and read back: its name.
    fn open(&mut self, head: &str) -> Result<String, String>;
    /// `text` appended and flushed.
    fn put(&mut self, text: &str) -> Result<(), String>;
}

/// What the run needs of the machine besides the card.
pub trait Env {
    fn now_us(&mut self) -> u64;
    /// The kernel's log lines since the last call: Warn and above, and the card's subsystem at info.
    fn log(&mut self) -> Vec<String>;
    /// The log levels in force (what is stored at all).
    fn levels(&mut self) -> String;
    /// The kernel's version, as the boot shows it.
    fn build(&mut self) -> String;
    /// One line on the screen.
    fn say(&mut self, line: &str);
    fn wait_s(&mut self, s: u64);
    /// Switches the machine off: returns only when it did not, with the reason.
    fn poweroff(&mut self) -> String;
}

/// How the run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub file: Option<String>,
    /// The stages that ran, in order (a refused call counts: it was asked).
    pub ran: Vec<Stage>,
    /// Where it stopped and why (German), `None` when every stage ran and passed.
    pub stopped: Option<(String, String)>,
    /// The power-off did not happen: why.
    pub off_failed: Option<String>,
}

/// What the stop rule says after a stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Go,
    /// The stage passed, its accesses were Linux's, and this many of the card's answers were not the trace's.
    GoWithAnswers(usize),
    /// (plain German, the detail).
    Stop(String, String),
}

/// The stop rule (module head).
pub fn next(r: &Result<Took, String>) -> Next {
    let t = match r {
        Ok(t) => t,
        Err(e) => return Next::Stop(String::from("die Stufe wurde nicht gestartet"), e.clone()),
    };
    let d = &t.d;
    if !d.pass {
        let why = d.lines.iter().rev().find(|l| l.starts_with("stopped: ")).cloned().unwrap_or_else(|| d.line.clone());
        return Next::Stop(String::from("die Stufe hat angehalten"), why);
    }
    // A write outside the stage's list is the driver's own doing: the guard kept it from the card, the stage may
    // have gone on without it, and what follows stands on a state Linux's driver never makes.
    if let Some(first) = d.refused.first() {
        return Next::Stop(String::from("die Stufe wollte etwas schreiben, das ihre Liste nicht erlaubt"), format!("{} refused, the first {first:?}", d.refused.len()));
    }
    if d.trail_lost != 0 {
        return Next::Stop(String::from("die Mitschrift der Stufe ist unvollständig, sie beweist nichts"), format!("{} accesses lost", d.trail_lost));
    }
    match d.seq.as_ref() {
        None => Next::Stop(String::from("die Stufe wurde nicht mit Linux verglichen"), String::from("no linuxseq line")),
        Some(v) => match v.said {
            Said::Differs => Next::Stop(String::from("die Stufe weicht von Linux ab"), v.line.clone()),
            Said::NotCompared | Said::AsFar => Next::Stop(String::from("die Stufe wurde nicht mit Linux verglichen"), v.line.clone()),
            Said::Equal | Said::SoFar if v.answers > 0 => Next::GoWithAnswers(v.answers),
            Said::Equal | Said::SoFar => Next::Go,
        },
    }
}

/// The run. `Georg` pressed J: from here nothing waits for a key, and the power-off comes in every case.
pub fn go(card: &mut dyn Card, disk: &mut dyn Disk, env: &mut dyn Env) -> Outcome {
    let top = depth::stack_at();
    let t0 = env.now_us();
    let mut out = Outcome { file: None, ran: Vec::new(), stopped: None, off_failed: None };
    env.say("Die Messung beginnt. Bitte keine Taste drücken; der Bildschirm kann gleich dunkel werden.");
    let facts = card.facts();
    let build = env.build();
    let levels = env.levels();
    let head = proto::head(&facts, &build, &levels);
    match disk.open(&head) {
        Ok(name) => {
            env.say(&format!("Das Protokoll ist {name} im Ordner BITSOS2\\REPORT des Startdatenträgers."));
            out.file = Some(name);
        }
        Err(e) => {
            // Nothing of the card was touched: the protocol could not be written, so nothing is run.
            stopped(&mut out, PREFLIGHT, "das Protokoll kann nicht geschrieben werden", &e);
            env.say(screen(PROTOCOL));
            env.say(&format!("Grund: {e}"));
            return off(out, env);
        }
    }
    let mut r = Run::new(disk, env, top, t0);
    if let Err((check, why)) = r.preflight(card, &facts) {
        stopped(&mut out, PREFLIGHT, &why, "");
        r.env.say(screen(check));
        r.env.say("Auf die Karte wurde nichts geschrieben.");
        r.end(card, &mut out);
        return off(out, r.env);
    }
    let mut watch = super::messtherm::Watch::new();
    for (i, st) in RUN.into_iter().enumerate() {
        if r.since_ms() * 1000 > LIMIT_US {
            let why = format!("die Zeitgrenze von {} s ist vor der Stufe {} überschritten", LIMIT_US / 1_000_000, st.name());
            let _ = r.put(&format!("STOPP {} {why} | limit\n", st.name()));
            stopped(&mut out, st.name(), &why, "limit");
            break;
        }
        let ms = r.since_ms();
        // On the disk before the stage runs, or the stage does not run: a hang in it would leave nothing.
        if let Err(e) = r.put(&format!("## Stufe {} von {}: {} ({})\nSTART {} nr={} t_ms={ms}\n", i + 1, RUN.len(), st.name(), st.german(), st.name(), i + 1)) {
            stopped(&mut out, st.name(), DISK_LOST, &e);
            break;
        }
        let took = card.stage(st);
        out.ran.push(st);
        let mut decision = next(&took);
        let mut text = r.record(card, st, &took);
        if !matches!(decision, Next::Stop(..)) {
            let (t, hot) = watch.after(card, st);
            text.push_str(&t);
            if let Some((plain, detail)) = hot {
                decision = Next::Stop(plain, detail);
            }
        }
        match &decision {
            Next::Go => text.push_str(&format!("WEITER {} gleich wie Linux\n", st.name())),
            Next::GoWithAnswers(n) => text.push_str(&format!("WEITER {} {n} Antworten der Karte sind nicht die der Mitschrift; die Stufe selbst ist gleich wie Linux\n", st.name())),
            Next::Stop(plain, detail) => text.push_str(&format!("STOPP {} {plain} | {detail}\n", st.name())),
        }
        if let Next::Stop(plain, detail) = decision {
            let _ = r.put(&text);
            stopped(&mut out, st.name(), &plain, &detail);
            break;
        }
        // The stage's record and the look's START reach the disk in one write, before the look reads.
        if let Err(e) = r.look(card, Stage::State, st.name(), text) {
            stopped(&mut out, st.name(), DISK_LOST, &e);
            break;
        }
    }
    if !out.ran.is_empty() {
        let text = watch.end(card);
        let _ = r.put(&text);
    }
    r.end(card, &mut out);
    off(out, r.env)
}

/// `Outcome::stopped`'s place for a stop before the first stage.
pub const PREFLIGHT: &str = "vorpruefung";
/// The preflight's name for a protocol that cannot be made (`screen`).
pub const PROTOCOL: &str = "protokoll";
const DISK_LOST: &str = "das Protokoll ließ sich nicht weiterschreiben: keine weitere Stufe berührt die Karte";

/// What the screen says for a preflight check that failed (plain German; the detail is the protocol's).
pub fn screen(check: &str) -> &'static str {
    match check {
        "karte" => "Vorprüfung: die Grafikkarte ist nicht die erwartete RX 9070 XT an ihrem Platz.",
        "anzeige" => "Vorprüfung: die Grafikkarte zeigt das Bild nicht; gemessen wird nur die Karte am Bildschirm.",
        "treiber" => "Vorprüfung: die Grafikkarte gehört gerade nicht dem Treiber von BitsOS.",
        "anschluss" => "Vorprüfung: die Grafikkarte ließ sich beim Start von BitsOS nicht lesen.",
        "firmware" => "Vorprüfung: eine Firmware-Datei der Grafikkarte fehlt auf dem Startdatenträger oder ist beschädigt.",
        "kalt" => "Vorprüfung: die Grafikkarte war nicht frisch eingeschaltet, sie lief schon.",
        "kalt-unlesbar" => "Vorprüfung: die Grafikkarte antwortet nicht auf die erste Frage.",
        PROTOCOL => "Vorprüfung: die Datei für das Protokoll lässt sich auf dem Startdatenträger nicht anlegen.",
        _ => "Vorprüfung nicht bestanden.",
    }
}

fn stopped(out: &mut Outcome, at: &str, plain: &str, detail: &str) {
    if out.stopped.is_none() {
        let why = if detail.is_empty() { String::from(plain) } else { format!("{plain} ({detail})") };
        out.stopped = Some((String::from(at), why));
    }
}

/// How the run went, in one plain line for the last screen.
fn verdict(out: &Outcome) -> String {
    match &out.stopped {
        None => String::from("Alle Schritte bestanden."),
        Some((at, _)) if at == PREFLIGHT => String::from("Angehalten bei der Vorprüfung. Näheres steht in der Datei."),
        Some((at, _)) => match RUN.iter().position(|s| s.name() == at) {
            Some(i) => format!("Angehalten beim Schritt {} von {}: {}. Näheres steht in der Datei.", i + 1, RUN.len(), RUN[i].german()),
            None => String::from("Angehalten. Näheres steht in der Datei."),
        },
    }
}

/// The screen's last words and the power-off. The quiesce hooks of the card's stages run inside it.
fn off(mut out: Outcome, env: &mut dyn Env) -> Outcome {
    env.say(&verdict(&out));
    env.say(&format!("Die Messung ist zu Ende. Der PC schaltet sich in {OFF_IN_S} Sekunden selbst aus."));
    env.say("Danach bitte CachyOS starten; dort liest Claude das Protokoll aus.");
    env.wait_s(OFF_IN_S);
    let why = env.poweroff();
    env.say(&format!("Der PC hat sich nicht ausgeschaltet ({why}). Bitte den Einschaltknopf gedrückt halten, bis er aus ist."));
    out.off_failed = Some(why);
    out
}
