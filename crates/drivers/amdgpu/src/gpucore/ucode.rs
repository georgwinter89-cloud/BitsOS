//! GX-7a: the firmware files' headers and the microcode list the PSP loads. Formats from linux-6.6
//! amdgpu/amdgpu_ucode.h (`common_firmware_header` :28 and the per-block headers), the per-microcode size and
//! offset rules of amdgpu_ucode.c `amdgpu_ucode_init_single_fw` (:740-960, PSP loading), the registration rules
//! of gfx_v11_0.c:486-560 (RS64 from the PFP header), amdgpu_rlc.c:518-555 (RLC v2.x), amdgpu_mes.c:1390-1460,
//! amdgpu_sdma.c:260-280, imu_v11_0.c:55-70, amdgpu_vcn.c:1040, amdgpu_dm.c:2140, and the skip and order rules
//! of amdgpu_psp.c (`psp_load_non_psp_fw` :2560 walks the microcode IDs in enum order, `fw_load_skip_check`
//! :2519). Every offset and size a file states is checked against the file before it is used; a part outside
//! the file, a header too short for the fields its version has, or a version the Linux code does not read is a
//! refusal (`Why`), never a panic.
//!
//! The header's `crc32` is kept but not checked: Linux never checks it (amdgpu_ucode.c:43 only prints it) and
//! in the linux-firmware-amdgpu 20261019 files for the 780M it matches the payload's CRC-32 only for
//! vcn_4_0_2 (measured 27.10.; plan/notes/gx-GX-7a.md), so a check would refuse good files.

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

/// What Linux calls the microcode IDs; the discriminant is Linux's `enum AMDGPU_UCODE_ID` value
/// (amdgpu_ucode.h:415-477), so sorting by it is Linux's load order. Only the IDs a BitsOS module can have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UcodeId {
    SdmaTh0 = 9,
    SdmaTh1 = 10,
    CpPfp = 12,
    CpMe = 13,
    Rs64Pfp = 14,
    Rs64Me = 15,
    Rs64Mec = 16,
    Rs64PfpP0Stack = 17,
    Rs64PfpP1Stack = 18,
    Rs64MeP0Stack = 19,
    Rs64MeP1Stack = 20,
    Rs64MecP0Stack = 21,
    Rs64MecP1Stack = 22,
    Rs64MecP2Stack = 23,
    Rs64MecP3Stack = 24,
    CpMec1 = 25,
    CpMec1Jt = 26,
    CpMes = 29,
    CpMesData = 30,
    CpMes1 = 31,
    CpMes1Data = 32,
    ImuI = 33,
    ImuD = 34,
    GlobalTapDelays = 35,
    Se0TapDelays = 36,
    Se1TapDelays = 37,
    Se2TapDelays = 38,
    Se3TapDelays = 39,
    RlcRestoreListCntl = 40,
    RlcRestoreListGpmMem = 41,
    RlcRestoreListSrmMem = 42,
    RlcIram = 43,
    RlcDram = 44,
    RlcP = 45,
    RlcV = 46,
    RlcG = 47,
    Vcn = 54,
    Dmcub = 60,
}

impl UcodeId {
    /// The PSP's firmware type for LOAD_IP_FW (`psp_get_fw_type`, amdgpu_psp.c:2209; numbers psp_gfx_if.h:201).
    pub const fn fw_type(self) -> u32 {
        use UcodeId::*;
        match self {
            SdmaTh0 => 71,
            SdmaTh1 => 72,
            CpPfp => 2,
            CpMe => 1,
            Rs64Pfp => 87,
            Rs64Me => 88,
            Rs64Mec => 89,
            Rs64PfpP0Stack => 90,
            Rs64PfpP1Stack => 91,
            Rs64MeP0Stack => 92,
            Rs64MeP1Stack => 93,
            Rs64MecP0Stack => 94,
            Rs64MecP1Stack => 95,
            Rs64MecP2Stack => 96,
            Rs64MecP3Stack => 97,
            CpMec1 => 4,
            CpMec1Jt => 5,
            CpMes => 33,
            CpMesData => 34,
            CpMes1 => 81,
            CpMes1Data => 82,
            ImuI => 68,
            ImuD => 69,
            GlobalTapDelays => 27,
            Se0TapDelays => 28,
            Se1TapDelays => 29,
            Se2TapDelays => 65,
            Se3TapDelays => 66,
            RlcRestoreListCntl => 22,
            RlcRestoreListGpmMem => 20,
            RlcRestoreListSrmMem => 21,
            RlcIram => 26,
            RlcDram => 48,
            RlcP => 25,
            RlcV => 7,
            RlcG => 8,
            Vcn => 13,
            Dmcub => 51,
        }
    }
}

/// Which file a module needs (the role and suffix of `ipmod`'s firmware lists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Pfp,
    Me,
    Mec,
    Rlc,
    Imu,
    /// MES scheduler pipe (`_mes_2`, fallback `_mes`).
    MesSched,
    /// MES KIQ pipe (`_mes1`): Linux 6.6 loads it only with `amdgpu_mes_kiq=1` (default 0).
    MesKiq,
    Sdma,
    Vcn,
    Dmcub,
    Toc,
    Ta,
}

/// Why a file or the list was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// Shorter than the 32-byte common header.
    Short { have: usize },
    /// `size_bytes` is not the file's length (Linux's only check, `amdgpu_ucode_validate`).
    Size { header: u32, file: usize },
    /// `header_size_bytes` below the common header, past the file, or too short for a field its version has.
    HeaderSize { header: u32, need: u64 },
    /// A header version the Linux code for this file does not read.
    Version { kind: Kind, major: u16, minor: u16 },
    /// A part the header names lies outside the file.
    Range { what: &'static str, offset: u64, size: u64 },
    /// The MEC's jump table is larger than its microcode.
    JumpTable { ucode: u32, jt_bytes: u64 },
    /// PFP says RS64 (header 2.0) and ME or MEC does not, or the other way round.
    Rs64Mixed(Kind),
    /// TA descriptors run into the microcode or past the file.
    TaCount(u32),
    /// Two files give the same microcode.
    Duplicate(UcodeId),
}

/// `common_firmware_header` (amdgpu_ucode.h:28-39), little endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub size_bytes: u32,
    pub header_size_bytes: u32,
    pub header_major: u16,
    pub header_minor: u16,
    pub ip_major: u16,
    pub ip_minor: u16,
    pub ucode_version: u32,
    pub ucode_size_bytes: u32,
    pub ucode_array_offset_bytes: u32,
    pub crc32: u32,
}

pub const COMMON_BYTES: usize = 32;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// A part `offset..offset+size` of a file of `len` bytes.
pub fn part(len: usize, offset: u64, size: u64, what: &'static str) -> Result<Range<usize>, Why> {
    let bad = Why::Range { what, offset, size };
    let end = offset.checked_add(size).ok_or_else(|| bad.clone())?;
    if end > len as u64 {
        return Err(bad);
    }
    Ok(offset as usize..end as usize)
}

/// Parses and checks the common header: the length Linux checks, a header size inside the file, the microcode
/// array inside the file.
pub fn header(b: &[u8]) -> Result<Header, Why> {
    if b.len() < COMMON_BYTES {
        return Err(Why::Short { have: b.len() });
    }
    let w = |at| le32(b, at).unwrap_or(0);
    let h = |at| le16(b, at).unwrap_or(0);
    let hd = Header {
        size_bytes: w(0),
        header_size_bytes: w(4),
        header_major: h(8),
        header_minor: h(10),
        ip_major: h(12),
        ip_minor: h(14),
        ucode_version: w(16),
        ucode_size_bytes: w(20),
        ucode_array_offset_bytes: w(24),
        crc32: w(28),
    };
    if hd.size_bytes as usize != b.len() {
        return Err(Why::Size { header: hd.size_bytes, file: b.len() });
    }
    if (hd.header_size_bytes as usize) < COMMON_BYTES || hd.header_size_bytes as usize > b.len() {
        return Err(Why::HeaderSize { header: hd.header_size_bytes, need: COMMON_BYTES as u64 });
    }
    part(b.len(), u64::from(hd.ucode_array_offset_bytes), u64::from(hd.ucode_size_bytes), "ucode array")?;
    Ok(hd)
}

/// One firmware file as read from the stick, its header checked.
#[derive(Clone, Debug)]
pub struct FwFile {
    pub kind: Kind,
    pub name: String,
    pub bytes: Vec<u8>,
    pub header: Header,
}

impl FwFile {
    pub fn new(kind: Kind, name: String, bytes: Vec<u8>) -> Result<FwFile, Why> {
        let header = header(&bytes)?;
        Ok(FwFile { kind, name, bytes, header })
    }

    /// A header field at byte `at`: it must lie inside the header the file declares.
    /// GX-7k: a MES file's microcode start (mes_firmware_header_v1_0: mes_uc_start_addr_lo @56, _hi @60), what
    /// CP_MES_PRGRM_CNTR_START takes `>> 2` (mes_v11_0.c init_microcode: `mes.uc_start_addr[pipe]`).
    pub fn mes_uc_start(&self) -> Option<u64> {
        if !matches!(self.kind, Kind::MesSched | Kind::MesKiq) {
            return None;
        }
        Some(u64::from(self.field(56).ok()?) | u64::from(self.field(60).ok()?) << 32)
    }

    fn field(&self, at: usize) -> Result<u32, Why> {
        let need = at as u64 + 4;
        if need > u64::from(self.header.header_size_bytes) {
            return Err(Why::HeaderSize { header: self.header.header_size_bytes, need });
        }
        le32(&self.bytes, at).ok_or(Why::HeaderSize { header: self.header.header_size_bytes, need })
    }

    fn part(&self, offset: u64, size: u64, what: &'static str) -> Result<Range<usize>, Why> {
        part(self.bytes.len(), offset, size, what)
    }

    fn array(&self) -> u64 {
        u64::from(self.header.ucode_array_offset_bytes)
    }

    fn version(&self, major: u16) -> Result<(), Why> {
        if self.header.header_major == major {
            Ok(())
        } else {
            Err(Why::Version { kind: self.kind, major: self.header.header_major, minor: self.header.header_minor })
        }
    }

    /// The whole microcode array (TOC; the default rule of `amdgpu_ucode_init_single_fw`).
    pub fn payload(&self) -> &[u8] {
        let a = self.header.ucode_array_offset_bytes as usize;
        self.bytes.get(a..a.saturating_add(self.header.ucode_size_bytes as usize)).unwrap_or(&[])
    }

    /// Whether `crc32` matches the payload's CRC-32 (informational only, see the module comment). Tests only
    /// (C1).
    #[cfg(test)]
    pub fn crc_matches(&self) -> bool {
        crc32(self.payload()) == self.header.crc32
    }

    /// The microcodes this file gives, with their parts (sizes as `amdgpu_ucode_init_single_fw` computes
    /// them). `rs64`: the CP mode Linux takes from the PFP header (gfx_v11_0.c:505).
    pub fn entries(&self, rs64: bool) -> Result<Vec<(UcodeId, Range<usize>)>, Why> {
        use UcodeId::*;
        let mut v = Vec::new();
        let h = self.header;
        let all = |id| -> Result<(UcodeId, Range<usize>), Why> {
            Ok((id, self.part(self.array(), u64::from(h.ucode_size_bytes), "ucode")?))
        };
        match self.kind {
            Kind::Pfp | Kind::Me | Kind::Mec if rs64 => {
                if !(h.header_major == 2 && h.header_minor == 0) {
                    return Err(Why::Rs64Mixed(self.kind));
                }
                // gfx_firmware_header_v2_0: ucode_size_bytes @36, data_size_bytes @44, data_offset_bytes @48.
                let code = self.part(self.array(), u64::from(self.field(36)?), "rs64 ucode")?;
                let data = self.part(u64::from(self.field(48)?), u64::from(self.field(44)?), "rs64 stack")?;
                let (main, stacks): (UcodeId, &[UcodeId]) = match self.kind {
                    Kind::Pfp => (Rs64Pfp, &[Rs64PfpP0Stack, Rs64PfpP1Stack]),
                    Kind::Me => (Rs64Me, &[Rs64MeP0Stack, Rs64MeP1Stack]),
                    _ => (Rs64Mec, &[Rs64MecP0Stack, Rs64MecP1Stack, Rs64MecP2Stack, Rs64MecP3Stack]),
                };
                v.push((main, code));
                v.extend(stacks.iter().map(|&s| (s, data.clone())));
            }
            Kind::Pfp | Kind::Me => {
                if h.header_major == 2 && h.header_minor == 0 && self.kind == Kind::Me {
                    return Err(Why::Rs64Mixed(self.kind));
                }
                v.push(all(if self.kind == Kind::Pfp { CpPfp } else { CpMe })?);
            }
            Kind::Mec => {
                if h.header_major == 2 && h.header_minor == 0 {
                    return Err(Why::Rs64Mixed(Kind::Mec));
                }
                self.version(1)?;
                // gfx_firmware_header_v1_0: jt_offset @36, jt_size @40, both in dwords.
                let jt_off = u64::from(self.field(36)?) * 4;
                let jt = u64::from(self.field(40)?) * 4;
                let size = u64::from(h.ucode_size_bytes)
                    .checked_sub(jt)
                    .ok_or(Why::JumpTable { ucode: h.ucode_size_bytes, jt_bytes: jt })?;
                v.push((CpMec1, self.part(self.array(), size, "mec")?));
                let at = self.array().checked_add(jt_off).ok_or(Why::Range { what: "mec jt", offset: jt_off, size: jt })?;
                v.push((CpMec1Jt, self.part(at, jt, "mec jt")?));
            }
            Kind::Rlc => self.rlc(&mut v)?,
            Kind::Imu => {
                self.version(1)?;
                // imu_firmware_header_v1_0: iram size @32, dram size @40; Linux places the DRAM part right after
                // the IRAM part and ignores the two offset fields.
                let iram = u64::from(self.field(32)?);
                let dram = u64::from(self.field(40)?);
                v.push((ImuI, self.part(self.array(), iram, "imu iram")?));
                let at = self.array() + iram;
                v.push((ImuD, self.part(at, dram, "imu dram")?));
            }
            Kind::MesSched | Kind::MesKiq => {
                self.version(1)?;
                // mes_firmware_header_v1_0: ucode size @36 offset @40, data size @48 offset @52 (from the file start).
                let (code, data) = if self.kind == Kind::MesSched { (CpMes, CpMesData) } else { (CpMes1, CpMes1Data) };
                v.push((code, self.part(u64::from(self.field(40)?), u64::from(self.field(36)?), "mes ucode")?));
                v.push((data, self.part(u64::from(self.field(52)?), u64::from(self.field(48)?), "mes data")?));
            }
            Kind::Sdma => {
                self.version(2)?;
                // sdma_firmware_header_v2_0: ctx size @36 (at the array), ctl offset @48 and size @52.
                v.push((SdmaTh0, self.part(self.array(), u64::from(self.field(36)?), "sdma ctx")?));
                v.push((SdmaTh1, self.part(u64::from(self.field(48)?), u64::from(self.field(52)?), "sdma ctl")?));
            }
            Kind::Vcn => v.push(all(Vcn)?),
            Kind::Dmcub => {
                self.version(1)?;
                v.push((Dmcub, self.part(self.array(), u64::from(self.field(32)?), "dmcub inst_const")?));
            }
            Kind::Toc | Kind::Ta => {}
        }
        Ok(v)
    }

    /// RLC v2.x (amdgpu_rlc.c:518-555): v2.0 gives RLC_G and the register lists; minor >= 1 the save/restore
    /// lists, >= 2 IRAM/DRAM, == 3 RLC_P/RLC_V, == 4 the tap delays (Linux's `==`: a v2.4 file has no RLC_P/V).
    /// Parts of size 0 are not registered (Linux's `if (size)`).
    fn rlc(&self, v: &mut Vec<(UcodeId, Range<usize>)>) -> Result<(), Why> {
        use UcodeId::*;
        let h = self.header;
        if h.header_major != 2 {
            return Err(Why::Version { kind: Kind::Rlc, major: h.header_major, minor: h.header_minor });
        }
        v.push((RlcG, self.part(self.array(), u64::from(h.ucode_size_bytes), "rlc g")?));
        // The register lists Linux copies at init (v2.0 @72/@76 and @80/@84): only their bounds matter here.
        self.part(u64::from(self.field(76)?), u64::from(self.field(72)?), "rlc reg_list_format")?;
        self.part(u64::from(self.field(84)?), u64::from(self.field(80)?), "rlc reg_list")?;
        let mut opt = |id: UcodeId, size_at: usize, off_at: usize, what| -> Result<(), Why> {
            let size = u64::from(self.field(size_at)?);
            let r = self.part(u64::from(self.field(off_at)?), size, what)?;
            if size != 0 {
                v.push((id, r));
            }
            Ok(())
        };
        let m = h.header_minor;
        if m >= 1 {
            opt(RlcRestoreListCntl, 116, 120, "rlc srl cntl")?;
            opt(RlcRestoreListGpmMem, 132, 136, "rlc srl gpm")?;
            opt(RlcRestoreListSrmMem, 148, 152, "rlc srl srm")?;
        }
        if m >= 2 {
            opt(RlcIram, 156, 160, "rlc iram")?;
            opt(RlcDram, 164, 168, "rlc dram")?;
        }
        if m == 3 {
            opt(RlcP, 180, 184, "rlc p")?;
            opt(RlcV, 196, 200, "rlc v")?;
        }
        if m == 4 {
            opt(GlobalTapDelays, 204, 208, "rlc global tap")?;
            opt(Se0TapDelays, 212, 216, "rlc se0 tap")?;
            opt(Se1TapDelays, 220, 224, "rlc se1 tap")?;
            opt(Se2TapDelays, 228, 232, "rlc se2 tap")?;
            opt(Se3TapDelays, 236, 240, "rlc se3 tap")?;
        }
        Ok(())
    }

    /// TA v2.0 (`ta_firmware_header_v2_0`, amdgpu_psp.c:3370-3398): the packed trusted applications. Their
    /// descriptors (16 bytes each from byte 36) must end before the microcode array and inside the file; the
    /// parts are relative to the array. Loading them (LOAD_TA) comes after GX-7a; until then tests only (C1: the
    /// cfg goes when LOAD_TA calls it).
    #[cfg(test)]
    pub fn ta_bins(&self) -> Result<Vec<TaBin>, Why> {
        if self.kind != Kind::Ta {
            return Ok(Vec::new());
        }
        self.version(2)?;
        let count = self.field(32)?;
        let end = 36u64 + u64::from(count) * 16;
        if end > self.array() || end > self.bytes.len() as u64 {
            return Err(Why::TaCount(count));
        }
        (0..count as usize)
            .map(|k| {
                let at = 36 + k * 16;
                let w = |o| le32(&self.bytes, at + o).unwrap_or(0);
                let off = self.array() + u64::from(w(8));
                Ok(TaBin { ta_type: w(0), version: w(4), range: self.part(off, u64::from(w(12)), "ta bin")? })
            })
            .collect()
    }
}

/// One packed trusted application of the TA file (`psp_fw_bin_desc`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaBin {
    /// `enum ta_fw_type` (1 ASD, 4 HDCP, 5 DTM, ...).
    pub ta_type: u32,
    pub version: u32,
    pub range: Range<usize>,
}

/// The files of one GPU, each kind at most once.
#[derive(Clone, Debug, Default)]
pub struct FwSet {
    pub files: Vec<FwFile>,
}

/// One microcode to load: which file and which part of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ucode {
    pub id: UcodeId,
    pub file: usize,
    pub range: Range<usize>,
}

impl Ucode {
    pub fn size(&self) -> u32 {
        (self.range.end - self.range.start) as u32
    }
}

/// The rules that decide what the PSP loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListOpts {
    /// `psp->autoload_supported` (amdgpu_psp.c:220, true for 13.0.4): MEC jump tables are skipped and RLC
    /// autoload starts after RLC_G.
    pub autoload: bool,
    /// Load MES's KIQ pipe. GX-7k: on for GC 11.0.x, as Linux 6.6 and 7.1 force it (amdgpu_discovery.c:2110-2116
    /// sets enable_mes_kiq unconditionally; the 27.10. capture loads gc_11_0_1_mes1.bin); the KIQ pipe is where
    /// MES's start maps the scheduler queue.
    pub mes_kiq: bool,
    /// BitsOS (GX-D9): the DMCUB the firmware started keeps running under the scanout; it is not reloaded.
    pub keep_display_firmware: bool,
    /// Load VCN's microcode (GX-7d: not yet; the video item turns it on).
    pub vcn: bool,
}

impl ListOpts {
    /// PSP 13.0.4 on the 780M as BitsOS runs it (GX-7d: no DMCUB, no VCN).
    pub const PSP13_0_4: ListOpts = ListOpts { autoload: true, mes_kiq: true, keep_display_firmware: true, vcn: false };
}

impl FwSet {
    pub fn get(&self, kind: Kind) -> Option<&FwFile> {
        self.files.iter().find(|f| f.kind == kind)
    }

    /// GX-7k: the scheduler's and the KIQ's microcode starts (`None` for a file not in the set).
    pub fn mes_uc_starts(&self) -> [Option<u64>; 2] {
        [self.get(Kind::MesSched).and_then(FwFile::mes_uc_start), self.get(Kind::MesKiq).and_then(FwFile::mes_uc_start)]
    }

    pub fn bytes(&self, u: &Ucode) -> &[u8] {
        self.files.get(u.file).and_then(|f| f.bytes.get(u.range.clone())).unwrap_or(&[])
    }

    /// RS64 CP mode: the PFP header is exactly 2.0 (gfx_v11_0.c:505).
    pub fn rs64(&self) -> bool {
        self.get(Kind::Pfp).is_some_and(|f| f.header.header_major == 2 && f.header.header_minor == 0)
    }

    /// The LOAD_IP_FW list in Linux's order (microcode ID order, amdgpu_psp.c:2573), without the parts Linux
    /// skips (size 0; MEC jump tables with autoload) and the ones `opts` leaves out.
    pub fn load_list(&self, opts: &ListOpts) -> Result<Vec<Ucode>, Why> {
        let rs64 = self.rs64();
        let mut out: Vec<Ucode> = Vec::new();
        for (i, f) in self.files.iter().enumerate() {
            if f.kind == Kind::MesKiq && !opts.mes_kiq {
                continue;
            }
            for (id, range) in f.entries(rs64)? {
                if out.iter().any(|u| u.id == id) {
                    return Err(Why::Duplicate(id));
                }
                let skip = range.is_empty()
                    || (opts.autoload && id == UcodeId::CpMec1Jt)
                    || (opts.keep_display_firmware && id == UcodeId::Dmcub)
                    || (!opts.vcn && id == UcodeId::Vcn);
                if !skip {
                    out.push(Ucode { id, file: i, range });
                }
            }
        }
        out.sort_by_key(|u| u.id);
        Ok(out)
    }
}

/// CRC-32 (IEEE 802.3, reflected, as zlib's), bitwise: only for the informational header check.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}
