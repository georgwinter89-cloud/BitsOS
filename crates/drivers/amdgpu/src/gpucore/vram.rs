//! Reading the discovery binary out of graphics memory, two ways:
//!
//! - **Indirect (Linux's way):** the MM_INDEX/MM_INDEX_HI/MM_DATA window in the register BAR
//!   (linux-6.6 amdgpu_device.c:246-276 `amdgpu_device_mm_access`; amdgpu_discovery.c:94-97 the dword offsets
//!   0x0/0x6/0x1). Every dword is an index **write** and a data read. The window is a driver-owned indirect pair
//!   (`dcn::MmWindow` is the only way to it): BitsOS saves both index registers first and puts them back after
//!   each chunk, so whatever was there before is there afterwards. Linux holds its lock for the whole access;
//!   BitsOS holds it (interrupts off) for one short chunk only (B4).
//! - **Carve-out (BitsOS's read-only way, APUs):** the carve-out is system memory; its CPU address is MMHUB's
//!   MMMC_VM_FB_OFFSET (bits 23:0, << 24: CPU address bits 47:24), which Linux takes as the APU's aperture
//!   (linux-6.6 gmc_v11_0.c:720-723, mmhub_v3_0_1.c:510-515); the display hub's DCN_VM_FB_OFFSET holds the same
//!   value (both 0x840 on the Mini-PC, 27.10.: CPU 0x8_4000_0000). The binary lies at that address plus VRAM
//!   size (MEMSIZE, any size: 512 MiB that day, 8 GB as Georg usually sets it) minus 64 KiB.
//!   GX-7b: on 27.10. that address read all ones although the arithmetic matches Linux's; `CarvePlan` samples
//!   the carve-out's start, the page under the discovery area and the area itself, so the probe can tell "the CPU
//!   does not reach the carve-out at this address" from "only the top 64 KiB is closed to the CPU".
//!
//! The functions here are pure over small traits, so the host tests run them against fake registers and memory.

use alloc::vec::Vec;

/// Byte offsets in the register BAR (dwords 0x0, 0x6, 0x1); the owner is `dcn`.
pub use crate::dcn::{MM_DATA, MM_INDEX, MM_INDEX_HI};
/// Bit 31 of MM_INDEX selects the memory aperture (Linux sets it for every access).
const MM_APERTURE: u32 = 0x8000_0000;
/// Dwords per locked chunk: the live caller holds its lock (interrupts off) for one chunk only, about 37
/// uncached accesses (2 saves, 16 index writes, 1 MM_INDEX_HI, 16 data reads, 2 restores): tens of
/// microseconds, not hundreds.
pub const CHUNK_DWORDS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VramError {
    /// A register access was refused (offset outside the BAR) or failed.
    Register { byte: u32 },
    /// Position or length not dword aligned, or the range wraps.
    Range,
    /// No carve-out address: DCN_VM_FB_OFFSET reads 0.
    NoCarveOut,
    /// The window stayed held by another user (bounded tries, B4).
    Busy,
}

/// The register access the indirect read needs.
pub trait IndexRegs {
    fn read(&self, byte: u32) -> Option<u32>;
    /// Writes; `false` if the write was refused.
    fn write(&self, byte: u32, value: u32) -> bool;
    /// Runs `f` with the window held against other users (the live one takes a lock with bounded tries;
    /// fakes need none); `None` = busy.
    fn locked<T>(&self, f: impl FnOnce(&Self) -> T) -> Option<T> {
        Some(f(self))
    }
}

fn rd(r: &impl IndexRegs, byte: u32) -> Result<u32, VramError> {
    r.read(byte).ok_or(VramError::Register { byte })
}

fn wr(r: &impl IndexRegs, byte: u32, v: u32) -> Result<(), VramError> {
    if r.write(byte, v) { Ok(()) } else { Err(VramError::Register { byte }) }
}

/// One chunk: save both index registers, read, put them back (also when a read fails halfway).
fn chunk(r: &impl IndexRegs, pos: u64, out: &mut Vec<u8>, dwords: usize) -> Result<(), VramError> {
    let (index, hi) = (rd(r, MM_INDEX)?, rd(r, MM_INDEX_HI)?);
    let mut res = Ok(());
    let mut cur_hi = None;
    for i in 0..dwords {
        let p = pos + i as u64 * 4;
        let want_hi = (p >> 31) as u32;
        let step = (|| {
            wr(r, MM_INDEX, p as u32 | MM_APERTURE)?;
            if cur_hi != Some(want_hi) {
                wr(r, MM_INDEX_HI, want_hi)?;
                cur_hi = Some(want_hi);
            }
            rd(r, MM_DATA)
        })();
        match step {
            Ok(v) => out.extend_from_slice(&v.to_le_bytes()),
            Err(e) => {
                res = Err(e);
                break;
            }
        }
    }
    let back = wr(r, MM_INDEX, index).and(wr(r, MM_INDEX_HI, hi));
    res.and(back)
}

/// `len` bytes of graphics memory at `pos` through the index/data window.
pub fn read_indirect(r: &impl IndexRegs, pos: u64, len: usize) -> Result<Vec<u8>, VramError> {
    if pos % 4 != 0 || len % 4 != 0 || pos.checked_add(len as u64).is_none() {
        return Err(VramError::Range);
    }
    let mut out = Vec::with_capacity(len);
    let mut done = 0usize;
    while done < len {
        let n = CHUNK_DWORDS.min((len - done) / 4);
        r.locked(|r| chunk(r, pos + done as u64, &mut out, n)).ok_or(VramError::Busy)??;
        done += n * 4;
    }
    Ok(out)
}

/// Where the discovery binary lies in graphics memory: VRAM size (MEMSIZE in MB) minus 64 KiB; `None` for
/// MEMSIZE 0 (Linux then asks ACPI for the TMR: not done here) or a size below 64 KiB.
pub fn discovery_pos(memsize_mb: u32) -> Option<u64> {
    (u64::from(memsize_mb) << 20).checked_sub(super::disc::TMR_OFFSET).filter(|_| memsize_mb != 0)
}

/// One read-only look into the carve-out: what, its offset in graphics memory, bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub what: &'static str,
    pub offset: u64,
    pub len: usize,
}

/// The carve-out as the CPU sees it and the places the probe reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarvePlan {
    /// CPU physical address of graphics memory offset 0.
    pub base: u64,
    pub size: u64,
    pub samples: alloc::vec::Vec<Sample>,
}

pub const START: &str = "start";
pub const BELOW: &str = "below the discovery area";
pub const DISCOVERY: &str = "discovery area";

impl CarvePlan {
    /// `fb_offset_reg`: MMMC_VM_FB_OFFSET or DCN_VM_FB_OFFSET (bits 23:0 are CPU address bits 47:24); MEMSIZE in MB.
    pub fn new(fb_offset_reg: u32, memsize_mb: u32) -> Result<CarvePlan, VramError> {
        let base = u64::from(fb_offset_reg & 0xFF_FFFF) << 24;
        if base == 0 {
            return Err(VramError::NoCarveOut);
        }
        let pos = discovery_pos(memsize_mb).ok_or(VramError::Range)?;
        let size = u64::from(memsize_mb) << 20;
        base.checked_add(size).ok_or(VramError::Range)?;
        let mut samples = alloc::vec![Sample { what: START, offset: 0, len: 4096 }];
        if pos >= 4096 {
            samples.push(Sample { what: BELOW, offset: pos - 4096, len: 4096 });
        }
        samples.push(Sample { what: DISCOVERY, offset: pos, len: super::disc::TMR_SIZE });
        Ok(CarvePlan { base, size, samples })
    }

    pub fn phys(&self, s: &Sample) -> u64 {
        self.base + s.offset
    }
}

/// What a sample held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// Every byte 0xff: what a read the memory system does not answer (or blocks) gives.
    AllOnes,
    AllZero,
    Data { nonzero: usize },
}

pub fn look(bytes: &[u8]) -> Look {
    if bytes.iter().all(|&b| b == 0xFF) {
        Look::AllOnes
    } else if bytes.iter().all(|&b| b == 0) {
        Look::AllZero
    } else {
        Look::Data { nonzero: bytes.iter().filter(|&&b| b != 0).count() }
    }
}

/// The conclusion from the looks (`None` where a sample was not read) and whether the discovery copy parsed.
pub fn conclude(start: Option<Look>, below: Option<Look>, disc: Option<Look>, parsed: bool) -> &'static str {
    let ones = |l: Option<Look>| l == Some(Look::AllOnes);
    let reads = |l: Option<Look>| matches!(l, Some(Look::AllZero | Look::Data { .. }));
    if parsed {
        "the CPU reads the discovery binary in the carve-out"
    } else if ones(disc) && reads(below) {
        "the carve-out answers the CPU up to the discovery area, whose 64 KiB read all ones: firmware closes the top of the carve-out to the CPU; the GPU's own path (MM_INDEX, ktest gx.disc.indirect) reads it"
    } else if ones(disc) && ones(below) && ones(start) {
        "every sample reads all ones: the CPU does not reach graphics memory at this address (wrong base, or not decoded for the CPU)"
    } else if ones(disc) && ones(below) && reads(start) {
        "the carve-out's start answers but its top does not: graphics memory is smaller than MEMSIZE says, or the top is closed to the CPU"
    } else if disc.is_none() {
        "the discovery area was not read"
    } else {
        "the discovery area answers but holds no valid binary"
    }
}
