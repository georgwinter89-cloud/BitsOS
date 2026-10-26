//! Reading tables out of physical memory through a `PhysMem` (the kernel's direct map + `hal::mmio`, or a
//! fake in host tests). A table is copied once into a heap buffer bounded by its own length, so parsers
//! work on plain slices and a table across pages or descriptors needs no special case.

use crate::sdt::{checksum_ok, Header, Signature, HEADER_LEN};
use alloc::vec::Vec;

/// The largest table BitsOS copies. A bound against a hostile length (R1: the size of a single request, not
/// a count of things); real tables are far smaller (the Mini-PC's biggest, the DSDT, is well below 1 MiB).
pub const TABLE_MAX: u32 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemError {
    /// Neither RAM nor firmware-reserved memory (a hole, MMIO, unusable): never read as a table.
    NotTableMemory,
    /// Reserved memory the register guard keeps out (an ECAM window, a disk controller's BAR).
    Guarded,
    /// `hal::mmio::map_device` failed.
    MapFailed,
    /// Address arithmetic overflowed or the range is past the CPU's physical addresses.
    OutOfRange,
    /// The parse's total read budget (`Metered`) is spent.
    Budget,
}

impl MemError {
    pub fn name(self) -> &'static str {
        match self {
            MemError::NotTableMemory => "not_table_memory",
            MemError::Guarded => "guarded",
            MemError::MapFailed => "map_failed",
            MemError::OutOfRange => "out_of_range",
            MemError::Budget => "budget_spent",
        }
    }
}

pub trait PhysMem {
    /// Fills `out` with the bytes at `phys..phys + out.len()`, or says why it cannot.
    fn read(&mut self, phys: u64, out: &mut [u8]) -> Result<(), MemError>;
    /// `start..end` holds registers a table named (HPET, I/O APIC, ECAM, ...): never read it as a table.
    fn avoid(&mut self, _start: u64, _end: u64) {}
    /// Bytes this reader will still read (checked before a table's buffer is allocated).
    fn budget_left(&self) -> u64 {
        u64::MAX
    }
}

/// The total bytes one parse reads (headers included): twice the largest table. Real firmware stays far
/// below (the Mini-PC's tables together are about 1 MiB), so this only stops hostile tables from stalling
/// boot or filling G0's heap, which never frees (a size bound, R1; not a count of tables).
pub const PARSE_BUDGET: u64 = 2 * TABLE_MAX as u64;

/// A `PhysMem` that stops after `left` bytes.
pub struct Metered<'a> {
    inner: &'a mut dyn PhysMem,
    left: u64,
}

impl<'a> Metered<'a> {
    pub fn new(inner: &'a mut dyn PhysMem, budget: u64) -> Metered<'a> {
        Metered { inner, left: budget }
    }
    pub fn spent(&self) -> bool {
        self.left == 0
    }
}

impl PhysMem for Metered<'_> {
    fn read(&mut self, phys: u64, out: &mut [u8]) -> Result<(), MemError> {
        let n = out.len() as u64;
        if n > self.left {
            self.left = 0;
            return Err(MemError::Budget);
        }
        self.left -= n;
        self.inner.read(phys, out)
    }
    fn avoid(&mut self, start: u64, end: u64) {
        self.inner.avoid(start, end)
    }
    fn budget_left(&self) -> u64 {
        self.left.min(self.inner.budget_left())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableError {
    ZeroAddress,
    Unreadable(MemError),
    /// Length below the 36-byte header (or below what the table type needs).
    TooShort(u32),
    /// Length above `TABLE_MAX`.
    TooLong(u32),
    BadSignature(Signature),
    NoMemory,
}

impl TableError {
    pub fn name(&self) -> &'static str {
        match self {
            TableError::ZeroAddress => "zero_address",
            TableError::Unreadable(e) => e.name(),
            TableError::TooShort(_) => "too_short",
            TableError::TooLong(_) => "too_long",
            TableError::BadSignature(_) => "bad_signature",
            TableError::NoMemory => "no_memory",
        }
    }
}

/// A table copied out of firmware memory.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub phys: u64,
    pub header: Header,
    pub bytes: Vec<u8>,
    pub checksum_ok: bool,
}

/// Reads only the header at `phys`.
pub fn header(mem: &mut dyn PhysMem, phys: u64) -> Result<Header, TableError> {
    if phys == 0 {
        return Err(TableError::ZeroAddress);
    }
    let mut h = [0u8; HEADER_LEN];
    mem.read(phys, &mut h).map_err(TableError::Unreadable)?;
    Header::parse(&h).ok_or(TableError::TooShort(0))
}

/// Copies the whole table at `phys`: header first, length checked, then exactly `length` bytes.
pub fn load(mem: &mut dyn PhysMem, phys: u64, want: Option<Signature>) -> Result<Loaded, TableError> {
    let h = header(mem, phys)?;
    if let Some(w) = want
        && h.signature != w
    {
        return Err(TableError::BadSignature(h.signature));
    }
    if (h.length as usize) < HEADER_LEN {
        return Err(TableError::TooShort(h.length));
    }
    if h.length > TABLE_MAX {
        return Err(TableError::TooLong(h.length));
    }
    if phys.checked_add(h.length as u64).is_none() {
        return Err(TableError::Unreadable(MemError::OutOfRange));
    }
    if h.length as u64 > mem.budget_left() {
        return Err(TableError::Unreadable(MemError::Budget));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(h.length as usize).map_err(|_| TableError::NoMemory)?;
    bytes.resize(h.length as usize, 0);
    mem.read(phys, &mut bytes).map_err(TableError::Unreadable)?;
    // The firmware may change memory under us only in theory; take the header from the copy we checked.
    let header = Header::parse(&bytes).ok_or(TableError::TooShort(h.length))?;
    if header.length != h.length {
        return Err(TableError::TooShort(header.length));
    }
    let checksum_ok = checksum_ok(&bytes);
    Ok(Loaded { phys, header, bytes, checksum_ok })
}
