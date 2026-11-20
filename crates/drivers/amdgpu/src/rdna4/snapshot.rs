//! What the remote desk reads of a card (MCP `gpu`), and the card's line on the Gaming-PC's report page. Pure text
//! from a `Card`; nothing here touches the hardware.

use super::attach::{BarFact, Link, PciFacts, Stop};
use super::fw::{self, StickFile};
use super::{Card, policy};
use crate::devs::Device;
use crate::ipmod::{self, Choice, State};
use crate::probe;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::attach::{MailboxRefusal, Source};
use rdna4_core::layout::{BlobRefusal, MemsizeError};

fn speed(code: u8) -> &'static str {
    match code {
        1 => "2.5 GT/s",
        2 => "5 GT/s",
        3 => "8 GT/s",
        4 => "16 GT/s",
        5 => "32 GT/s",
        6 => "64 GT/s",
        _ => "unknown speed",
    }
}

fn link(l: &Link) -> String {
    format!("{} x{} (can {} x{})", speed(l.speed), l.width, speed(l.max_speed), l.max_width)
}

fn bar(name: &str, b: Option<BarFact>, what: &str) -> String {
    match b {
        None => format!("{name}: none ({what})"),
        Some(b) => {
            let size = if b.bytes >= 1 << 20 { format!("{} MiB", b.bytes >> 20) } else { format!("{} KiB", b.bytes >> 10) };
            format!("{name}: {:#x}, {size}, {}-bit{} ({what})", b.phys, if b.is64 { 64 } else { 32 }, if b.prefetch { " prefetchable" } else { "" })
        }
    }
}

fn pci_lines(p: &PciFacts, out: &mut Vec<String>) {
    out.push(format!(
        "pci: command {:#06x} (memory decode {}, bus master {} as the firmware left it), power {}, MSI {}, MSI-X {}",
        p.command,
        if p.command & super::attach::CMD_MEMORY != 0 { "on" } else { "off" },
        if p.bus_master() { "on" } else { "off" },
        p.power_state.map_or(String::from("unknown"), |d| format!("D{d}")),
        if p.msi { "yes" } else { "no" },
        if p.msix { "yes" } else { "no" },
    ));
    out.push(bar("bar0", p.bar0, "graphics memory; only the table's pages are read, through a window of at most 68 KiB"));
    out.push(bar("bar2", p.bar2, "doorbells; not mapped"));
    out.push(bar("bar5", p.bar5, "registers; mapped uncached, at most five fixed dwords read"));
    match (&p.link, &p.upstream) {
        (Some(l), Some((at, up))) => out.push(format!("link: {}; narrowest above it {} at {at}", link(l), link(up))),
        (Some(l), None) => out.push(format!("link: {}", link(l))),
        (None, _) => out.push(String::from("link: no link registers")),
    }
}

/// The modules `plan` chose for the card's table, each with where its code stands.
pub fn module_lines(card: &Card) -> Vec<String> {
    let Ok(a) = &card.state else { return Vec::new() };
    ipmod::plan(&a.discovery)
        .choices
        .iter()
        .map(|c| match *c {
            Choice::Chosen { role, version, module } => match module.state {
                State::Planned(item) => format!("module {}: {} for {version}, not built yet ({item}; Linux {})", role.name(), module.name, module.linux),
                State::Present(at) => format!("module {}: {} for {version}, present ({at})", role.name(), module.name),
            },
            Choice::Missing { role, version, candidate } => format!("module {}: MISSING for {version}{}", role.name(), candidate.map_or(String::new(), |m| format!(" (Linux serves it with {}'s block)", m.name))),
            Choice::Harvested { role, version } => format!("module {}: {version} is fused off", role.name()),
            Choice::Absent { role } => format!("module {}: the card has no such block", role.name()),
        })
        .collect()
}

/// What Linux's start of the power controller depends on, known from reads alone (wave 43).
pub fn power_line(p: &PciFacts, a: &super::attach::Attached) -> String {
    let (generation, width) = rdna4_core::pcielink::caps(&p.path);
    format!(
        "power controller: scpm {:?} (the PSP's runtime database at memory size - 1 MiB); link path {:?} (the card first), Linux's limit generation code {generation}, width code {width}; host {:?} (Fixed: Linux forces every link level to the link as it runs)",
        a.scpm, p.path, p.link_host
    )
}

/// The whole answer for one card. Its first line says what the device is and that nothing was written.
pub fn text(dev: &Device, card: &Card, files: &[StickFile]) -> String {
    let p = &card.facts;
    let mut out: Vec<String> = Vec::new();
    out.push(format!(
        "gpu {} {}: Radeon RX 9070 XT (1002:{:04x} rev {:#04x}, subsystem {:04x}:{:04x}), {}, read-only attach (GP-GPU1)",
        dev.card_name(),
        dev.key,
        p.device,
        p.revision,
        p.subsystem.0,
        p.subsystem.1,
        if dev.display { "the display card (its BAR0 holds the firmware's framebuffer)" } else { "not the display card" },
    ));
    out.push(format!(
        "writes: {} register writes, {} refused, {} memory writes refused: {}",
        card.totals.writes,
        card.totals.refused,
        card.totals.mem_refused,
        if card.totals == Default::default() { "nothing was written, nothing was tried" } else { "A WRITE WAS TRIED (refused by the owner's gate): a bug, report it" },
    ));
    pci_lines(p, &mut out);
    // What the bring-up stages of this boot left standing (wave 42); never waits for a running stage.
    let stages = match dev.stages.0.try_lock() {
        None => String::from("; a stage is running"),
        Some(s) if !s.nb.nbif && !s.nb.remapped && s.nb.gart.is_none() && s.nb.ih.is_none() => String::from(" (recorded for later waves); no memory handed to the card, bus master not touched"),
        Some(s) => format!("; stages of this boot: {}", s.nb.describe()),
    };
    out.push(format!("dma: {} bits{stages}", policy::DMA_BITS));
    match &card.state {
        Err(stop) => {
            let (n, name) = stop.step();
            out.push(format!("attach: STOPPED at step {n} of 5 ({name}) after {} ms: {stop}", card.took_us / 1000));
        }
        Ok(a) => {
            out.push(format!("attach: PASS in {} ms (firmware ready after {} ms)", card.took_us / 1000, a.ready_after_ms));
            out.push(format!(
                "memsize: {} MiB (RCC_CONFIG_MEMSIZE), {} bytes of VRAM, {} through BAR0",
                a.memsize_mib,
                a.vram_bytes,
                if p.bar0.is_some_and(|b| b.bytes >= a.vram_bytes) { "all of it visible" } else { "not all visible" }
            ));
            out.push(format!(
                "discovery: at VRAM offset {:#x} (phys {:#x}), binary {}.{}, {} of {} bytes read are the table, {} entries, table version {}",
                a.discovery_offset,
                a.discovery_phys,
                a.discovery.binary_major,
                a.discovery.binary_minor,
                a.binary_size,
                a.read_bytes,
                a.discovery.ips.len(),
                a.discovery.table_version
            ));
            out.push(power_line(p, a));
            out.push(String::from(match a.source {
                Source::Default => "place: the default one (memory size - 64 KiB); the card's mailbox was not asked",
                Source::Mailbox => "place: the card's MAILBOX (DRIVER_SCRATCH_0..2); the default place (memory size - 64 KiB) held no table",
            }));
            if a.differs.is_empty() {
                out.push(String::from("measured: the table equals the one read under Linux on 5.11. (46 entries, versions and bases)"));
            } else {
                out.push(format!("measured: {} DIFFERENCES to the table read under Linux on 5.11.:", a.differs.len()));
                out.extend(a.differs.iter().map(|d| format!("  {d}")));
            }
            out.extend(a.discovery.ips.iter().map(|e| probe::ip_line(&a.discovery, e)));
            out.extend(module_lines(card));
        }
    }
    out.extend(fw::lines(files));
    // What the bring-up stages did since (the attach itself writes nothing); a stage that runs right now holds it.
    out.push(match dev.stages.0.try_lock() {
        Some(s) => format!("stages: {}", s.summary()),
        None => String::from("stages: one is running"),
    });
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Why the attach stopped, for the report page: plain German, one text per cause, so two causes at one step do not
/// look the same on the screen.
pub fn reason(stop: &Stop) -> &'static str {
    match stop {
        Stop::NotOurs => "keine Grafikkarte dieser Bauart",
        Stop::DecodeOff => "Karte ist vom Rechner abgeschaltet",
        Stop::Asleep(_) => "Karte schläft",
        Stop::Bar5(_) => "Fenster zur Steuerung der Karte fehlt oder passt nicht",
        Stop::Bar0(_) => "Fenster zum Speicher der Karte fehlt oder passt nicht",
        Stop::Unreadable(_) => "Karte antwortet beim Lesen nicht",
        Stop::NotReady => "Karte meldet sich nicht bereit",
        Stop::Memsize(MemsizeError::Gone) => "Speichergröße unlesbar, Karte antwortet nicht",
        Stop::Memsize(MemsizeError::Zero) => "Karte nennt keine Speichergröße",
        Stop::OutsideBar0 { .. } => "Speichergröße passt nicht in das Fenster zum Speicher",
        Stop::VramRead { .. } => "Speicher der Karte lässt sich nicht lesen",
        Stop::Mailbox(MailboxRefusal::Gone) => "Bauteilliste fehlt am üblichen Ort, Karte antwortet nicht mehr",
        Stop::Mailbox(MailboxRefusal::Size { .. }) => "Bauteilliste fehlt am üblichen Ort, der genannte Ersatzort ist zu klein",
        Stop::Mailbox(MailboxRefusal::Unaligned { .. }) => "Bauteilliste fehlt am üblichen Ort, der genannte Ersatzort liegt schief",
        Stop::Mailbox(MailboxRefusal::OutsideBar0 { .. }) => "Bauteilliste fehlt am üblichen Ort, der genannte Ersatzort liegt außerhalb der Karte",
        Stop::Blob(BlobRefusal::Signature { .. }) => "Bauteilliste: Kennung fehlt, kein Ersatzort genannt",
        Stop::Blob(BlobRefusal::Checksum { .. }) => "Bauteilliste: Prüfsumme falsch",
        Stop::Blob(BlobRefusal::Size { .. } | BlobRefusal::TooShort { .. }) => "Bauteilliste: Länge passt nicht",
        Stop::MailboxBlob { why: BlobRefusal::Signature { .. }, .. } => "Bauteilliste: Kennung fehlt auch am Ersatzort",
        Stop::MailboxBlob { why: BlobRefusal::Checksum { .. }, .. } => "Bauteilliste am Ersatzort: Prüfsumme falsch",
        Stop::MailboxBlob { .. } => "Bauteilliste am Ersatzort: Länge passt nicht",
        Stop::Parse(_) => "Bauteilliste: Inhalt nicht lesbar",
    }
}

/// The card's line on the report page (plain German, no code words; the desk's B10 prints it).
pub fn report_line(dev: &Device, card: &Card) -> String {
    let which = if dev.display { format!("Grafikkarte {} (zeigt das Bild)", dev.index + 1) } else { format!("Grafikkarte {}", dev.index + 1) };
    match &card.state {
        Ok(a) if card.totals == Default::default() => {
            let known = if a.differs.is_empty() { "wie unter Linux gemessen" } else { "anders als unter Linux gemessen" };
            let place = if a.source == Source::Mailbox { ", am Ersatzort gefunden" } else { "" };
            format!("{which}: erkannt, {} GB Speicher, Bauteilliste gelesen ({known}{place}). Nichts verändert.", (a.vram_bytes + (1 << 29)) >> 30)
        }
        Ok(_) => format!("{which}: erkannt, aber ein Schreibversuch wurde abgewiesen. Bitte dem Desk melden."),
        Err(stop) => {
            let (n, _) = stop.step();
            let step = ["Anschluss prüfen", "Karte bereit?", "Speichergröße lesen", "Bauteilliste lesen", "Bauteilliste prüfen"][usize::from(n) - 1];
            format!("{which}: Lesen angehalten bei Schritt {n} von 5 ({step}): {}. Nichts verändert.", reason(stop))
        }
    }
}
