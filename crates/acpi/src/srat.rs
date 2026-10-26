//! The SRAT (ACPI 6.5 §5.2.16), when present: which CPUs and memory ranges belong to which proximity
//! domain. Neither machine has more than one domain today; SC and MM read it when it matters.

use crate::bytes::{u32_at, u8_at};
use crate::{Note, Notes};
use alloc::vec::Vec;

const SIG: [u8; 4] = *b"SRAT";
const ENTRIES: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuAffinity {
    pub apic_id: u32,
    pub domain: u32,
    pub enabled: bool,
    pub x2apic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemAffinity {
    pub base: u64,
    pub length: u64,
    pub domain: u32,
    pub enabled: bool,
    pub hot_pluggable: bool,
    pub non_volatile: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Srat {
    pub cpus: Vec<CpuAffinity>,
    pub memory: Vec<MemAffinity>,
    pub other_entries: u32,
}

impl Srat {
    /// Distinct proximity domains named by enabled entries.
    pub fn domains(&self) -> usize {
        let mut d: Vec<u32> = self.cpus.iter().filter(|c| c.enabled).map(|c| c.domain).collect();
        d.extend(self.memory.iter().filter(|m| m.enabled).map(|m| m.domain));
        d.sort_unstable();
        d.dedup();
        d.len()
    }
}

pub fn parse(t: &[u8], phys: u64, notes: &mut Notes) -> Option<Srat> {
    if t.len() < ENTRIES {
        notes.push(Note::table(SIG, phys, "too_short", t.len() as u64));
        return None;
    }
    let mut s = Srat::default();
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
        if entry(&mut s, ty, &t[o..o + len]).is_none() {
            notes.push(Note::table(SIG, phys, "entry_short", ((ty as u64) << 32) | o as u64));
        }
        o += len;
    }
    Some(s)
}

fn entry(s: &mut Srat, ty: u8, e: &[u8]) -> Option<()> {
    match ty {
        0 => {
            if e.len() < 16 {
                return None;
            }
            let hi = u32_at(e, 8)? >> 8; // bytes 9..11 are bits 31:8 of the domain
            s.cpus.push(CpuAffinity { domain: u8_at(e, 2)? as u32 | hi << 8, apic_id: u8_at(e, 3)? as u32, enabled: u32_at(e, 4)? & 1 != 0, x2apic: false });
        }
        1 => {
            if e.len() < 40 {
                return None;
            }
            let base = u32_at(e, 8)? as u64 | (u32_at(e, 12)? as u64) << 32;
            let length = u32_at(e, 16)? as u64 | (u32_at(e, 20)? as u64) << 32;
            let f = u32_at(e, 28)?;
            s.memory.push(MemAffinity { base, length, domain: u32_at(e, 2)?, enabled: f & 1 != 0, hot_pluggable: f & 2 != 0, non_volatile: f & 4 != 0 });
        }
        2 => {
            if e.len() < 24 {
                return None;
            }
            s.cpus.push(CpuAffinity { domain: u32_at(e, 4)?, apic_id: u32_at(e, 8)?, enabled: u32_at(e, 12)? & 1 != 0, x2apic: true });
        }
        _ => s.other_entries += 1,
    }
    Some(())
}
