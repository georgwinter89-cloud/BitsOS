//! ACPI tables (CP-2) and the interrupt controllers (CP-3: `irq`, `ioapic`, `irq_plan`, ktests in `irq_ktest`).
//! Owner: CP. CP's CPU set-up and SMP bring-up live in `crates/platform` (CP-4), on top of this crate.
//!
//! Tables are read without an AML interpreter (spec/core.md §4): RSDP from BootInfo, XSDT (RSDT as
//! fallback), FADT, MADT, HPET, MCFG, IVRS, SRAT, the DSDT for `\_S5`. Firmware data is hostile input:
//! every read is bounded by the table's own length, checksums are checked, nothing panics.
//! - pure parsing: `parse::parse` over a `mem::PhysMem` (host-tested with table builders),
//! - the kernel side: `live::init(&BootInfo)` reads the tables once (`kmem` reads physical memory), keeps
//!   them for `tables()`, logs them, and serves the register guard (`live::check_gas` and friends) that
//!   every lane calls before writing to an address a table names.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod bytes;
pub mod clock;
pub mod cpu_keys;
pub mod fadt;
pub mod guard;
pub mod hpet;
pub mod ioapic;
pub mod irq;
pub mod irq_plan;
pub mod ivrs;
pub mod kmem;
pub mod kpci;
pub mod live;
pub mod madt;
pub mod mcfg;
pub mod mem;
pub mod memmap;
pub mod parse;
pub mod power;
pub mod ranges;
pub mod report;
pub mod rsdp;
pub mod s5;
pub mod sdt;
pub mod srat;
#[doc(hidden)]
pub mod testkit;

mod irq_ktest;
mod ktests;

#[cfg(test)]
mod tests;

pub use cpu_keys::cpu_keys;
pub use live::{check_gas, check_io, check_mem, init, set_pci_view, tables, trampoline};

use alloc::vec::Vec;
use sdt::{Header, Signature};

/// Which root table the tables came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Xsdt,
    Rsdt,
}

impl RootKind {
    pub fn signature(self) -> Signature {
        match self {
            RootKind::Xsdt => sdt::XSDT,
            RootKind::Rsdt => sdt::RSDT,
        }
    }
    pub fn entry_size(self) -> usize {
        match self {
            RootKind::Xsdt => 8,
            RootKind::Rsdt => 4,
        }
    }
}

/// One table the root lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub phys: u64,
    pub header: Header,
    /// `None`: only the header was read (a table BitsOS does not use).
    pub checksum_ok: Option<bool>,
    /// Parsed and kept.
    pub used: bool,
}

/// A problem found in the firmware's tables; each becomes a `Warn` event (B6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    pub signature: Signature,
    pub phys: u64,
    /// What is wrong, e.g. `bad_checksum`, `entry_length`, `fallback_rsdt`.
    pub what: &'static str,
    /// The register or field concerned, if any.
    pub field: &'static str,
    pub detail: u64,
    /// How often this problem (same signature, `what` and `field`) was seen; `phys` and `detail` are the
    /// first one's.
    pub count: u32,
}

impl Note {
    pub fn table(signature: Signature, phys: u64, what: &'static str, detail: u64) -> Note {
        Note { signature, phys, what, field: "", detail, count: 1 }
    }
    pub fn field(signature: Signature, phys: u64, what: &'static str, field: &'static str, detail: u64) -> Note {
        Note { signature, phys, what, field, detail, count: 1 }
    }
    fn same_kind(&self, o: &Note) -> bool {
        self.signature == o.signature && self.what == o.what && self.field == o.field
    }
}

/// The problems found, one entry per kind. A hostile table can repeat one problem millions of times (a
/// 16 MiB MADT of 2-byte entries); counting repeats keeps memory and the log bounded by the number of
/// distinct problems, which the parsers' fixed `what` strings bound.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notes(Vec<Note>);

impl Notes {
    pub fn new() -> Notes {
        Notes(Vec::new())
    }
    pub fn push(&mut self, n: Note) {
        match self.0.iter_mut().find(|o| o.same_kind(&n)) {
            Some(o) => o.count = o.count.saturating_add(1),
            None => self.0.push(n),
        }
    }
    /// Every problem seen, repeats included.
    pub fn total(&self) -> u64 {
        self.0.iter().map(|n| n.count as u64).sum()
    }
}

impl core::ops::Deref for Notes {
    type Target = [Note];
    fn deref(&self) -> &[Note] {
        &self.0
    }
}

/// Everything read from the firmware's tables.
#[derive(Clone, Debug, Default)]
pub struct Acpi {
    pub rsdp: Option<rsdp::Rsdp>,
    pub root: Option<RootKind>,
    pub tables: Vec<TableInfo>,
    pub fadt: Option<fadt::Fadt>,
    pub madt: Option<madt::Madt>,
    pub hpet: Option<hpet::Hpet>,
    pub mcfg: Vec<mcfg::Ecam>,
    pub ivrs: Option<ivrs::Ivrs>,
    pub srat: Option<srat::Srat>,
    pub dsdt: Option<TableInfo>,
    /// `\_S5` and where it was found (signature, physical address of that table).
    pub s5: Option<(s5::S5, Signature, u64)>,
    pub notes: Notes,
}

impl Acpi {
    /// True if the root table lists a table with this signature (header read, whatever its checksum).
    pub fn has(&self, sig: &Signature) -> bool {
        self.tables.iter().any(|t| &t.header.signature == sig)
    }
    pub fn count(&self, sig: &Signature) -> usize {
        self.tables.iter().filter(|t| &t.header.signature == sig).count()
    }
}
