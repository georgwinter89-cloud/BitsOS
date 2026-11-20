//! Where things lie on this card, as arithmetic. `RCC_CONFIG_MEMSIZE` read 16 304 MiB under Linux on 5.11.
//! (`mem_info_vram_total`), BAR0 is 16 GiB, so the CPU sees all of VRAM.

/// A:`amdgpu_discovery.h`:29. 6.6 read 8 KiB; `amdgpu::gpucore::disc::TMR_SIZE` still says so for the 780M, and that
/// stays.
pub const DISCOVERY_READ: usize = 10 << 10;
/// A:`amdgpu_discovery.h`:30: the blob lies this far below the end of VRAM.
pub const DISCOVERY_FROM_END: u64 = 64 << 10;
/// `include/discovery.h`:28.
pub const BINARY_SIGNATURE: u32 = 0x2821_1407;
/// Everything up to and including `binary_size` (`include/discovery.h`:56-63).
pub const HEADER_BYTES: usize = 12;
/// The most that is read of a place the card's mailbox names. `binary_size` is 16 bits, so no table reaches past
/// it; Linux reads whatever size the register states (A:`amdgpu_discovery.c`:343).
pub const MAILBOX_READ_MAX: usize = 64 << 10;
/// A:`amdgpu_discovery.c`:746: the checksum covers what follows its own field.
const CHECKSUM_FROM: usize = 10;

pub const GART_BYTES: u64 = 512 << 20; // A:gmc_v12_0.c:764
pub const DMA_BITS: u32 = 44; // A:gmc_v12_0.c:843, for GC 12.0.0 and 12.0.1
/// Where Linux found VRAM in the card's own address space on 5.11. (journal), read from the card, not chosen by a
/// driver.
pub const VRAM_MC_BASE_SEEN: u64 = 0x80_0000_0000;

pub const MEMSIZE_MIB_SEEN: u32 = 16_304;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemsizeError {
    /// All ones: the card does not answer (A:`amdgpu_discovery.c`:313).
    Gone,
    /// Zero means the blob is in system memory (A:`amdgpu_discovery.c`:315), which is an APU's path and not this
    /// card's.
    Zero,
}

pub fn vram_bytes(memsize_reg: u32) -> Result<u64, MemsizeError> {
    match memsize_reg {
        u32::MAX => Err(MemsizeError::Gone),
        0 => Err(MemsizeError::Zero),
        mib => Ok((mib as u64) << 20),
    }
}

/// Byte offset into VRAM, and so into BAR0.
pub fn discovery_offset(memsize_reg: u32) -> Result<u64, MemsizeError> {
    // The smallest non-zero register value is 1 MiB, so this cannot underflow.
    Ok(vram_bytes(memsize_reg)? - DISCOVERY_FROM_END)
}

/// The physical address to read the blob at. `None` if the read would not fit inside BAR0: the rule that the whole read
/// lies in the card's own window is what makes it safe to try on a card nobody has probed.
pub fn discovery_phys(bar0_base: u64, bar0_bytes: u64, memsize_reg: u32) -> Option<u64> {
    let offset = discovery_offset(memsize_reg).ok()?;
    if offset.checked_add(DISCOVERY_READ as u64)? > bar0_bytes {
        return None;
    }
    bar0_base.checked_add(offset)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobHeader {
    pub version_major: u16,
    pub version_minor: u16,
    pub binary_size: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobRefusal {
    TooShort {
        read: usize,
    },
    Signature {
        found: u32,
    },
    /// Linux checks neither end before it sums the bytes. Past the read would be someone else's memory or stale bytes,
    /// and a short read of a longer table is how an 8 KiB read would lose IPs without saying so.
    Size {
        binary_size: u16,
        read: usize,
    },
    Checksum {
        stored: u16,
        computed: u16,
    },
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// Accepts the bytes read from the card, or says why not. A:`amdgpu_discovery.c`:737-755, and stricter on the size.
pub fn accept(read: &[u8]) -> Result<BlobHeader, BlobRefusal> {
    if read.len() < HEADER_BYTES {
        return Err(BlobRefusal::TooShort { read: read.len() });
    }
    let found = u32::from_le_bytes([read[0], read[1], read[2], read[3]]);
    if found != BINARY_SIGNATURE {
        return Err(BlobRefusal::Signature { found });
    }
    let binary_size = le16(read, 10);
    let size = binary_size as usize;
    if size < HEADER_BYTES || size > read.len() {
        return Err(BlobRefusal::Size { binary_size, read: read.len() });
    }
    let stored = le16(read, 8);
    // Stored at offset 8, so the sum from CHECKSUM_FROM leaves the size field in and the checksum out.
    let computed = read[CHECKSUM_FROM..size].iter().fold(0u16, |sum, &b| sum.wrapping_add(b as u16));
    if stored != computed {
        return Err(BlobRefusal::Checksum { stored, computed });
    }
    Ok(BlobHeader { version_major: le16(read, 4), version_minor: le16(read, 6), binary_size })
}

/// True if the card can address this whole range of host memory.
pub fn dma_reachable(addr: u64, bytes: u64) -> bool {
    match addr.checked_add(bytes) {
        Some(end) => end <= 1u64 << DMA_BITS,
        None => false,
    }
}
