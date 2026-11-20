//! Register map of the RX 9070 XT's blocks (Navi 48: GC 12.0.x, MP0/MP1 14.0.x, NBIF 6.3.1, OSSSYS 7.0, MMHUB 4.1,
//! ATHUB 4.1, HDP 7.0, LSDMA 7.0, SMUIO 14.0.2, THM 14.0.2) as Linux 7.3.1's headers name it.
//!
//! Everything under `generated/` is produced by `host/gen/rdna4`; regenerate instead of editing. A register is a
//! dword offset below one of its IP's discovery bases (`base_idx`), so its place in BAR5 comes from the card's own
//! table (`Reg::dword`), never from a number in code. SDMA 7's registers are in `gc_12_0_0` (Linux has no SDMA
//! header for them). This crate only names addresses; it never touches hardware and no kernel crate uses it yet.
#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

#[rustfmt::skip]
#[allow(non_upper_case_globals)]
mod generated;
pub use generated::*;

/// BAR5 of the card: 512 KiB of registers; a dword past it needs the indirect pair.
pub const BAR5_DWORDS: u32 = (512 << 10) / 4;

/// A memory-mapped register: dword `offset` below discovery base `base_idx` of its IP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg {
    pub name: &'static str,
    pub base_idx: u8,
    pub offset: u32,
}

impl Reg {
    pub const fn new(name: &'static str, base_idx: u8, offset: u32) -> Reg {
        Reg { name, base_idx, offset }
    }

    /// The dword in the card's register space, from the IP's bases as its discovery table lists them.
    pub fn dword(&self, bases: &[u32]) -> Option<u32> {
        bases.get(usize::from(self.base_idx))?.checked_add(self.offset)
    }

    /// The byte in BAR5; None when the base is missing or the register lies past BAR5.
    pub fn bar5_byte(&self, bases: &[u32]) -> Option<u32> {
        self.dword(bases).filter(|&d| d < BAR5_DWORDS).map(|d| d * 4)
    }
}

/// An indirect register: the index written to its block's index register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ix {
    pub name: &'static str,
    pub index: u32,
}

impl Ix {
    pub const fn new(name: &'static str, index: u32) -> Ix {
        Ix { name, index }
    }
}

/// A register in PCI configuration space (byte offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cfg {
    pub name: &'static str,
    pub offset: u32,
}

impl Cfg {
    pub const fn new(name: &'static str, offset: u32) -> Cfg {
        Cfg { name, offset }
    }
}

/// A bit field of a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub shift: u8,
    pub mask: u32,
}

impl Field {
    pub const fn new(shift: u8, mask: u32) -> Field {
        Field { shift, mask }
    }
    pub const fn get(&self, reg: u32) -> u32 {
        (reg & self.mask) >> self.shift
    }
    /// `reg` with the field set to `value` (bits of `value` past the field are dropped).
    pub const fn put(&self, reg: u32, value: u32) -> u32 {
        (reg & !self.mask) | ((value << self.shift) & self.mask)
    }
}

/// A block's register by its Linux name (without `reg`).
pub fn by_name(block: &str, name: &str) -> Option<Reg> {
    BLOCKS.iter().find(|(b, _)| *b == block)?.1.iter().copied().find(|r| r.name == name)
}

#[cfg(test)]
mod tests;
