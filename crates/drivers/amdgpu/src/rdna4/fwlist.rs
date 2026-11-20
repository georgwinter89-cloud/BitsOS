//! GP-GPU2: the IP firmware the card's secure OS gets through LOAD_IP_FW, in Linux's order, cut from the stick's
//! files by their documented headers (the licence forbids looking further). The secure processor's own two files
//! are `pspfiles`'.
//!
//! GC 12's list (gfx_v12_0.c:589-665: RS64 PFP and ME with one stack each, MEC with two; RLC v2.x; IMU), MES from one
//! file for both pipes (`enable_uni_mes`, amdgpu_discovery.c:2998-3007, amdgpu_mes.c:780-850), SDMA 7 as one RS64
//! image (amdgpu_ucode.c:901-905), VCN 5 (amdgpu_vcn.c:1145-1156) and the display's DMCUB
//! (amdgpu_dm_dmub.c:578-584), sorted by Linux 7.3.1's `AMDGPU_UCODE_ID` (amdgpu_ucode.h, the walk of
//! `psp_load_non_psp_fw` amdgpu_psp.c:3515-3593), MEC jump tables skipped under autoload (:3490-3494).
//!
//! The SMU's firmware goes first, always: `psp_load_non_psp_fw` calls `psp_load_smu_fw` ahead of the walk when
//! `autoload_supported && !pmfw_centralized_cstate_management` (:3521-3526), 14.0.3 has the first (:184) and is not
//! in the list of the second (:117-133), and the only skip is a resume from runtime power-off with BACO
//! (:3446-3448). The trace of Linux's cold start agrees: the frame after the TOC's has the longest silence before
//! the next one (28.5 ms, init.txt lines 778-779), the largest image. The autoload command goes right after RLC_G
//! (:3581-3588), so VCN and DMCUB, whose ids are higher, follow it.
//!
//! Own enum, not `gpucore::ucode::UcodeId`: GC 12 has other stacks than GC 11 and an RS64 SDMA, and the 780M's list
//! stays exactly as it was.

use crate::gpucore::ucode::{self, Header, Why};
use alloc::vec::Vec;
use core::ops::Range;

/// The microcodes of this card, discriminant = Linux 7.3.1's `enum AMDGPU_UCODE_ID` value (so sorting is Linux's
/// load order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Id {
    SdmaRs64 = 11,
    Rs64Pfp = 15,
    Rs64Me = 16,
    Rs64Mec = 17,
    Rs64PfpP0Stack = 18,
    Rs64MeP0Stack = 20,
    Rs64MecP0Stack = 22,
    Rs64MecP1Stack = 23,
    CpMes = 30,
    CpMesData = 31,
    CpMes1 = 32,
    CpMes1Data = 33,
    ImuI = 34,
    ImuD = 35,
    RlcRestoreListCntl = 41,
    RlcRestoreListGpmMem = 42,
    RlcRestoreListSrmMem = 43,
    RlcIram = 44,
    RlcDram = 45,
    RlcG = 50,
    Smc = 52,
    Vcn = 57,
    Dmcub = 63,
}

impl Id {
    /// `psp_get_fw_type` (amdgpu_psp.c:3105-3260) into `enum psp_gfx_fw_type` (psp_gfx_if.h:209-290).
    pub const fn fw_type(self) -> u32 {
        match self {
            Id::SdmaRs64 => 71,
            Id::Rs64Pfp => 87,
            Id::Rs64Me => 88,
            Id::Rs64Mec => 89,
            Id::Rs64PfpP0Stack => 90,
            Id::Rs64MeP0Stack => 92,
            Id::Rs64MecP0Stack => 94,
            Id::Rs64MecP1Stack => 95,
            Id::CpMes => 33,
            Id::CpMesData => 34,
            Id::CpMes1 => 81,
            Id::CpMes1Data => 82,
            Id::ImuI => 68,
            Id::ImuD => 69,
            Id::RlcRestoreListCntl => 22,
            Id::RlcRestoreListGpmMem => 20,
            Id::RlcRestoreListSrmMem => 21,
            Id::RlcIram => 26,
            Id::RlcDram => 48,
            Id::RlcG => 8,
            Id::Smc => 18,
            Id::Vcn => 13,
            Id::Dmcub => 51,
        }
    }
}

/// The stick's files the card needs, by role (`fw::wanted` names them).
pub const SOS: &str = "amdgpu/psp_14_0_3_sos.bin";
pub const TA: &str = "amdgpu/psp_14_0_3_ta.bin";
pub const FILES: [(&str, Role); 10] = [
    ("amdgpu/sdma_7_0_1.bin", Role::Sdma),
    ("amdgpu/gc_12_0_1_pfp.bin", Role::Pfp),
    ("amdgpu/gc_12_0_1_me.bin", Role::Me),
    ("amdgpu/gc_12_0_1_mec.bin", Role::Mec),
    ("amdgpu/gc_12_0_1_uni_mes.bin", Role::Mes),
    ("amdgpu/gc_12_0_1_imu.bin", Role::Imu),
    ("amdgpu/gc_12_0_1_rlc.bin", Role::Rlc),
    ("amdgpu/smu_14_0_3.bin", Role::Smc),
    ("amdgpu/vcn_5_0_0.bin", Role::Vcn),
    ("amdgpu/dcn_4_0_1_dmcub.bin", Role::Dmcub),
];

/// The file's name on this board: a kicker board's own name where it has one (`rdna4_core::fwset`'s rule).
pub fn name_for(plain: &'static str, device: u16, revision: u8) -> &'static str {
    let kicker = rdna4_core::fwset::is_kicker(device, revision);
    rdna4_core::fwset::FILES.iter().find(|f| f.file == plain).and_then(|f| f.kicker_file.filter(|_| kicker)).unwrap_or(plain)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Sdma,
    Pfp,
    Me,
    Mec,
    Mes,
    Imu,
    Rlc,
    Smc,
    Vcn,
    Dmcub,
}

/// One image to load: which file, which bytes of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub id: Id,
    pub file: usize,
    pub range: Range<usize>,
}

impl Image {
    pub fn size(&self) -> u32 {
        (self.range.end - self.range.start) as u32
    }
}

fn le32(b: &[u8], at: usize) -> Result<u32, Why> {
    let s = b.get(at..at + 4).ok_or(Why::HeaderSize { header: b.len() as u32, need: at as u64 + 4 })?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A header field: it must lie inside the header the file declares.
fn field(b: &[u8], h: &Header, at: usize) -> Result<u32, Why> {
    if at + 4 > h.header_size_bytes as usize {
        return Err(Why::HeaderSize { header: h.header_size_bytes, need: at as u64 + 4 });
    }
    le32(b, at)
}

fn version(h: &Header, major: u16, minor: Option<u16>, kind: ucode::Kind) -> Result<(), Why> {
    if h.header_major != major || minor.is_some_and(|m| m != h.header_minor) {
        return Err(Why::Version { kind, major: h.header_major, minor: h.header_minor });
    }
    Ok(())
}

/// The stick's file of a role (the plain name; `name_for` applies the kicker rule).
pub fn file_of(role: Role) -> Option<&'static str> {
    FILES.iter().find(|(_, r)| *r == role).map(|(name, _)| *name)
}

/// Where an RS64 engine starts, from its file's header: `ucode_start_addr_lo` @52 and `ucode_start_addr_hi` @56
/// of `gfx_firmware_header_v2_0` (amdgpu_ucode.h:189-198), which `gfx_v12_0_config_gfx_rs64` writes into the
/// engine's program counter start (gfx_v12_0.c:2142-2225). The PFP, ME and MEC files alike.
pub fn rs64_start(b: &[u8]) -> Result<(u32, u32), Why> {
    let h = ucode::header(b)?;
    version(&h, 2, Some(0), ucode::Kind::Pfp)?;
    Ok((field(b, &h, 52)?, field(b, &h, 56)?))
}

/// Wave 44: where both MES pipes start, from the `uni_mes` file's header: `mes_uc_start_addr_lo` @56 and
/// `mes_uc_start_addr_hi` @60 of `mes_firmware_header_v1_0` (amdgpu_ucode.h:201-213), which
/// `amdgpu_mes_init_microcode` keeps as `uc_start_addr` (amdgpu_mes.c:807-809) for `mes_v12_0_enable`.
pub fn mes_start(b: &[u8]) -> Result<(u32, u32), Why> {
    let h = ucode::header(b)?;
    version(&h, 1, None, ucode::Kind::MesSched)?;
    Ok((field(b, &h, rdna4_core::mes12::UC_START_LO_AT)?, field(b, &h, rdna4_core::mes12::UC_START_HI_AT)?))
}

/// Wave 44: a file's `ucode_version` from its common header (byte 16), which Linux keeps as the engine's firmware
/// version (`amdgpu_gfx_cp_init_microcode`, amdgpu_gfx.c:1500-1595).
pub fn ucode_version(b: &[u8]) -> Result<u32, Why> {
    Ok(ucode::header(b)?.ucode_version)
}

/// The parts one file gives (`amdgpu_ucode_init_single_fw`'s sizes and offsets, amdgpu_ucode.c:851-1122).
pub fn parts(role: Role, b: &[u8]) -> Result<Vec<(Id, Range<usize>)>, Why> {
    let h = ucode::header(b)?;
    let len = b.len();
    let array = u64::from(h.ucode_array_offset_bytes);
    let part = |off: u64, size: u64, what| ucode::part(len, off, size, what);
    let f = |at| field(b, &h, at);
    let mut v = Vec::new();
    match role {
        Role::Pfp | Role::Me | Role::Mec => {
            // gfx_firmware_header_v2_0: ucode_size @36, data_size @44, data_offset @48 (amdgpu_ucode.h:189-198).
            version(&h, 2, Some(0), ucode::Kind::Pfp)?;
            let code = part(array, u64::from(f(36)?), "rs64 ucode")?;
            let data = part(u64::from(f(48)?), u64::from(f(44)?), "rs64 stack")?;
            let (main, stacks): (Id, &[Id]) = match role {
                Role::Pfp => (Id::Rs64Pfp, &[Id::Rs64PfpP0Stack]),
                Role::Me => (Id::Rs64Me, &[Id::Rs64MeP0Stack]),
                _ => (Id::Rs64Mec, &[Id::Rs64MecP0Stack, Id::Rs64MecP1Stack]),
            };
            v.push((main, code));
            v.extend(stacks.iter().map(|&s| (s, data.clone())));
        }
        Role::Sdma => {
            // sdma_firmware_header_v3_0: ucode_size @40, the image at the array (amdgpu_ucode.h:371-376).
            version(&h, 3, Some(0), ucode::Kind::Sdma)?;
            v.push((Id::SdmaRs64, part(array, u64::from(f(40)?), "sdma rs64")?));
        }
        Role::Mes => {
            // mes_firmware_header_v1_0: ucode size @36 offset @40, data size @48 offset @52; one file, both pipes.
            version(&h, 1, None, ucode::Kind::MesSched)?;
            let code = part(u64::from(f(40)?), u64::from(f(36)?), "mes ucode")?;
            let data = part(u64::from(f(52)?), u64::from(f(48)?), "mes data")?;
            v.extend([(Id::CpMes, code.clone()), (Id::CpMesData, data.clone()), (Id::CpMes1, code), (Id::CpMes1Data, data)]);
        }
        Role::Imu => {
            // imu_firmware_header_v1_0: IRAM size @32, DRAM size @40, the DRAM part right after the IRAM part.
            version(&h, 1, None, ucode::Kind::Imu)?;
            let iram = u64::from(f(32)?);
            v.push((Id::ImuI, part(array, iram, "imu iram")?));
            v.push((Id::ImuD, part(array + iram, u64::from(f(40)?), "imu dram")?));
        }
        Role::Rlc => {
            version(&h, 2, None, ucode::Kind::Rlc)?;
            v.push((Id::RlcG, part(array, u64::from(h.ucode_size_bytes), "rlc g")?));
            // amdgpu_rlc.c:518-555: v2.1 the save/restore lists, v2.2 IRAM/DRAM; parts of size 0 are not loaded.
            let mut opt = |id, size_at, off_at, what| -> Result<(), Why> {
                let size = u64::from(f(size_at)?);
                let r = part(u64::from(f(off_at)?), size, what)?;
                if size != 0 {
                    v.push((id, r));
                }
                Ok(())
            };
            if h.header_minor >= 1 {
                opt(Id::RlcRestoreListCntl, 116, 120, "rlc srl cntl")?;
                opt(Id::RlcRestoreListGpmMem, 132, 136, "rlc srl gpm")?;
                opt(Id::RlcRestoreListSrmMem, 148, 152, "rlc srl srm")?;
            }
            if h.header_minor >= 2 {
                opt(Id::RlcIram, 156, 160, "rlc iram")?;
                opt(Id::RlcDram, 164, 168, "rlc dram")?;
            }
        }
        Role::Smc => {
            // smu_v14_0_init_microcode checks the size only (amdgpu_ucode_request): the common header's array.
            v.push((Id::Smc, part(array, u64::from(h.ucode_size_bytes), "smc")?));
        }
        Role::Vcn => {
            // The default arm of amdgpu_ucode_init_single_fw (amdgpu_ucode.c:1108-1111): the common header's array.
            v.push((Id::Vcn, part(array, u64::from(h.ucode_size_bytes), "vcn")?));
        }
        Role::Dmcub => {
            // dmcub_firmware_header_v1_0: inst_const_bytes @32 (amdgpu_ucode.h:425-429); only the instruction and
            // constant part goes through the PSP (amdgpu_ucode.c:1008-1011).
            version(&h, 1, Some(0), ucode::Kind::Dmcub)?;
            v.push((Id::Dmcub, part(array, u64::from(f(32)?), "dmcub inst const")?));
        }
    }
    Ok(v)
}

/// The LOAD_IP_FW list over `files` (in `FILES`' order, one byte vector each), in Linux's order: the SMU's firmware
/// first, then the walk by id. It holds RLC_G (the autoload goes right after it, `after_autoload`).
pub fn list(files: &[&[u8]]) -> Result<Vec<Image>, Why> {
    if files.len() != FILES.len() {
        return Err(Why::Range { what: "the firmware files", offset: files.len() as u64, size: FILES.len() as u64 });
    }
    let mut out: Vec<Image> = Vec::new();
    for (i, (role, b)) in FILES.iter().map(|(_, r)| *r).zip(files).enumerate() {
        for (id, range) in parts(role, b)? {
            if out.iter().any(|u| u.id == id) {
                return Err(Why::Range { what: "an image twice", offset: id as u64, size: 0 });
            }
            // `fw_load_skip_check` (amdgpu_psp.c:3474): an image of size 0 is not loaded.
            if !range.is_empty() {
                out.push(Image { id, file: i, range });
            }
        }
    }
    // The SMU's firmware ahead of the walk (psp_load_smu_fw before it), the walk itself by id.
    out.sort_by_key(|u| (u.id != Id::Smc, u.id));
    if out.first().map(|u| u.id) != Some(Id::Smc) {
        return Err(Why::Range { what: "the smu's firmware first", offset: 0, size: 0 });
    }
    if !out.iter().any(|u| u.id == Id::RlcG) {
        return Err(Why::Range { what: "rlc g", offset: 0, size: 0 });
    }
    Ok(out)
}

/// How many images of `list` go before the autoload command: up to RLC_G (amdgpu_psp.c:3581-3588).
pub fn before_autoload(list: &[Image]) -> usize {
    list.iter().position(|u| u.id == Id::RlcG).map_or(list.len(), |i| i + 1)
}
