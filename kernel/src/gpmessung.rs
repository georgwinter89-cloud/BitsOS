//! `gpu-messung` (Georg 19.11.2026): the measurement run of the Gaming-PC's RX 9070 XT. Two ways ask for it, and
//! both enter `start`: the monitor's command (`bits>`, `physical_only`: a line with a key MCP injected is refused)
//! and `/bin/gpu-messung` at busybox's shell (`/ #`), which writes `start` to `/dev/bitsos/gpu-messung` (`node`):
//! on the Gaming-PC the shell holds the console, so the monitor never gets it. The guard is the same for both:
//! `start` says what it will do and asks for J, and only a key of a real keyboard answers (`console::text_ask`:
//! keys MCP or a program injected and those of a virtual device are taken and never answer). So no program, MCP's
//! `run` included, and no CONFIG key or boot step can start the card work without Georg's J at the machine; the
//! node also refuses a process MCP's run started at its open, as the monitor refuses a line with an injected key.
//! After the J it runs `amdgpu::rdna4::messung` on the card: the protocol goes to the boot partition
//! (`\BITSOS2\REPORT\GPUMnnnn.TXT`, a new number per run, written and flushed after every stage), and the PC
//! switches itself off at the end through the kernel's ACPI power-off (`console::reset::poweroff`), so the card
//! loses power before CachyOS starts (plan/spec/gpu-rdna4.md section 6, "The measurement run").

mod disk;
mod env;
mod ktests;
mod ktests_node;
mod node;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu::devs::Device;
use amdgpu::rdna4::messlive::{self, Live};
use amdgpu::rdna4::messung;
use console::text_ask::{self, Answer};
use core::sync::atomic::{AtomicBool, Ordering};

/// How long the start question waits for a key.
const ASK_NS: u64 = 120_000_000_000;

/// What the screen says before the question (plain German; nothing after the J waits for a key).
const INTRO: [&str; 9] = [
    "Messung der Grafikkarte RX 9070 XT",
    "Die Messung startet die Grafikkarte einmal ganz so, wie Linux sie startet: Schritt für Schritt bis zur Kopiereinheit.",
    "Jeder Schritt wird mit Linux verglichen und sofort in die Datei GPUMnnnn.TXT im Ordner BITSOS2\\REPORT des Startdatenträgers geschrieben.",
    "Weicht ein Schritt von Linux ab oder hält er an, endet die Messung dort.",
    "Sie dauert etwa eine Minute. Der Bildschirm kann dabei dunkel werden: das ist kein Fehler. Bitte keine Taste drücken.",
    "Am Ende schaltet sich der PC von selbst aus. Danach bitte CachyOS starten; dort liest Claude die Datei aus.",
    "Passiert zwei Minuten lang gar nichts, den Einschaltknopf gedrückt halten, bis der PC aus ist, und dann CachyOS starten.",
    "",
    "Taste J: Messung starten.   Jede andere Taste: abbrechen, es wird nichts getan.",
];

pub const BUSY: &str = "Die Messung läuft schon, oder ihre Frage steht schon auf dem Bildschirm: ein zweiter Start wird abgelehnt. Es wurde nichts getan.";
pub const DECLINED: &str = "Abgebrochen: es wurde nichts getan.";
pub const NO_KEY: &str = "Keine Taste in zwei Minuten: es wurde nichts getan.";
/// After a run whose power-off failed (the screen said the same before).
pub const NOT_OFF: &str = "Die Messung ist zu Ende, aber der PC hat sich nicht ausgeschaltet: bitte den Einschaltknopf gedrückt halten, bis er aus ist.";

/// One start at a time, from the question to the run's end. The card's own hold (`livestage::Claim`) begins only
/// after the J, and two questions at once would share the one answer of `text_ask`.
static STARTED: AtomicBool = AtomicBool::new(false);

/// The one start that stands; dropping it lets the next one in.
pub struct Hold(());

impl Hold {
    pub fn take() -> Option<Hold> {
        STARTED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok().map(|_| Hold(()))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        STARTED.store(false, Ordering::Release);
    }
}

/// What the run needs of the PC: the kernel's is the RX 9070 XT (`Rx9070`), the ktests give one that touches
/// nothing, so the question and its guard are tested on QEMU as they run on the Gaming-PC.
pub trait Machine: Send {
    /// The card is in this PC; Err: the one German line for a PC without it.
    fn here(&mut self) -> Result<(), &'static str>;
    /// After Georg's J. Err: refused before anything of the card was touched (German). Ok: the run went to its
    /// end and the PC did not switch off (when it does, this never returns).
    fn run(&mut self) -> Result<(), String>;
}

/// How a start ended, for whoever asked: whether the run went, and one plain German line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    pub ran: bool,
    pub line: String,
}

fn nothing(line: &str) -> End {
    End { ran: false, line: String::from(line) }
}

/// The measurement run, from either way of asking (`via`, for the log): refused while another start stands.
pub fn start(m: &mut dyn Machine, via: &str) -> End {
    match Hold::take() {
        Some(hold) => started(hold, m, via),
        None => {
            klog::event!(Info, "gx", "messung.busy", via = via; "a second start while one stands: refused");
            nothing(BUSY)
        }
    }
}

/// `start` once the hold is taken (the node takes it before its thread starts, so a second request is refused at
/// once).
pub fn started(hold: Hold, m: &mut dyn Machine, via: &str) -> End {
    let end = ask_and_run(m, via);
    drop(hold);
    klog::event!(Info, "gx", "messung.end", via = via, ran = end.ran, line = end.line.as_str());
    end
}

fn ask_and_run(m: &mut dyn Machine, via: &str) -> End {
    if let Err(line) = m.here() {
        return nothing(line);
    }
    let lines: Vec<String> = INTRO.iter().map(|l| String::from(*l)).collect();
    match text_ask::ask(&lines, ASK_NS) {
        Some(Answer::Ja) => {}
        Some(Answer::Nein) => return nothing(DECLINED),
        None => return nothing(NO_KEY),
    }
    klog::event!(Warn, "gx", "messung.confirmed", via = via; "J on a real keyboard: the measurement run starts");
    match m.run() {
        Ok(()) => End { ran: true, line: String::from(NOT_OFF) },
        Err(e) => nothing(&format!("{e} Es wurde nichts getan.")),
    }
}

/// The Gaming-PC's card.
#[derive(Default)]
pub struct Rx9070 {
    dev: Option<&'static Device>,
}

impl Machine for Rx9070 {
    fn here(&mut self) -> Result<(), &'static str> {
        self.dev = Some(messlive::find()?);
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        let dev = self.dev.ok_or(messlive::NONE_HERE)?;
        let mut card = Live::new(dev)?;
        klog::event!(Warn, "gx", "rdna4.messung.start", card = dev.index; "the measurement run starts: confirmed with J on a real keyboard");
        let out = messung::go(&mut card, &mut disk::FileDisk::default(), &mut env::KernelEnv::new());
        // Only when the power-off did not happen: the screen said in German to hold the power button.
        let why = out.off_failed.unwrap_or_default();
        klog::event!(Error, "gx", "rdna4.messung.not_off", why = why.as_str(), stopped = out.stopped.as_ref().map_or("", |s| s.1.as_str()); "the measurement run ended and the PC did not switch off: hold the power button");
        Ok(())
    }
}

/// The monitor's way in. A run that ends here did not switch the PC off: the screen already said so.
fn gpu_messung(c: &mut console::monitor::Ctx) -> Result<(), String> {
    let end = start(&mut Rx9070::default(), "monitor");
    if !end.ran {
        c.println(&end.line);
    }
    Ok(())
}
console::command!(name: "gpu-messung", args: "", help: "Messung der RX 9070 XT, danach schaltet sich der PC aus (keyboard only)", physical_only: true, run: gpu_messung);
