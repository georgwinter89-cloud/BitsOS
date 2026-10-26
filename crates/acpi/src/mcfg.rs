//! The MCFG (PCI Firmware Specification 3.2 §4.1.2): ECAM windows per segment and bus range.

use crate::bytes::{u16_at, u64_at, u8_at};
use crate::{Note, Notes};
use alloc::vec::Vec;

const SIG: [u8; 4] = *b"MCFG";
const ENTRIES: usize = 44;
const ENTRY_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ecam {
    /// Address of bus 0 of the segment (Linux adds `bus << 20` to it).
    pub base: u64,
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
}

impl Ecam {
    /// The configuration window this entry decodes: `(start, end)` exclusive.
    pub fn window(&self) -> (u64, u64) {
        (self.base + ((self.bus_start as u64) << 20), self.base + ((self.bus_end as u64 + 1) << 20))
    }
    pub fn contains(&self, start: u64, end: u64) -> bool {
        let (s, e) = self.window();
        start < e && s < end
    }
}

/// Entries that make sense; the rest are noted and dropped (bus range backwards, address 0, overflow).
pub fn parse(t: &[u8], phys: u64, notes: &mut Notes) -> Vec<Ecam> {
    let mut out = Vec::new();
    if t.len() < ENTRIES {
        notes.push(Note::table(SIG, phys, "too_short", t.len() as u64));
        return out;
    }
    if !(t.len() - ENTRIES).is_multiple_of(ENTRY_LEN) {
        notes.push(Note::table(SIG, phys, "odd_length", t.len() as u64));
    }
    let mut o = ENTRIES;
    while o + ENTRY_LEN <= t.len() {
        let e = (|| Some(Ecam { base: u64_at(t, o)?, segment: u16_at(t, o + 8)?, bus_start: u8_at(t, o + 10)?, bus_end: u8_at(t, o + 11)? }))();
        match e {
            Some(e) if e.base != 0 && e.bus_end >= e.bus_start && e.base.checked_add(256 << 20).is_some() => out.push(e),
            Some(e) => notes.push(Note::table(SIG, phys, "bad_entry", e.base)),
            None => break,
        }
        o += ENTRY_LEN;
    }
    out
}
