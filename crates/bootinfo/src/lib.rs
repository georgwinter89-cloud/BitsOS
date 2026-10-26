//! BootInfo v1: what `bitsboot` hands to the kernel and what Reload passes on unchanged.
//!
//! Layout (little endian): a 32-byte header, then tagged records, each 8-byte aligned. Readers skip tags they do
//! not know, so loaders and kernels of different ages interoperate (V1's lesson: a fixed struct forced every
//! layout change onto the stick). New fields go into new tags or at the end of a record (readers check `size`).
//!
//! Desk contract (`plan/CONTRACTS.md` §BootInfo). Lanes only fill values; a new tag or field is a change request.
#![no_std]
#![forbid(unsafe_code)]

pub mod layout;
mod records;
mod writer;

pub use records::*;
pub use writer::{Writer, WriteError};

pub const MAGIC: [u8; 8] = *b"BITSBOOT";
pub const VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 32;

/// UEFI memory types BitsOS adds (OS-defined range 0x8000_0000..). The frame allocator never frees `KEEP`.
pub mod memtype {
    pub const CONVENTIONAL: u32 = 7;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const UNUSABLE: u32 = 8;
    pub const ACPI_RECLAIM: u32 = 9;
    pub const ACPI_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT_SPACE: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT: u32 = 14;
    /// BootInfo itself, CONFIG text, stick record, crash page, trampoline, relocation stub: never freed.
    pub const BITS_KEEP: u32 = 0x8000_0001;
    /// Initramfs and firmware blobs: freed only when a Reload no longer needs them (V2-PLAN §2.5).
    pub const BITS_MODULE: u32 = 0x8000_0002;
    /// The loaded kernel image (PT_LOAD segments incl. bss and the boot stack).
    pub const BITS_KERNEL: u32 = 0x8000_0003;
    /// Page tables built by bitsboot; live until MM builds its own.
    pub const BITS_PAGETABLE: u32 = 0x8000_0004;

    /// Memory the frame allocator may hand out once the hand-over data is consumed.
    pub fn is_free_after_handover(ty: u32) -> bool {
        matches!(ty, CONVENTIONAL | LOADER_CODE | LOADER_DATA | BOOT_SERVICES_CODE | BOOT_SERVICES_DATA)
    }
    /// Memory that is RAM (cacheable write-back), whatever its use.
    pub fn is_ram(ty: u32) -> bool {
        matches!(
            ty,
            CONVENTIONAL
                | LOADER_CODE
                | LOADER_DATA
                | BOOT_SERVICES_CODE
                | BOOT_SERVICES_DATA
                | RUNTIME_SERVICES_CODE
                | RUNTIME_SERVICES_DATA
                | ACPI_RECLAIM
                | ACPI_NVS
                | PERSISTENT
                | BITS_KEEP
                | BITS_MODULE
                | BITS_KERNEL
                | BITS_PAGETABLE
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u32),
    BadSize,
    RecordOverrun { offset: usize },
}

/// A parsed view over BootInfo bytes. Cheap to copy; all getters walk the records.
#[derive(Clone, Copy)]
pub struct BootInfo<'a> {
    bytes: &'a [u8],
    record_count: u32,
}

pub(crate) fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub(crate) fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// One raw record: tag and payload.
#[derive(Clone, Copy, Debug)]
pub struct Record<'a> {
    pub tag: u32,
    pub payload: &'a [u8],
}

pub struct Records<'a> {
    bytes: &'a [u8],
    off: usize,
    left: u32,
}

impl<'a> Iterator for Records<'a> {
    type Item = Record<'a>;
    fn next(&mut self) -> Option<Record<'a>> {
        if self.left == 0 || self.off + 8 > self.bytes.len() {
            return None;
        }
        let tag = rd_u32(self.bytes, self.off);
        let size = rd_u32(self.bytes, self.off + 4) as usize;
        let start = self.off + 8;
        let end = start.checked_add(size)?;
        if end > self.bytes.len() {
            return None;
        }
        self.off = (end + 7) & !7;
        self.left -= 1;
        Some(Record { tag, payload: &self.bytes[start..end] })
    }
}

impl<'a> BootInfo<'a> {
    /// Parse from the bytes the loader handed over. `bytes` may be longer than `total_size` (capacity).
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ParseError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ParseError::TooShort);
        }
        if bytes[0..8] != MAGIC {
            return Err(ParseError::BadMagic);
        }
        let version = rd_u32(bytes, 8);
        if version != VERSION {
            return Err(ParseError::UnsupportedVersion(version));
        }
        let header_size = rd_u32(bytes, 12) as usize;
        let total = rd_u32(bytes, 16) as usize;
        if header_size != HEADER_SIZE || total < HEADER_SIZE || total > bytes.len() {
            return Err(ParseError::BadSize);
        }
        let record_count = rd_u32(bytes, 24);
        let bi = BootInfo { bytes: &bytes[..total], record_count };
        // Validate every record fits, so later getters never see a short payload.
        let mut off = HEADER_SIZE;
        for _ in 0..record_count {
            if off + 8 > total {
                return Err(ParseError::RecordOverrun { offset: off });
            }
            let size = rd_u32(bytes, off + 4) as usize;
            let end = off + 8 + size;
            if end > total {
                return Err(ParseError::RecordOverrun { offset: off });
            }
            off = (end + 7) & !7;
        }
        Ok(bi)
    }

    /// Size of the total hand-over block (header + records).
    pub fn total_size(&self) -> usize {
        self.bytes.len()
    }

    /// Reads the header's `total_size` field without a full parse (the kernel entry uses it to size the slice).
    pub fn peek_total_size(header: &[u8]) -> Option<usize> {
        if header.len() < HEADER_SIZE || header[0..8] != MAGIC {
            return None;
        }
        Some(rd_u32(header, 16) as usize)
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn records(&self) -> Records<'a> {
        Records { bytes: self.bytes, off: HEADER_SIZE, left: self.record_count }
    }

    pub fn find(&self, tag: u32) -> Option<&'a [u8]> {
        self.records().find(|r| r.tag == tag).map(|r| r.payload)
    }

    pub fn memory_map(&self) -> Option<MemoryMap<'a>> {
        self.find(tag::MEMORY_MAP).and_then(MemoryMap::parse)
    }
    pub fn rsdp(&self) -> Option<Rsdp> {
        self.find(tag::RSDP).and_then(Rsdp::parse)
    }
    pub fn smbios(&self) -> Option<Smbios> {
        self.find(tag::SMBIOS).and_then(Smbios::parse)
    }
    pub fn framebuffer(&self) -> Option<Framebuffer> {
        self.find(tag::FRAMEBUFFER).and_then(Framebuffer::parse)
    }
    pub fn mode_list(&self) -> Option<ModeList<'a>> {
        self.find(tag::MODE_LIST).and_then(ModeList::parse)
    }
    /// CONFIG.TXT as the loader read it (any length; no 2047-byte limit as in V1).
    pub fn config(&self) -> Option<&'a [u8]> {
        self.find(tag::CONFIG)
    }
    /// The monitor's EDID (DP-3), when the loader found one.
    pub fn edid(&self) -> Option<&'a [u8]> {
        self.find(tag::EDID)
    }
    pub fn stick(&self) -> Option<Stick<'a>> {
        self.find(tag::STICK).and_then(Stick::parse)
    }
    pub fn loader_log(&self) -> Option<&'a [u8]> {
        self.find(tag::LOADER_LOG)
    }
    pub fn tsc(&self) -> Option<Tsc> {
        self.find(tag::TSC).and_then(Tsc::parse)
    }
    pub fn reload(&self) -> Option<Reload> {
        self.find(tag::RELOAD).and_then(Reload::parse)
    }
    pub fn modules(&self) -> impl Iterator<Item = Module<'a>> + 'a {
        self.records().filter(|r| r.tag == tag::MODULE).filter_map(|r| Module::parse(r.payload))
    }
    pub fn crash_page(&self) -> Option<PhysRange> {
        self.find(tag::CRASH_PAGE).and_then(PhysRange::parse)
    }
    pub fn trampoline(&self) -> Option<Trampoline> {
        self.find(tag::TRAMPOLINE).and_then(Trampoline::parse)
    }
    pub fn reload_stub(&self) -> Option<PhysRange> {
        self.find(tag::RELOAD_STUB).and_then(PhysRange::parse)
    }
    pub fn kernel_image(&self) -> Option<KernelImage> {
        self.find(tag::KERNEL_IMAGE).and_then(KernelImage::parse)
    }
    pub fn generation(&self) -> Option<Generation> {
        self.find(tag::GENERATION).and_then(Generation::parse)
    }
    pub fn page_tables(&self) -> Option<PageTables> {
        self.find(tag::PAGE_TABLES).and_then(PageTables::parse)
    }
    pub fn boot_id(&self) -> Option<[u8; 16]> {
        self.find(tag::BOOT_ID).filter(|p| p.len() >= 16).map(|p| {
            let mut id = [0u8; 16];
            id.copy_from_slice(&p[..16]);
            id
        })
    }
    pub fn firmware(&self) -> Option<Firmware<'a>> {
        self.find(tag::FIRMWARE).and_then(Firmware::parse)
    }
    pub fn handover(&self) -> Option<Handover> {
        self.find(tag::HANDOVER).and_then(Handover::parse)
    }
}

#[cfg(test)]
mod tests;
