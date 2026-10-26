//! The MADT ("APIC", ACPI 6.5 §5.2.12): local APICs (xAPIC and x2APIC entries), I/O APICs, interrupt source
//! overrides, NMI sources and local APIC NMI pins. Entries with a bad length end the walk (a length of 0
//! would loop forever), entries shorter than their type needs are skipped; both are noted.

use crate::bytes::{u16_at, u32_at, u64_at, u8_at};
use crate::{Note, Notes};
use alloc::vec::Vec;

const SIG: [u8; 4] = *b"APIC";
const ENTRIES: usize = 44;

/// MADT flags bit 0: the machine also has dual 8259 PICs (they must be masked).
pub const PCAT_COMPAT: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub apic_id: u32,
    /// ACPI processor UID (8 bits for xAPIC entries).
    pub uid: u32,
    pub enabled: bool,
    /// Off now but may be brought online (ACPI 6.3); Linux counts such CPUs as possible.
    pub online_capable: bool,
    /// From an x2APIC entry (type 9).
    pub x2apic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApic {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

/// Polarity and trigger of an interrupt (MPS INTI flags).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntiFlags(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    BusDefault,
    High,
    Low,
    Reserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    BusDefault,
    Edge,
    Level,
    Reserved,
}

impl IntiFlags {
    pub fn polarity(self) -> Polarity {
        match self.0 & 3 {
            0 => Polarity::BusDefault,
            1 => Polarity::High,
            3 => Polarity::Low,
            _ => Polarity::Reserved,
        }
    }
    pub fn trigger(self) -> Trigger {
        match (self.0 >> 2) & 3 {
            0 => Trigger::BusDefault,
            1 => Trigger::Edge,
            3 => Trigger::Level,
            _ => Trigger::Reserved,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Override {
    pub bus: u8,
    /// ISA IRQ.
    pub source: u8,
    pub gsi: u32,
    pub flags: IntiFlags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NmiSource {
    pub gsi: u32,
    pub flags: IntiFlags,
}

/// A local APIC's LINT pin wired to NMI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LapicNmi {
    /// Processor UID, or `None` for "all processors" (0xFF in type 4, 0xFFFF_FFFF in type 0xA).
    pub uid: Option<u32>,
    pub flags: IntiFlags,
    pub lint: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Madt {
    /// Local APIC base (the 64-bit override when present).
    pub lapic_address: u64,
    pub flags: u32,
    pub cpus: Vec<Cpu>,
    pub ioapics: Vec<IoApic>,
    pub overrides: Vec<Override>,
    pub nmi_sources: Vec<NmiSource>,
    pub lapic_nmis: Vec<LapicNmi>,
    /// Entries of types this parser does not read (GIC, SAPIC, wakeup, ...).
    pub other_entries: u32,
}

impl Madt {
    pub fn has_8259(&self) -> bool {
        self.flags & PCAT_COMPAT != 0
    }
    pub fn enabled_cpus(&self) -> usize {
        self.cpus.iter().filter(|c| c.enabled).count()
    }
    /// The override for an ISA IRQ, if the firmware gave one.
    pub fn isa_override(&self, irq: u8) -> Option<&Override> {
        self.overrides.iter().find(|o| o.bus == 0 && o.source == irq)
    }
    /// The I/O APIC serving `gsi`: the one with the highest base not above it.
    pub fn ioapic_for(&self, gsi: u32) -> Option<&IoApic> {
        self.ioapics.iter().filter(|a| a.gsi_base <= gsi).max_by_key(|a| a.gsi_base)
    }
}

pub fn parse(t: &[u8], phys: u64, notes: &mut Notes) -> Option<Madt> {
    if t.len() < ENTRIES {
        notes.push(Note::table(SIG, phys, "too_short", t.len() as u64));
        return None;
    }
    let mut m = Madt { lapic_address: u32_at(t, 36)? as u64, flags: u32_at(t, 40)?, ..Madt::default() };
    let mut o = ENTRIES;
    while o < t.len() {
        let (Some(ty), Some(len)) = (u8_at(t, o), u8_at(t, o + 1)) else {
            notes.push(Note::table(SIG, phys, "entry_cut", o as u64));
            break;
        };
        let len = len as usize;
        if len < 2 || o + len > t.len() {
            notes.push(Note::table(SIG, phys, "entry_length", o as u64));
            break;
        }
        let e = &t[o..o + len];
        match entry(&mut m, ty, e) {
            None => notes.push(Note::table(SIG, phys, "entry_short", ((ty as u64) << 32) | o as u64)),
            Some(Kept::Broadcast(id)) => notes.push(Note::table(SIG, phys, "broadcast_apic_id", id as u64)),
            Some(Kept::Yes) => {}
        }
        o += len;
    }
    check_duplicates(&m, phys, notes);
    Some(m)
}

/// APIC IDs that address every CPU, never one (Linux `acpi_parse_lapic`/`acpi_parse_x2apic` skip them).
const XAPIC_BROADCAST: u32 = 0xFF;
const X2APIC_BROADCAST: u32 = u32::MAX;

enum Kept {
    Yes,
    /// A CPU entry with the broadcast ID: not a CPU, skipped.
    Broadcast(u32),
}

/// One entry; `None` when it is shorter than its type needs.
fn entry(m: &mut Madt, ty: u8, e: &[u8]) -> Option<Kept> {
    match ty {
        0 => {
            let f = u32_at(e, 4)?;
            let (uid, apic_id) = (u8_at(e, 2)? as u32, u8_at(e, 3)? as u32);
            if apic_id == XAPIC_BROADCAST {
                return Some(Kept::Broadcast(apic_id));
            }
            m.cpus.push(Cpu { uid, apic_id, enabled: f & 1 != 0, online_capable: f & 2 != 0, x2apic: false });
        }
        1 => m.ioapics.push(IoApic { id: u8_at(e, 2)?, address: u32_at(e, 4)?, gsi_base: u32_at(e, 8)? }),
        2 => m.overrides.push(Override { bus: u8_at(e, 2)?, source: u8_at(e, 3)?, gsi: u32_at(e, 4)?, flags: IntiFlags(u16_at(e, 8)?) }),
        3 => m.nmi_sources.push(NmiSource { flags: IntiFlags(u16_at(e, 2)?), gsi: u32_at(e, 4)? }),
        4 => {
            let uid = u8_at(e, 2)?;
            m.lapic_nmis.push(LapicNmi { uid: (uid != 0xFF).then_some(uid as u32), flags: IntiFlags(u16_at(e, 3)?), lint: u8_at(e, 5)? });
        }
        5 => m.lapic_address = u64_at(e, 4)?,
        9 => {
            let f = u32_at(e, 8)?;
            let (apic_id, uid) = (u32_at(e, 4)?, u32_at(e, 12)?);
            if apic_id == X2APIC_BROADCAST {
                return Some(Kept::Broadcast(apic_id));
            }
            m.cpus.push(Cpu { apic_id, uid, enabled: f & 1 != 0, online_capable: f & 2 != 0, x2apic: true });
        }
        0xA => {
            let uid = u32_at(e, 4)?;
            m.lapic_nmis.push(LapicNmi { flags: IntiFlags(u16_at(e, 2)?), uid: (uid != u32::MAX).then_some(uid), lint: u8_at(e, 8)? });
        }
        _ => m.other_entries += 1,
    }
    Some(Kept::Yes)
}

/// Two enabled CPUs with one APIC ID, or two I/O APICs with one GSI base, are firmware bugs worth a line
/// (Linux ignores the second CPU; CP-4 decides).
fn check_duplicates(m: &Madt, phys: u64, notes: &mut Notes) {
    let mut ids: Vec<u32> = m.cpus.iter().filter(|c| c.enabled).map(|c| c.apic_id).collect();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        notes.push(Note::table(SIG, phys, "duplicate_apic_id", w[0] as u64));
    }
    let mut bases: Vec<u32> = m.ioapics.iter().map(|a| a.gsi_base).collect();
    bases.sort_unstable();
    if let Some(w) = bases.windows(2).find(|w| w[0] == w[1]) {
        notes.push(Note::table(SIG, phys, "duplicate_gsi_base", w[0] as u64));
    }
}
