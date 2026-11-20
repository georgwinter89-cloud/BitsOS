//! The self-check of a bring-up as a list of stages, each ending in one plain line: for the log, and for the
//! Gaming-PC's report page, which shows `Report::lines` so that a round without network still says where it stopped.
//! The lines are German and for Georg: no register names.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ksync::SpinLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Stage {
    /// The card answers and is the chip this driver knows.
    Probe = 0,
    /// The firmware file was found and has the expected version.
    Firmware = 1,
    /// The built-in transceiver is the expected one and took its settings.
    Phy = 2,
    /// The cable has a link.
    Link = 3,
    /// The rings work: a frame came in.
    Rings = 4,
    /// The router gave an address.
    Lease = 5,
    /// The router answers a ping (which needs its ARP answer first).
    Gateway = 6,
    /// The card's interrupt arrives; until then it is polled.
    Irq = 7,
}

pub const STAGES: [Stage; 8] =
    [Stage::Probe, Stage::Firmware, Stage::Phy, Stage::Link, Stage::Rings, Stage::Lease, Stage::Gateway, Stage::Irq];

impl Stage {
    pub const fn name(self) -> &'static str {
        match self {
            Stage::Probe => "probe",
            Stage::Firmware => "firmware",
            Stage::Phy => "phy",
            Stage::Link => "link",
            Stage::Rings => "rings",
            Stage::Lease => "lease",
            Stage::Gateway => "gateway",
            Stage::Irq => "irq",
        }
    }

    pub const fn from_u8(v: u8) -> Option<Stage> {
        if (v as usize) < STAGES.len() { Some(STAGES[v as usize]) } else { None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// Not as expected, but the bring-up goes on.
    Note,
    Failed,
}

impl Verdict {
    pub const fn name(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Note => "note",
            Verdict::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub stage: Stage,
    pub verdict: Verdict,
    pub text: String,
}

/// One card's lines: the newest per stage, and where the card was stopped, if it was.
pub struct Report {
    lines: SpinLock<Vec<Line>>,
}

impl Default for Report {
    fn default() -> Self {
        // Room for every stage now, so that no line is ever added with the lock held and the heap asked.
        Report { lines: SpinLock::named("rtl8126.report", Vec::with_capacity(STAGES.len())) }
    }
}

impl Report {
    /// Replaces the stage's line. Thread context (allocates before, locks briefly).
    pub fn set(&self, line: Line) {
        let mut l = self.lines.lock();
        match l.iter_mut().find(|x| x.stage == line.stage) {
            Some(slot) => *slot = line,
            None => l.push(line),
        }
    }

    /// A new bring-up: the stages from `from` on are open again.
    pub fn reopen(&self, from: Stage) {
        self.lines.lock().retain(|x| x.stage < from);
    }

    pub fn get(&self, stage: Stage) -> Option<Line> {
        self.lines.lock().iter().find(|x| x.stage == stage).cloned()
    }

    /// The first stage that failed, if any.
    pub fn failed_at(&self) -> Option<Stage> {
        self.lines.lock().iter().filter(|x| x.verdict == Verdict::Failed).map(|x| x.stage).min()
    }

    /// The lines in stage order, each as "Netzwerk <n>/8: <text>".
    pub fn lines(&self) -> Vec<String> {
        let mut l = self.lines.lock().clone();
        l.sort_by_key(|x| x.stage);
        l.iter().map(|x| numbered(x.stage, &x.text)).collect()
    }
}

/// A stage's line as the report page shows it.
pub fn numbered(stage: Stage, text: &str) -> String {
    format!("Netzwerk {}/{}: {}", stage as u8 + 1, STAGES.len(), text)
}

/// Lines of cards that failed before a `Card` existed (no register window, no memory for the rings): one per PCI
/// location, the newest. Without them such a card would leave the report page as empty as a machine without one.
pub struct Early {
    /// (location, line). A fixed table: the heap is never asked with the lock held.
    lines: SpinLock<[Option<(String, String)>; EARLY_MAX]>,
}

/// More locations than this never hold the card.
const EARLY_MAX: usize = 8;

impl Early {
    pub const fn new() -> Early {
        Early { lines: SpinLock::named("rtl8126.early", [const { None }; EARLY_MAX]) }
    }

    /// Thread context (probe). With the table full of other locations the line is only in the log.
    pub fn set(&self, slot: &str, line: String) {
        let mut entry = Some((String::from(slot), line));
        {
            let mut l = self.lines.lock();
            let at = l.iter().position(|e| e.as_ref().is_some_and(|(s, _)| s == slot)).or_else(|| l.iter().position(Option::is_none));
            if let Some(at) = at {
                core::mem::swap(&mut l[at], &mut entry);
            }
        }
        drop(entry);
    }

    pub fn lines(&self) -> Vec<String> {
        let copy = self.lines.lock().clone();
        copy.into_iter().flatten().map(|(_, line)| line).collect()
    }
}

impl Default for Early {
    fn default() -> Self {
        Early::new()
    }
}

/// The German lines. Numbers and names in them are facts the desk asks Georg to read out.
pub mod text {
    use alloc::format;
    use alloc::string::String;

    pub fn probe_ok(chip: &str, xid: u16, slot: &str) -> String {
        format!("Karte erkannt: {chip}, Kennung {xid:03x}, Steckplatz {slot}")
    }
    pub fn probe_gone() -> String {
        "Die Karte antwortet nicht (alle Werte lesen sich als Einsen)".into()
    }
    pub fn probe_unknown(xid: u16) -> String {
        format!("Unbekannter Chip, Kennung {xid:03x}: nicht angefasst")
    }
    pub fn probe_other(what: &str) -> String {
        format!("Die Karte lässt sich nicht vorbereiten: {what}")
    }
    /// Before the card has a name of its own in the report: with its location.
    pub fn probe_early(what: &str, slot: &str) -> String {
        format!("Die Karte lässt sich nicht vorbereiten: {what} (Steckplatz {slot})")
    }
    pub const NO_RING_MEMORY: &str = "kein passender Speicher für die Datenpakete";
    pub fn firmware_ok(version: &str, expected: bool) -> String {
        if expected {
            format!("Firmware geladen: {version}")
        } else {
            format!("Firmware geladen: {version} (andere Fassung als unter Linux gemessen)")
        }
    }
    pub fn firmware_missing() -> String {
        "Firmware-Datei nicht gefunden: die Karte läuft ohne sie weiter".into()
    }
    pub fn firmware_bad() -> String {
        "Firmware-Datei beschädigt: die Karte läuft ohne sie weiter".into()
    }
    pub fn phy_ok(id: u32) -> String {
        format!("Leitungsteil erkannt, Kennung {id:08x}")
    }
    pub fn phy_wrong(id: u32) -> String {
        format!("Leitungsteil mit unbekannter Kennung {id:08x}: Karte angehalten")
    }
    pub fn phy_failed(what: &str) -> String {
        format!("Der Leitungsteil antwortet nicht richtig: {what}")
    }
    pub fn link_ok(mbps: u32) -> String {
        format!("Kabelverbindung steht: {mbps} Mbit/s")
    }
    pub fn link_none(secs: u64) -> String {
        format!("Keine Kabelverbindung nach {secs} Sekunden (Kabel prüfen); die Karte wartet weiter")
    }
    pub fn rings_ok() -> String {
        "Empfang läuft: das erste Datenpaket ist angekommen".into()
    }
    pub fn rings_retry(secs: u64) -> String {
        format!("Kein Datenpaket nach {secs} Sekunden: die Karte wird einmal neu gestartet")
    }
    pub fn rings_stopped(secs: u64, sent: u64, done: u64) -> String {
        format!("Kein Datenpaket nach {secs} Sekunden, auch nach dem Neustart (gesendet {sent}, davon fertig {done}): Karte angehalten")
    }
    pub fn start_failed(what: &str) -> String {
        format!("Die Karte lässt sich nicht starten: {what}")
    }
    pub fn lease_ok(a: [u8; 4]) -> String {
        format!("Adresse vom Router erhalten: {}.{}.{}.{}", a[0], a[1], a[2], a[3])
    }
    pub fn lease_none(secs: u64, received: u64, sent: u64, done: u64) -> String {
        format!("Noch keine Adresse vom Router nach {secs} Sekunden (empfangen {received}, gesendet {sent}, davon fertig {done})")
    }
    pub fn gateway_ok(us: u64) -> String {
        format!("Der Router antwortet ({us} Mikrosekunden)")
    }
    pub fn gateway_none(tries: u32) -> String {
        format!("Der Router antwortet nicht auf {tries} Anfragen")
    }
    pub fn gateway_unknown() -> String {
        "Der Router hat sich selbst nicht genannt: keine Probe".into()
    }
    pub fn irq_ok(kind: &str) -> String {
        format!("Die Karte meldet sich von selbst ({kind})")
    }
    pub fn irq_none() -> String {
        "Die Karte meldet sich nicht von selbst: sie wird weiter regelmäßig abgefragt".into()
    }
    pub fn irq_unproven(secs: u64) -> String {
        format!("Ob die Karte sich von selbst meldet, ließ sich nicht prüfen (kein Datenpaket in {secs} Sekunden): sie wird weiter regelmäßig abgefragt")
    }
    pub fn tx_stuck(times: u32) -> String {
        format!("Gesendete Pakete bleiben hängen, auch nach {times} Neustarts: Karte angehalten")
    }
}
