//! The common table header (ACPI 6.5 §5.2.6) and the generic address structure (§5.2.3.2).

use crate::bytes::{slice, sum, u32_at, u64_at, u8_at};

pub const HEADER_LEN: usize = 36;

/// A table signature, e.g. `*b"FACP"`.
pub type Signature = [u8; 4];

pub const FADT: Signature = *b"FACP";
pub const MADT: Signature = *b"APIC";
pub const HPET: Signature = *b"HPET";
pub const MCFG: Signature = *b"MCFG";
pub const IVRS: Signature = *b"IVRS";
pub const SRAT: Signature = *b"SRAT";
pub const DSDT: Signature = *b"DSDT";
pub const SSDT: Signature = *b"SSDT";
pub const XSDT: Signature = *b"XSDT";
pub const RSDT: Signature = *b"RSDT";

/// The header fields of one table, as read from memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub signature: Signature,
    pub length: u32,
    pub revision: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
}

impl Header {
    /// Parses the first 36 bytes; `None` if fewer are given.
    pub fn parse(b: &[u8]) -> Option<Header> {
        Some(Header {
            signature: slice(b, 0, 4)?.try_into().ok()?,
            length: u32_at(b, 4)?,
            revision: u8_at(b, 8)?,
            oem_id: slice(b, 10, 6)?.try_into().ok()?,
            oem_table_id: slice(b, 16, 8)?.try_into().ok()?,
            oem_revision: u32_at(b, 24)?,
        })
    }
}

/// True when the bytes of a whole table sum to 0.
pub fn checksum_ok(table: &[u8]) -> bool {
    sum(table) == 0
}

/// Address space of a generic address structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    Memory,
    Io,
    PciConfig,
    /// Embedded controller, SMBus, FFixedHW, ...: never written by BitsOS.
    Other(u8),
}

impl Space {
    pub fn from_id(id: u8) -> Space {
        match id {
            0 => Space::Memory,
            1 => Space::Io,
            2 => Space::PciConfig,
            o => Space::Other(o),
        }
    }
    pub fn id(self) -> u8 {
        match self {
            Space::Memory => 0,
            Space::Io => 1,
            Space::PciConfig => 2,
            Space::Other(o) => o,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Space::Memory => "mem",
            Space::Io => "io",
            Space::PciConfig => "pci",
            Space::Other(_) => "other",
        }
    }
}

/// Generic address structure: where a register lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gas {
    pub space: Space,
    pub bit_width: u8,
    pub bit_offset: u8,
    /// 0 undefined (legacy), 1 byte, 2 word, 3 dword, 4 qword.
    pub access_size: u8,
    pub address: u64,
}

impl Gas {
    pub const LEN: usize = 12;

    pub fn parse(b: &[u8], o: usize) -> Option<Gas> {
        Some(Gas {
            space: Space::from_id(u8_at(b, o)?),
            bit_width: u8_at(b, o + 1)?,
            bit_offset: u8_at(b, o + 2)?,
            access_size: u8_at(b, o + 3)?,
            address: u64_at(b, o + 4)?,
        })
    }

    /// A system I/O register of `bytes` bytes (a legacy FADT block).
    pub fn io(port: u32, bytes: u8) -> Gas {
        Gas { space: Space::Io, bit_width: bytes.saturating_mul(8), bit_offset: 0, access_size: 0, address: port as u64 }
    }

    /// Bytes one access takes: the access size if given, else the register width, at least 1, at most 8.
    pub fn access_bytes(&self) -> u32 {
        let by_size = match self.access_size {
            1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => 0,
        };
        let n = if by_size != 0 { by_size } else { (self.bit_width as u32).div_ceil(8) };
        n.clamp(1, 8)
    }
}
