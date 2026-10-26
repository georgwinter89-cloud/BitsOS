//! The HPET table (IA-PC HPET specification 1.0a, table 3): where the timer block is and what it offers.

use crate::bytes::{u16_at, u32_at, u8_at};
use crate::sdt::{Gas, Space};
use crate::{Note, Notes};

const SIG: [u8; 4] = *b"HPET";
const LEN: usize = 56;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hpet {
    /// Event timer block ID: a copy of the block's capability register bits 31:0.
    pub block_id: u32,
    pub base: Gas,
    pub number: u8,
    pub min_tick: u16,
    pub page_protection: u8,
}

impl Hpet {
    pub fn vendor(&self) -> u16 {
        (self.block_id >> 16) as u16
    }
    /// Number of comparators (bits 12:8 hold the last index).
    pub fn comparators(&self) -> u8 {
        ((self.block_id >> 8) & 0x1F) as u8 + 1
    }
    pub fn counter_64bit(&self) -> bool {
        self.block_id & (1 << 13) != 0
    }
    pub fn legacy_capable(&self) -> bool {
        self.block_id & (1 << 15) != 0
    }
    /// The register block's physical address, if it is in memory space (the only valid one).
    pub fn mmio_base(&self) -> Option<u64> {
        (self.base.space == Space::Memory && self.base.address != 0).then_some(self.base.address)
    }
}

pub fn parse(t: &[u8], phys: u64, notes: &mut Notes) -> Option<Hpet> {
    if t.len() < LEN {
        notes.push(Note::table(SIG, phys, "too_short", t.len() as u64));
        return None;
    }
    let h = Hpet {
        block_id: u32_at(t, 36)?,
        base: Gas::parse(t, 40)?,
        number: u8_at(t, 52)?,
        min_tick: u16_at(t, 53)?,
        page_protection: u8_at(t, 55)?,
    };
    if h.mmio_base().is_none() {
        notes.push(Note::table(SIG, phys, "base_not_memory", h.base.address));
    }
    Some(h)
}
