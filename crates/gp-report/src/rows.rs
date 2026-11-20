//! The page's own rows in plain German, from plain facts the kernel reads out of the drivers' getters. The
//! wording follows `plan/spec/gaming-pc.md` section 8; a verdict word Georg can photograph comes last.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// `3878400000` -> `3 878,4 MHz` (German digits: a space between thousands, a comma before the tenth).
pub fn mhz(hz: u64) -> String {
    let tenths = (hz + 50_000) / 100_000;
    let (whole, tenth) = (tenths / 10, tenths % 10);
    format!("{},{tenth} MHz", thousands(whole))
}

/// `1234567` -> `1 234 567`.
pub fn thousands(n: u64) -> String {
    let s = format!("{n}");
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// The CPUs as the bring-up left them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cpus {
    /// CPUs running (the BSP included).
    pub up: usize,
    /// CPUs the MADT names as usable (the BSP included).
    pub planned: usize,
    pub p_cores: usize,
    pub e_cores: usize,
    /// CPUs with HWP on.
    pub hwp_on: usize,
}

pub fn cpus(c: Cpus) -> Vec<String> {
    let kinds = if c.p_cores + c.e_cores > 0 { format!(" ({} schnelle, {} sparsame)", c.p_cores, c.e_cores) } else { String::new() };
    let mut v = alloc::vec![if c.up >= c.planned && c.planned > 0 {
        format!("Prozessor: {} Kerne laufen{kinds}", c.up)
    } else {
        format!("Prozessor: Nur {} von {} Kernen laufen{kinds}", c.up, c.planned.max(c.up))
    }];
    v.push(if c.hwp_on >= c.up && c.up > 0 {
        format!("Hochtakten: eingeschaltet auf {} Kernen", c.hwp_on)
    } else {
        format!("Hochtakten: auf {} Kernen nicht eingeschaltet (BitsOS läuft trotzdem, nur langsamer)", c.up.saturating_sub(c.hwp_on))
    });
    v
}

/// The TSC rate and where it came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub hz: u64,
    /// CPUID 0x15's rate, 0 when the CPU names none.
    pub cpuid15_hz: u64,
    /// Measured against CPUID 0x15 (parts per million), when both exist.
    pub ppm: Option<i64>,
    /// The rate taken is CPUID 0x15's, checked by a measurement.
    pub cpuid15_checked: bool,
    /// The HPET was left out on purpose (it can stop in the deepest package sleep state).
    pub hpet_left_out: bool,
}

pub fn clock(c: Clock) -> Vec<String> {
    let mut v = Vec::new();
    v.push(match (c.cpuid15_checked, c.cpuid15_hz) {
        (true, _) => format!("Uhr: {}, vom Prozessor genannt und nachgemessen (Abweichung {} Millionstel)", mhz(c.hz), c.ppm.unwrap_or(0).unsigned_abs()),
        (false, 0) => format!("Uhr: {}, gemessen", mhz(c.hz)),
        (false, n) => format!("Uhr: gemessen {}, der Prozessor nennt {}: die Messung gilt", mhz(c.hz), mhz(n)),
    });
    if c.hpet_left_out {
        v.push(String::from("Zweite Uhr im Chipsatz: wird auf diesem PC nicht benutzt (sie kann im Tiefschlaf stehen bleiben)"));
    }
    v
}

/// The MADT's processor list.
pub fn madt(cpus: usize, empty_slots: u32, problems: usize) -> String {
    if problems == 0 {
        format!("Prozessorliste: {cpus} Prozessoren, {empty_slots} leere Plätze, keine Fehler")
    } else {
        format!("Prozessorliste: {cpus} Prozessoren, {empty_slots} leere Plätze, {problems} Fehler in der Liste")
    }
}

/// `count` fault-safe register reads the CPU refused, `regs` their numbers (`0x…`).
pub fn msr_faults(count: u64, regs: &[u32]) -> String {
    if count == 0 {
        return String::from("Abgelehnte Prozessor-Abfragen: keine");
    }
    let list: Vec<String> = regs.iter().map(|r| format!("{r:#x}")).collect();
    format!("Abgelehnte Prozessor-Abfragen: {count} hat der Prozessor abgelehnt (Nummern: {}). BitsOS läuft weiter; bitte fotografieren.", list.join(", "))
}

/// Speculation control as the kernel set it (`IA32_SPEC_CTRL`; None: the CPU has none or refused it).
pub fn spec_ctrl(value: Option<u64>) -> String {
    match value {
        Some(v) => format!("Schutz gegen Seitenkanäle im Prozessor: eingeschaltet (Wert {v:#x})"),
        None => String::from("Schutz gegen Seitenkanäle im Prozessor: nicht gesetzt"),
    }
}

/// Power off and restart as ACPI found them.
pub fn power(s5_type: Option<u8>, reset_found: bool) -> String {
    match (s5_type, reset_found) {
        (Some(t), true) => format!("Ausschalten und Neustart: Wege gefunden (Ausschalten: Art {t}, Neustart: über den Chipsatz)"),
        (Some(t), false) => format!("Ausschalten und Neustart: Ausschalten gefunden (Art {t}), Neustart nur über den Tastatur-Baustein"),
        (None, _) => String::from("Ausschalten und Neustart: Kein Weg zum Ausschalten gefunden: bitte den Einschaltknopf lange drücken"),
    }
}

/// The DMA verdict (`acpi::dmar::verdict`), by its name.
pub fn dma(verdict: &str) -> String {
    match verdict {
        "free" | "no_table" => String::from("Speicherschutz für Geräte: ausgeschaltet: Geräte dürfen arbeiten"),
        "translation_on" | "protected_memory_on" => String::from(
            "Speicherschutz für Geräte: EINGESCHALTET: Tastatur, Stick, Netzwerk und Festplatten bleiben aus. Bitte fotografieren und ausschalten.",
        ),
        _ => String::from("Speicherschutz für Geräte: unbekannt: konnte nicht gelesen werden"),
    }
}

/// Drivers the DMA gate stopped (`pci enable.refused ... dma=true`), as "<function> (<driver>)".
pub fn dma_stopped(list: &[String]) -> Option<String> {
    (!list.is_empty()).then(|| format!("Vom Speicherschutz angehalten: {} (bitte fotografieren)", list.join(", ")))
}

/// The chipset's watchdog (`acpi::tco`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watchdog {
    Absent,
    Halted,
    Running { timeout_s: u32 },
    Unreadable,
}

pub fn watchdog(w: Watchdog) -> String {
    match w {
        Watchdog::Absent => String::from("Wächter im Chipsatz: nicht gefunden"),
        Watchdog::Halted => String::from("Wächter im Chipsatz: vorhanden, angehalten (BitsOS benutzt ihn noch nicht)"),
        Watchdog::Running { timeout_s } => format!("Wächter im Chipsatz: vorhanden, läuft ({timeout_s} Sekunden)"),
        Watchdog::Unreadable => String::from("Wächter im Chipsatz: vorhanden, Zustand nicht lesbar"),
    }
}

/// The loader skipped a generation older than the disk guard (`bitsboot generation.unguarded number=<n>`).
pub fn unguarded(n: u32) -> String {
    format!("Version {n} auf dem Stick ist zu alt für diesen PC und wurde übersprungen.")
}

/// GP-R3 (UPDATE's change request): the build's own number (crates/bitsver), which names the same code on every
/// machine; "Laufende Version" above it is the slot on this boot medium. `date` is YYYY-MM-DD or empty.
pub fn version_number(number: u32, date: &str) -> String {
    if number == 0 {
        return String::from("Versionsnummer: keine (ein Entwicklungsstand)");
    }
    let p: Vec<&str> = date.split('-').collect();
    match p.as_slice() {
        [y, m, d] => format!("Versionsnummer: {number} vom {d}.{m}.{y}"),
        _ => format!("Versionsnummer: {number}"),
    }
}

/// The picture's mapping: write-combining (fast) or not (gpx note U11).
pub fn picture_speed(wc: Option<bool>) -> String {
    match wc {
        Some(true) => String::from("Bild: schneller Schreibweg (zusammengefasst) eingeschaltet"),
        Some(false) => String::from("Bild: langsamer Schreibweg (das Bild baut sich langsam auf, BitsOS läuft trotzdem)"),
        None => String::from("Bild: Schreibweg unbekannt (kein Bild übernommen)"),
    }
}

/// How the desk reaches the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remote {
    /// MCP answers at this address and port.
    Reachable { addr: String, port: u16 },
    /// MCP runs but no card has an address yet.
    NoAddress,
    /// The profile keeps the network off (no `gp_net=on`).
    NetOff,
    /// MCP did not start.
    NotStarted(McpDown),
}

/// Why MCP did not start (the kernel maps `mcp::StartError` onto these; the page never shows a code name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpDown {
    /// No `token=` line on the stick.
    NoToken,
    /// The token is too short, too long or holds a space.
    BadToken,
    BadPort,
    /// The kernel's network stack could not take the connection (no stack, port taken, no thread).
    NetworkNotReady,
    /// The kernel never said (cannot happen: `main` notes MCP's start before the report starts).
    Unknown,
}

pub fn mcp_down(d: McpDown) -> &'static str {
    match d {
        McpDown::NoToken => "auf dem Stick steht kein Zugangsschlüssel (die Zeile token fehlt in CONFIG.TXT)",
        McpDown::BadToken => "der Zugangsschlüssel auf dem Stick ist ungültig (8 bis 127 Zeichen, ohne Leerzeichen)",
        McpDown::BadPort => "die Anschlussnummer für MCP auf dem Stick ist ungültig",
        McpDown::NetworkNotReady => "das Netzwerk im System ist nicht bereit",
        McpDown::Unknown => "der Grund ist unbekannt",
    }
}

/// Marks the rows that mean the remote half of the round cannot happen (red, never cut: `page::KEEP_MARKS`).
pub const UNREACHABLE: &str = "Claude kann diesen PC so nicht erreichen";

pub fn remote(r: &Remote) -> String {
    match r {
        Remote::Reachable { addr, port } => format!("MCP erreichbar unter {addr}:{port}"),
        Remote::NoAddress => {
            format!("MCP läuft, aber die Karte hat noch keine Adresse: {UNREACHABLE}. Kommt die Adresse noch, wird diese Seite einmal erneuert.")
        }
        Remote::NetOff => String::from("MCP nicht erreichbar: das Netzwerk ist aus (auf dem Stick steht kein gp_net=on)"),
        Remote::NotStarted(why) => format!("MCP nicht gestartet: {}. {UNREACHABLE}. Bitte fotografieren.", mcp_down(*why)),
    }
}

/// The network card's rows when its driver said nothing (`rtl8126::report_lines` empty).
pub fn net_silent(card_present: bool, net_off: bool) -> String {
    match (card_present, net_off) {
        (_, true) => String::from("Netzwerkkarten: vom sicheren Modus abgeschaltet (auf dem Stick steht kein gp_net=on)"),
        (true, false) => String::from("Netzwerkkarte von Realtek gefunden, aber ihr Treiber hat sich nicht gemeldet"),
        (false, false) => String::from("Keine Netzwerkkarte von Realtek gefunden"),
    }
}

/// Where sound plays by default: `outputs` are the playable outputs in the mixer's order (the first one plays).
pub fn sound(outputs: &[String]) -> String {
    match outputs.first() {
        Some(o) => format!("Ton: spielt auf {o} (Kopfhörer am USB zuerst; kein Ton über HDMI auf diesem PC)"),
        None => String::from("Ton: kein Ausgang gefunden (Kopfhörer am USB einstecken; die Buchsen am Mainboard gehen noch nicht)"),
    }
}

/// How the file's row begins (`Report::set_file` finds it by this).
pub const FILE_ROW: &str = "Bericht auf dem Stick:";

/// The report file on the stick: written (its name), not asked for, or why not.
pub fn file(state: &FileState) -> String {
    match state {
        FileState::Written(name) => format!("{FILE_ROW} \\BITSOS2\\REPORT\\{name}"),
        FileState::Off => format!("{FILE_ROW} nicht geschrieben (der Stick wird nur gelesen, kein gp_stick=on)"),
        FileState::Failed(why) => format!("{FILE_ROW} nicht geschrieben ({why})"),
        FileState::Waiting => format!("{FILE_ROW} wird geschrieben, sobald der Stick bereit ist"),
    }
}

/// The file's state before anything waits for the stick: off without `gp_stick=on`; failed at once when the
/// device protection keeps USB off or no USB controller runs (the stick can never come, and its wait would only
/// cost time); else waiting for the file thread.
pub fn file_first(stick_on: bool, dma_blocked: bool, usb_running: bool) -> FileState {
    match (stick_on, dma_blocked, usb_running) {
        (false, _, _) => FileState::Off,
        (true, true, _) => FileState::Failed(String::from("der Speicherschutz für Geräte hält USB an")),
        (true, false, false) => FileState::Failed(String::from("kein USB-Baustein läuft")),
        (true, false, true) => FileState::Waiting,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileState {
    Written(String),
    Off,
    Failed(String),
    Waiting,
}
