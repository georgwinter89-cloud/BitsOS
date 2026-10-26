//! The firmware file of the RTL8852B read as Linux rtw89 fw.c reads it (JS
//! js/fw-parse.mjs): the multi-firmware container (MFW), the header of the
//! chosen entry with its sections, the elements after the container and the
//! firmware feature table. Pure functions, no device access. Every offset and
//! length from the file is checked before it is used: a short or hostile file
//! is an error (`FwFile`), never a panic. The file is only parsed as data
//! (spec/wlan.md §2.2: its licence forbids reverse engineering).

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use crate::bus::field;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::FeatCond;
use crate::tables::fw_feat::FEAT_TBL;
use crate::tables::regs::*;

/// struct rtw89_mfw_hdr / rtw89_mfw_info
const MFW_HDR_LEN: usize = 16;
const MFW_INFO_LEN: usize = 16;
/// struct rtw89_fw_hdr (w0..w7) / rtw89_fw_hdr_section
const FW_HDR_LEN: usize = 32;
const FW_SECTION_LEN: usize = 16;
/// sizeof(struct rtw89_fw_element_hdr), the union's fixed part included
const ELM_HDR_LEN: u64 = 32;
/// offset of the element's union u
const ELM_UNION: u64 = 24;
/// struct rtw89_fw_mss_pool_hdr
const MSS_POOL_HDR_LEN: usize = 32;
/// fw.c:41 mss_signature
const MSS_SIGNATURE: &[u8; 8] = b"MSSKPOOL";
/// union rtw89_fw_section_mssc_content.key_sign_len (fw.h:722)
const KEY_SIGN_LEN_AT: usize = 60;

/// enum rtw89_fw_element_id (fw.h), the ids __fw_element_handlers covers (ARRAY_SIZE 30,
/// up to TX_COMP); the names for logs
pub const ELM_NAMES: [&str; 30] = [
    "BBMCU0",
    "BBMCU1",
    "BB_REG",
    "BB_GAIN",
    "RADIO_A",
    "RADIO_B",
    "RADIO_C",
    "RADIO_D",
    "RF_NCTL",
    "TXPWR_BYRATE",
    "TXPWR_LMT_2GHZ",
    "TXPWR_LMT_5GHZ",
    "TXPWR_LMT_6GHZ",
    "TXPWR_LMT_RU_2GHZ",
    "TXPWR_LMT_RU_5GHZ",
    "TXPWR_LMT_RU_6GHZ",
    "TX_SHAPE_LMT",
    "TX_SHAPE_LMT_RU",
    "TXPWR_TRK",
    "RFKLOG_FMT",
    "REGD",
    "TXPWR_DA_LMT_2GHZ",
    "TXPWR_DA_LMT_5GHZ",
    "TXPWR_DA_LMT_6GHZ",
    "TXPWR_DA_LMT_RU_2GHZ",
    "TXPWR_DA_LMT_RU_5GHZ",
    "TXPWR_DA_LMT_RU_6GHZ",
    "AFE_PWR_SEQ",
    "DIAG_MAC",
    "TX_COMP",
];

fn bad(what: &'static str, value: u64) -> Rtw89Error {
    Rtw89Error::FwFile {
        what,
        value: u32::try_from(value).unwrap_or(u32::MAX),
    }
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u32::from(u16::from_le_bytes([s[0], s[1]])))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn to_usize(v: u64, what: &'static str) -> Rtw89Result<usize> {
    usize::try_from(v).map_err(|_| bad(what, v))
}

/// Linux: RTW89_FW_VER_CODE (core.h:6047)
pub const fn ver_code(maj: u8, min: u8, sub: u8, idx: u8) -> u32 {
    ((maj as u32) << 24) | ((min as u32) << 16) | ((sub as u32) << 8) | idx as u32
}

/// struct rtw89_mfw_info: one entry of the container
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MfwEntry {
    pub cv: u8,
    pub typ: u8,
    pub mp: u8,
    pub shift: u32,
    pub size: u32,
}

/// Where rtw89_mfw_recognize found the suit in the file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MfwFound {
    pub offset: usize,
    pub size: usize,
    /// `None` for a legacy file (no container: the whole file is the normal firmware)
    pub entry: Option<MfwEntry>,
}

// Linux: rtw89_mfw_recognize (fw.c:603) with rtw89_mfw_validate_hdr: the entry of one
// type for the chip cut: the highest cut not above `cv`, never an mp one (the entries
// are not in version order)
pub fn mfw_recognize(file: &[u8], typ: u32, cv: u32) -> Rtw89Result<MfwFound> {
    if file.len() < MFW_HDR_LEN || u32::from(file[0]) != RTW89_MFW_SIG {
        // legacy firmware (no container) holds the normal type only
        if typ != RTW89_FW_NORMAL {
            return Err(Rtw89Error::NoFirmware { typ, cv });
        }
        return Ok(MfwFound {
            offset: 0,
            size: file.len(),
            entry: None,
        });
    }
    let fw_nr = usize::from(file[1]);
    if fw_nr == 0 {
        return Err(bad("mfw header has no fw entry", 0));
    }
    if MFW_HDR_LEN + fw_nr * MFW_INFO_LEN > file.len() {
        return Err(bad("mfw header out of address", fw_nr as u64));
    }
    let mut found: Option<MfwEntry> = None;
    for i in 0..fw_nr {
        let ent = mfw_entry(file, i)?;
        if u32::from(ent.typ) != typ {
            continue;
        }
        if u32::from(ent.cv) <= cv && ent.mp == 0 && found.is_none_or(|f| f.cv < ent.cv) {
            found = Some(ent);
        }
    }
    let Some(ent) = found else {
        return Err(Rtw89Error::NoFirmware { typ, cv });
    };
    if u64::from(ent.shift) + u64::from(ent.size) > file.len() as u64 {
        return Err(bad("fw_suit out of address", u64::from(typ)));
    }
    Ok(MfwFound {
        offset: ent.shift as usize,
        size: ent.size as usize,
        entry: Some(ent),
    })
}

fn mfw_entry(file: &[u8], i: usize) -> Rtw89Result<MfwEntry> {
    let o = MFW_HDR_LEN + i * MFW_INFO_LEN;
    let short = || bad("mfw header out of address", i as u64);
    let b = file.get(o..o + MFW_INFO_LEN).ok_or_else(short)?;
    Ok(MfwEntry {
        cv: b[0],
        typ: b[1],
        mp: b[2],
        shift: le32(b, 4).ok_or_else(short)?,
        size: le32(b, 8).ok_or_else(short)?,
    })
}

/// The version fields of the firmware header (struct rtw89_fw_suit's)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FwVersion {
    pub major: u8,
    pub minor: u8,
    pub sub: u8,
    pub idx: u8,
    /// RTW89_FW_VER_CODE
    pub code: u32,
    pub commit_id: u32,
    pub build_year: u32,
    pub build_mon: u8,
    pub build_date: u8,
    pub build_hour: u8,
    pub build_min: u8,
    pub cmd_ver: u8,
}

// Linux: rtw89_fw_update_ver (fw.c:727) with rtw89_fw_update_ver_v0: header version 0
// only (v1 headers belong to BE chips)
fn update_ver(data: &[u8]) -> Rtw89Result<(u32, FwVersion)> {
    let w =
        |i: usize| le32(data, i * 4).ok_or_else(|| bad("fw header too short", data.len() as u64));
    let hdr_ver = field(w(3)?, FW_HDR_W3_HDR_VER);
    if hdr_ver != 0 {
        return Err(bad(
            "firmware header version not handled",
            u64::from(hdr_ver),
        ));
    }
    let (w1, w4) = (w(1)?, w(4)?);
    let b = |v: u32, m: u32| field(v, m) as u8;
    let mut v = FwVersion {
        major: b(w1, FW_HDR_W1_MAJOR_VERSION),
        minor: b(w1, FW_HDR_W1_MINOR_VERSION),
        sub: b(w1, FW_HDR_W1_SUBVERSION),
        idx: b(w1, FW_HDR_W1_SUBINDEX),
        commit_id: field(w(2)?, FW_HDR_W2_COMMITID),
        build_year: field(w(5)?, FW_HDR_W5_YEAR),
        build_mon: b(w4, FW_HDR_W4_MONTH),
        build_date: b(w4, FW_HDR_W4_DATE),
        build_hour: b(w4, FW_HDR_W4_HOUR),
        build_min: b(w4, FW_HDR_W4_MIN),
        cmd_ver: b(w(7)?, FW_HDR_W7_CMD_VERSERION),
        code: 0,
    };
    v.code = ver_code(v.major, v.minor, v.sub, v.idx);
    Ok((hdr_ver, v))
}

/// struct rtw89_fw_hdr_section_info: one section of the suit
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FwSection {
    pub typ: u32,
    /// FWSECTION_HDR_W1_SEC_SIZE, plus FWDL_SECTION_CHKSUM_LEN when `checksum`
    pub len: u32,
    pub checksum: bool,
    pub redl: bool,
    pub dl_addr: u32,
    /// where the section's bytes start, counted from the suit's start
    pub offset: usize,
    /// secure boot would cut the security section (960 on 8852B); 0 = not
    pub len_override: u32,
    /// a second formatted security section (secure boot only)
    pub ignore: bool,
    /// the MSS key copied into the last piece (secure boot only): suit offset and length
    pub key_addr: Option<usize>,
    pub key_len: u32,
    pub mssc: u32,
    /// signature bytes after the section (never downloaded)
    pub mssc_len: u32,
}

/// struct rtw89_fw_bin_info
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FwBinInfo {
    pub section_num: u32,
    pub dynamic_hdr_en: bool,
    pub idmem_share_mode: u32,
    /// AX: always FWDL_SECTION_PER_PKT_LEN (2020)
    pub part_size: u32,
    pub hdr_len: u32,
    pub dynamic_hdr_len: u32,
    pub dsp_checksum: bool,
    pub secure_section_exist: bool,
    pub sections: Vec<FwSection>,
}

impl FwBinInfo {
    /// The header bytes that go into the header H2C (`hdr_len - dynamic_hdr_len`)
    pub fn base_hdr_len(&self) -> u32 {
        self.hdr_len.saturating_sub(self.dynamic_hdr_len)
    }
}

// Linux: __parse_formatted_mssc (fw.c:282) without secure boot: only the length to skip
fn formatted_mssc_len(
    content: &[u8],
    sec: &mut FwSection,
    info: &mut FwBinInfo,
) -> Rtw89Result<u64> {
    let pool = content.get(sec.len as usize..).unwrap_or(&[]);
    if pool.len() < MSS_POOL_HDR_LEN || &pool[..8] != MSS_SIGNATURE {
        return Err(bad("wrong MSS signature", sec.offset as u64));
    }
    let (defen, rmpfmt, mssdev_max) = (pool[16], pool[20], u64::from(pool[21]));
    let h = |at| le16(pool, at).map_or(0, u64::from);
    let (keypair_num, msscust_max, msskey_num_max) = (h(22), h(24), h(26));
    if u32::from(rmpfmt) != MSS_POOL_RMP_TBL_BITMASK {
        return Err(bad(
            "MSS key pool remap table format unsupported",
            u64::from(rmpfmt),
        ));
    }
    let mut rmp_tbl_size = (msskey_num_max * msscust_max * mssdev_max) >> 3;
    if defen != 0 {
        rmp_tbl_size += u64::from(FWDL_MSS_POOL_DEFKEYSETS_SIZE);
    }
    let key_raw_offset = le32(pool, 12).map_or(0, u64::from);
    if rmp_tbl_size + MSS_POOL_HDR_LEN as u64 != key_raw_offset {
        return Err(bad("MSS key pool format error", key_raw_offset));
    }
    let mut key_sign_len = le16(content, KEY_SIGN_LEN_AT).ok_or_else(|| {
        bad(
            "security section shorter than its key_sign_len",
            content.len() as u64,
        )
    })? as u64
        >> 2;
    if key_sign_len == 0 {
        key_sign_len = 512;
    }
    if info.dsp_checksum {
        key_sign_len += u64::from(FWDL_SECURITY_CHKSUM_LEN);
    }
    // out: with secure boot off no key is selected; a second formatted section is ignored
    if info.secure_section_exist {
        sec.ignore = true;
    }
    info.secure_section_exist = true;
    Ok(MSS_POOL_HDR_LEN as u64 + rmp_tbl_size + keypair_num * key_sign_len)
}

// Linux: __parse_security_section (fw.c:404), secure boot off
fn security_section(content: &[u8], sec: &mut FwSection, info: &mut FwBinInfo) -> Rtw89Result<u64> {
    let mssc = u64::from(sec.mssc);
    let len = if sec.mssc & FORMATTED_MSSC_MASK == FORMATTED_MSSC {
        formatted_mssc_len(content, sec, info)?
    } else {
        let mut l = mssc * u64::from(FWDL_SECURITY_SIGLEN);
        if info.dsp_checksum {
            l += mssc * u64::from(FWDL_SECURITY_CHKSUM_LEN);
        }
        // secure boot would pick the signature at the efuse's MSS index here
        info.secure_section_exist = true;
        l
    };
    // __check_secure_blacklist returns at once without secure boot
    Ok(len)
}

// Linux: rtw89_fw_hdr_parser_v0 (fw.c:140); 8852B is AX: part size FWDL_SECTION_PER_PKT_LEN
fn hdr_parser_v0(data: &[u8]) -> Rtw89Result<FwBinInfo> {
    let w =
        |i: usize| le32(data, i * 4).ok_or_else(|| bad("fw header too short", data.len() as u64));
    let (w3, w6, w7) = (w(3)?, w(6)?, w(7)?);
    let mut info = FwBinInfo {
        section_num: field(w6, FW_HDR_W6_SEC_NUM),
        dynamic_hdr_en: field(w7, FW_HDR_W7_DYN_HDR) != 0,
        idmem_share_mode: field(w7, FW_HDR_W7_IDMEM_SHARE_MODE),
        part_size: FWDL_SECTION_PER_PKT_LEN,
        ..FwBinInfo::default()
    };
    // struct_size(fw_hdr, sections, n)
    let base = FW_HDR_LEN + info.section_num as usize * FW_SECTION_LEN;
    if base > data.len() {
        return Err(bad(
            "fw header shorter than its sections",
            u64::from(info.section_num),
        ));
    }
    if info.dynamic_hdr_en {
        info.hdr_len = field(w3, FW_HDR_W3_LEN);
        let dyn_ok = (info.hdr_len as usize)
            .checked_sub(base)
            .filter(|&l| le32(data, base) == Some(l as u32));
        let Some(l) = dyn_ok else {
            return Err(bad(
                "invalid fw dynamic header len",
                u64::from(info.hdr_len),
            ));
        };
        info.dynamic_hdr_len = l as u32;
    } else {
        info.hdr_len = base as u32;
        info.dynamic_hdr_len = 0;
    }
    let mut bin = u64::from(info.hdr_len);
    for i in 0..info.section_num as usize {
        let o = FW_HDR_LEN + i * FW_SECTION_LEN;
        let sw = |k: usize| le32(data, o + k * 4).unwrap_or(0);
        let (w0, w1, w2) = (sw(0), sw(1), sw(2));
        let mut sec = FwSection {
            typ: field(w1, FWSECTION_HDR_W1_SECTIONTYPE),
            len: field(w1, FWSECTION_HDR_W1_SEC_SIZE),
            checksum: field(w1, FWSECTION_HDR_W1_CHECKSUM) != 0,
            redl: field(w1, FWSECTION_HDR_W1_REDL) != 0,
            dl_addr: w0 & 0x1fff_ffff,
            offset: to_usize(bin, "fw bin size")?,
            ..FwSection::default()
        };
        if sec.checksum {
            sec.len += FWDL_SECTION_CHKSUM_LEN;
        }
        let mut mssc_len = 0;
        if sec.typ == FWDL_SECURITY_SECTION_TYPE {
            sec.mssc = field(w2, FWSECTION_HDR_W2_MSSC);
            if bin + u64::from(sec.len) > data.len() as u64 {
                return Err(bad("fw bin size", bin + u64::from(sec.len)));
            }
            let content = &data[sec.offset..];
            mssc_len = security_section(content, &mut sec, &mut info)?;
            // 8852B with secure boot would cut the section to 960 bytes (len_override): it is off
        }
        sec.mssc_len = u32::try_from(mssc_len).map_err(|_| bad("fw bin size", mssc_len))?;
        bin += u64::from(sec.len) + mssc_len;
        info.sections.push(sec);
    }
    if bin != data.len() as u64 {
        return Err(bad("fw bin size", bin));
    }
    Ok(info)
}

/// One element after the container (struct rtw89_fw_element_hdr); each consumer parses its
/// payload ([`Firmware::payload`])
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwElement {
    pub id: u32,
    pub name: &'static str,
    pub size: u32,
    pub ver: [u8; 4],
    pub aid: u16,
    /// the element header's place in the file
    pub offset: usize,
    /// the union u: file offset + 24, length 8 + size (cut at the file's end)
    pub payload: Range<usize>,
}

// Linux: rtw89_fw_recognize_elements (fw.c:1560): the list only
fn recognize_elements(file: &[u8]) -> Rtw89Result<Vec<FwElement>> {
    if file.len() < MFW_HDR_LEN || u32::from(file[0]) != RTW89_MFW_SIG {
        return Err(bad("not mfw format", 0));
    }
    // rtw89_mfw_get_size: the end of the last entry
    let fw_nr = usize::from(file[1]);
    if fw_nr == 0 {
        return Err(bad("mfw header has no fw entry", 0));
    }
    let last = mfw_entry(file, fw_nr - 1)?;
    let align = u64::from(RTW89_FW_ELEMENT_ALIGN);
    let up = |x: u64| x.div_ceil(align) * align;
    let mut offset = up(u64::from(last.shift) + u64::from(last.size));
    if offset == 0 {
        return Err(bad("no element offset", 0));
    }
    let len = file.len() as u64;
    let mut list = Vec::new();
    while offset + ELM_HDR_LEN < len {
        let o = offset as usize;
        let id = le32(file, o).unwrap_or(0);
        let size = le32(file, o + 4).unwrap_or(0);
        if offset + u64::from(size) >= len {
            break; // "firmware element size exceeds"
        }
        if (id as usize) < ELM_NAMES.len() {
            let end = (offset + ELM_HDR_LEN + u64::from(size)).min(len) as usize;
            let mut ver = [0u8; 4];
            ver.copy_from_slice(&file[o + 8..o + 12]);
            list.push(FwElement {
                id,
                name: ELM_NAMES[id as usize],
                size,
                ver,
                aid: le16(file, o + 12).unwrap_or(0) as u16,
                offset: o,
                payload: o + ELM_UNION as usize..end,
            });
        }
        offset = up(offset + ELM_HDR_LEN + u64::from(size));
    }
    Ok(list)
}

// Linux: rtw89_fw_iterate_feature_cfg (fw.c:956) over fw_feat_tbl's RTL8852B rows
// (fw.c:871-883): the RTW89_FW_FEATURE_* bits this version has; 8852B has no disable rows
pub fn features_for(code: u32) -> BTreeSet<u32> {
    FEAT_TBL
        .iter()
        .filter(|r| match r.cond {
            FeatCond::Ge => code >= r.code,
            FeatCond::Le => code <= r.code,
            FeatCond::Lt => code < r.code,
        })
        .map(|r| r.feature)
        .collect()
}

/// struct rtw89_fw_suit as rtw89_fw_recognize leaves it, plus the parsed header
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwSuit {
    /// RTW89_FW_NORMAL_CE (5) or RTW89_FW_NORMAL (1)
    pub typ: u32,
    /// where the suit's bytes are in the file
    pub offset: usize,
    pub size: usize,
    pub entry: Option<MfwEntry>,
    pub hdr_ver: u32,
    pub ver: FwVersion,
    pub info: FwBinInfo,
}

/// The firmware file as rtw89_fw_recognize made of it: the chosen suit, the elements and
/// the features. Owns the file: element payloads and the suit's bytes are slices of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firmware {
    file: Vec<u8>,
    pub suit: FwSuit,
    pub elements: Vec<FwElement>,
    /// RTW89_FW_FEATURE_* bits (tables::fw_feat)
    pub feat: BTreeSet<u32>,
    /// needed elements the file lacks (only noted: 8852B's list carries the transition bit)
    pub missing_elements: Vec<&'static str>,
}

impl Firmware {
    // Linux: rtw89_fw_recognize (fw.c:1056) + rtw89_fw_hdr_parser (fw.c:539) +
    // rtw89_fw_recognize_elements (fw.c:1560). `cv` picks the entry; secure boot is refused
    // (not ported: the MSS key selection and the 960-byte security section).
    pub fn parse(file: Vec<u8>, cv: u32, secure_boot: bool) -> Rtw89Result<Firmware> {
        if secure_boot {
            return Err(Rtw89Error::FwSecureBoot);
        }
        // 8852B: try_ce_fw, fw_b_aid 0: NORMAL_CE first, NORMAL as fallback
        let (typ, found) = match mfw_recognize(&file, RTW89_FW_NORMAL_CE, cv) {
            Ok(f) => (RTW89_FW_NORMAL_CE, f),
            Err(_) => (RTW89_FW_NORMAL, mfw_recognize(&file, RTW89_FW_NORMAL, cv)?),
        };
        let data = &file[found.offset..found.offset + found.size];
        let (hdr_ver, ver) = update_ver(data)?;
        // rtw89_fw_validate_ver_required: the 8852BE has no variant, nothing to check
        let info = hdr_parser_v0(data)?;
        let elements = recognize_elements(&file)?;
        let have = elements.iter().fold(0u64, |m, e| m | (1u64 << e.id));
        let needed = u64::from(RTW89_AX_GEN_DEF_NEEDED_FW_ELEMENTS_NO_6GHZ);
        let missing_elements = ELM_NAMES
            .iter()
            .enumerate()
            .filter(|(id, _)| needed & (1 << id) != 0 && have & (1 << id) == 0)
            .map(|(_, n)| *n)
            .collect();
        Ok(Firmware {
            suit: FwSuit {
                typ,
                offset: found.offset,
                size: found.size,
                entry: found.entry,
                hdr_ver,
                ver,
                info,
            },
            feat: features_for(ver.code),
            elements,
            missing_elements,
            file,
        })
    }

    pub fn file(&self) -> &[u8] {
        &self.file
    }

    /// The suit's bytes (struct rtw89_fw_suit `data`, `size`)
    pub fn suit_data(&self) -> &[u8] {
        self.file
            .get(self.suit.offset..self.suit.offset.saturating_add(self.suit.size))
            .unwrap_or(&[])
    }

    /// An element's union u
    pub fn payload(&self, e: &FwElement) -> &[u8] {
        self.file.get(e.payload.clone()).unwrap_or(&[])
    }

    /// The first element with this id (RTW89_FW_ELEMENT_ID_*)
    pub fn element(&self, id: u32) -> Option<&FwElement> {
        self.elements.iter().find(|e| e.id == id)
    }

    /// Linux: RTW89_CHK_FW_FEATURE (core.h:6125) by the RTW89_FW_FEATURE_* bit
    pub fn has_feat(&self, bit: u32) -> bool {
        self.feat.contains(&bit)
    }

    /// The same by the name after RTW89_FW_FEATURE_ (JS `hasFeat(d, name)`)
    pub fn has_feat_name(&self, name: &str) -> bool {
        FEAT_TBL
            .iter()
            .any(|r| r.name == name && self.feat.contains(&r.feature))
    }

    /// The feature names this firmware has, in fw_feat_tbl's order
    pub fn feat_names(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for r in FEAT_TBL {
            if self.feat.contains(&r.feature) && !out.contains(&r.name) {
                out.push(r.name);
            }
        }
        out
    }

    // Linux: rtw89_fw_update_ver (fw.c:754), the line it prints
    pub fn version_text(&self) -> String {
        let v = &self.suit.ver;
        format!(
            "Firmware version {}.{}.{}.{} ({:08x}), cmd version {}, type {}",
            v.major, v.minor, v.sub, v.idx, v.commit_id, v.cmd_ver, self.suit.typ
        )
    }
}
