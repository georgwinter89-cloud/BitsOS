//! GX-10 PS: the two files of the secure processor itself, cut by their documented headers only (the licence forbids
//! looking further): `psp_14_0_3_sos.bin` (the bootloader's eight parts, the secure OS, the TOC and the register
//! list) and `psp_14_0_3_ta.bin` (the ASD and the trusted applications). Both are header v2.0: the common header, a
//! count, then 16-byte descriptors of type, version, offset from the microcode array and size
//! (`psp_firmware_header_v2_0`, `ta_firmware_header_v2_0`, `psp_fw_bin_desc`, amdgpu_ucode.h:111-140 and :160-178;
//! `psp_init_sos_microcode` amdgpu_psp.c:4159 with `parse_sos_bin_descriptor` :4020-4112, `psp_init_ta_microcode`
//! :4448 with `parse_ta_v2_microcode` :4420 and `parse_ta_bin_descriptor` :4300-4363).

use crate::gpucore::ucode::{self, Kind, Why};
use core::ops::Range;
use rdna4_core::bootseq::Part;

/// `UCODE_MAX_PSP_PACKAGING` (amdgpu_ucode.h:473): Linux refuses a file with this many descriptors or more.
pub const MAX_DESCRIPTORS: u32 = 26;
/// `psp_fw_type` of the two parts that go over the ring, not to the bootloader (amdgpu_ucode.h:118-133).
pub const TYPE_TOC: u32 = 4;
pub const TYPE_RL: u32 = 6;
/// `ta_fw_type` (amdgpu_ucode.h:160-171).
pub const TA_ASD: u32 = 1;
pub const TA_RAS: u32 = 3;
pub const TA_HDCP: u32 = 4;
pub const TA_DTM: u32 = 5;

/// One descriptor: its type, its version and its bytes in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    pub kind: u32,
    pub version: u32,
    pub range: Range<usize>,
}

fn le32(b: &[u8], at: usize) -> Result<u32, Why> {
    let s = b.get(at..at + 4).ok_or(Why::HeaderSize { header: b.len() as u32, need: at as u64 + 4 })?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The descriptors of a v2.0 file, each one's bytes inside the file. A type that comes twice is an error: Linux
/// would take the last, and which bytes go to the card must not hang on that.
fn descriptors(b: &[u8], kind: Kind, what: &'static str) -> Result<alloc::vec::Vec<Desc>, Why> {
    let h = ucode::header(b)?;
    if h.header_major != 2 || h.header_minor != 0 {
        return Err(Why::Version { kind, major: h.header_major, minor: h.header_minor });
    }
    let count = le32(b, 32)?;
    if count == 0 || count >= MAX_DESCRIPTORS {
        return Err(Why::TaCount(count));
    }
    let array = u64::from(h.ucode_array_offset_bytes);
    if 36 + u64::from(count) * 16 > array {
        return Err(Why::TaCount(count));
    }
    let mut out: alloc::vec::Vec<Desc> = alloc::vec::Vec::new();
    for i in 0..count as usize {
        let at = 36 + i * 16;
        let (kind, version, off, size) = (le32(b, at)?, le32(b, at + 4)?, le32(b, at + 8)?, le32(b, at + 12)?);
        let range = ucode::part(b.len(), array + u64::from(off), u64::from(size), what)?;
        if out.iter().any(|d| d.kind == kind) {
            return Err(Why::Range { what: "a part twice", offset: u64::from(kind), size: 0 });
        }
        out.push(Desc { kind, version, range });
    }
    Ok(out)
}

/// `psp_14_0_3_sos.bin` as the driver uses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SosFile {
    pub parts: alloc::vec::Vec<Desc>,
}

impl SosFile {
    pub fn parse(b: &[u8]) -> Result<SosFile, Why> {
        Ok(SosFile { parts: descriptors(b, Kind::Toc, "psp part")? })
    }

    /// A part by its type: `None` when the file has no such descriptor or it is empty (`is_psp_fw_valid`,
    /// amdgpu_psp.c:4711-4714).
    pub fn part(&self, kind: u32) -> Option<&Desc> {
        self.parts.iter().find(|d| d.kind == kind && !d.range.is_empty())
    }

    pub fn boot(&self, p: Part) -> Option<&Desc> {
        self.part(p as u32)
    }

    /// `psp->sos.fw_version`: decides whether Linux asks for the firmware's reserved region (amdgpu_psp.c:1083-1086).
    pub fn sos_version(&self) -> Option<u32> {
        self.parts.iter().find(|d| d.kind == Part::Sos as u32).map(|d| d.version)
    }

    pub fn toc(&self) -> Result<&Desc, Why> {
        self.part(TYPE_TOC).ok_or(Why::Range { what: "psp toc", offset: 0, size: 0 })
    }
}

/// `psp_14_0_3_ta.bin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaFile {
    pub parts: alloc::vec::Vec<Desc>,
}

impl TaFile {
    pub fn parse(b: &[u8]) -> Result<TaFile, Why> {
        Ok(TaFile { parts: descriptors(b, Kind::Ta, "trusted application")? })
    }

    /// An application by its type; Linux skips one whose descriptor is missing or empty (`psp_asd_initialize`
    /// amdgpu_psp.c:1681, `psp_ras_initialize` :2368, `psp_hdcp_initialize` :2550, `psp_dtm_initialize` :2624).
    pub fn app(&self, kind: u32) -> Option<&Desc> {
        self.parts.iter().find(|d| d.kind == kind && !d.range.is_empty())
    }
}
