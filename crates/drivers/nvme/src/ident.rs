//! Identify Controller, Identify Namespace, the active namespace list and the SMART / Health log, parsed from
//! the controller's 4 KiB pages (layouts: linux-6.6/include/linux/nvme.h `struct nvme_id_ctrl`,
//! `struct nvme_id_ns`, `struct nvme_smart_log`). Device data: every field is read with bounds checks and
//! every count is checked against the page, never trusted.

use alloc::string::String;
use alloc::vec::Vec;

fn u16_at(b: &[u8], i: usize) -> u16 {
    b.get(i..i + 2).map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    b.get(i..i + 4).map_or(0, |x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    b.get(i..i + 8).map_or(0, |x| u64::from_le_bytes(x.try_into().unwrap_or([0; 8])))
}
/// A 128-bit counter, saturated to u64 (data units: 2^64 x 512 KB is beyond any disk).
fn u128_sat(b: &[u8], i: usize) -> u64 {
    if u64_at(b, i + 8) != 0 { u64::MAX } else { u64_at(b, i) }
}

/// ASCII field, trailing spaces/NULs cut, anything not printable shown as '?'.
pub fn text(b: &[u8]) -> String {
    let s: String = b.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else if c == 0 { ' ' } else { '?' }).collect();
    String::from(s.trim_end())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CtrlId {
    pub vid: u16,
    pub ssvid: u16,
    pub serial: String,
    pub model: String,
    pub firmware: String,
    /// Maximum data transfer size as a power of two of the minimum page size (0 = no limit).
    pub mdts: u8,
    pub cntlid: u16,
    pub ver: u32,
    /// RTD3 entry latency in µs (shutdown time hint).
    pub rtd3e: u32,
    pub oacs: u16,
    /// Number of namespaces (the highest valid NSID).
    pub nn: u32,
    pub oncs: u16,
    /// Volatile write cache present (bit 0).
    pub vwc: u8,
    pub subnqn: String,
}

pub fn parse_ctrl(b: &[u8]) -> CtrlId {
    let field = |a: usize, n: usize| b.get(a..a + n).map(text).unwrap_or_default();
    CtrlId {
        vid: u16_at(b, 0),
        ssvid: u16_at(b, 2),
        serial: field(4, 20),
        model: field(24, 40),
        firmware: field(64, 8),
        mdts: b.get(77).copied().unwrap_or(0),
        cntlid: u16_at(b, 78),
        ver: u32_at(b, 80),
        rtd3e: u32_at(b, 88),
        oacs: u16_at(b, 256),
        nn: u32_at(b, 516),
        oncs: u16_at(b, 520),
        vwc: b.get(525).copied().unwrap_or(0),
        subnqn: field(768, 256),
    }
}

impl CtrlId {
    pub fn volatile_cache(&self) -> bool {
        self.vwc & 1 != 0
    }
    /// Largest transfer in bytes (None = no limit) for a minimum page size of 2^`mps_shift`.
    pub fn max_transfer(&self, mps_shift: u32) -> Option<u64> {
        (self.mdts != 0).then(|| 1u64.checked_shl(self.mdts as u32 + mps_shift).unwrap_or(u64::MAX))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LbaFormat {
    /// Metadata bytes per block.
    pub ms: u16,
    /// Block size as a power of two.
    pub ds: u8,
    pub rp: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NsId {
    pub nsze: u64,
    pub ncap: u64,
    pub nuse: u64,
    pub nlbaf: u8,
    pub flbas: u8,
    pub formats: Vec<LbaFormat>,
    pub nguid: [u8; 16],
    pub eui64: [u8; 8],
}

pub fn parse_ns(b: &[u8]) -> NsId {
    let nlbaf = b.get(25).copied().unwrap_or(0);
    // NLBAF is 0-based; at most 64 formats fit the page (bytes 128..384).
    let n = (nlbaf as usize + 1).min(64);
    let formats = (0..n)
        .map(|i| {
            let o = 128 + 4 * i;
            LbaFormat { ms: u16_at(b, o), ds: b.get(o + 2).copied().unwrap_or(0), rp: b.get(o + 3).copied().unwrap_or(0) & 3 }
        })
        .collect();
    let mut nguid = [0u8; 16];
    let mut eui64 = [0u8; 8];
    if let Some(x) = b.get(104..120) {
        nguid.copy_from_slice(x);
    }
    if let Some(x) = b.get(120..128) {
        eui64.copy_from_slice(x);
    }
    NsId { nsze: u64_at(b, 0), ncap: u64_at(b, 8), nuse: u64_at(b, 16), nlbaf, flbas: b.get(26).copied().unwrap_or(0), formats, nguid, eui64 }
}

impl NsId {
    /// The format in use: FLBAS bits 3:0, with bits 6:5 as the upper bits when more than 16 formats exist
    /// (linux-6.6/include/linux/nvme.h `nvme_lbaf_index`).
    pub fn format_index(&self) -> usize {
        let lo = (self.flbas & 0xF) as usize;
        if self.nlbaf > 16 { lo | (((self.flbas >> 5) & 3) as usize) << 4 } else { lo }
    }
    pub fn format(&self) -> Option<LbaFormat> {
        self.formats.get(self.format_index()).copied()
    }
    /// Metadata interleaved with the data (FLBAS bit 4).
    pub fn extended_lba(&self) -> bool {
        self.flbas & 0x10 != 0
    }
    /// Why the block layer cannot use this namespace (None = usable): blocks outside 512 B..4 KiB (Linux sets
    /// such a disk's capacity to 0, core.c `nvme_update_disk_info`), metadata (this driver passes none) or an
    /// empty namespace.
    pub fn unusable(&self) -> Option<String> {
        let Some(f) = self.format() else { return Some(alloc::format!("LBA format {} not described", self.format_index())) };
        if !(9..=12).contains(&f.ds) {
            return Some(alloc::format!("block size 2^{} not supported", f.ds));
        }
        if f.ms != 0 {
            return Some(alloc::format!("{} metadata bytes per block not supported", f.ms));
        }
        if self.nsze == 0 {
            return Some(String::from("size 0"));
        }
        None
    }
    pub fn block_size(&self) -> u32 {
        self.format().map_or(512, |f| 1u32 << f.ds.clamp(9, 12))
    }
}

/// The active namespace list (CNS 02h): NSIDs in increasing order up to the first 0. Values are device data:
/// only increasing NSIDs above `after` are kept.
pub fn parse_ns_list(b: &[u8], after: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut last = after;
    for i in 0..b.len() / 4 {
        let id = u32_at(b, 4 * i);
        if id == 0 {
            break;
        }
        if id > last && id != 0xFFFF_FFFF {
            out.push(id);
            last = id;
        }
    }
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Smart {
    pub critical_warning: u8,
    /// Composite temperature in Kelvin.
    pub temperature_k: u16,
    pub available_spare: u8,
    pub spare_threshold: u8,
    pub percent_used: u8,
    /// In units of 1000 x 512 bytes.
    pub data_units_read: u64,
    pub data_units_written: u64,
    pub host_reads: u64,
    pub host_writes: u64,
    pub power_cycles: u64,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
    pub error_log_entries: u64,
}

pub fn parse_smart(b: &[u8]) -> Smart {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    Smart {
        critical_warning: at(0),
        temperature_k: u16_at(b, 1),
        available_spare: at(3),
        spare_threshold: at(4),
        percent_used: at(5),
        data_units_read: u128_sat(b, 32),
        data_units_written: u128_sat(b, 48),
        host_reads: u128_sat(b, 64),
        host_writes: u128_sat(b, 80),
        power_cycles: u128_sat(b, 112),
        power_on_hours: u128_sat(b, 128),
        unsafe_shutdowns: u128_sat(b, 144),
        media_errors: u128_sat(b, 160),
        error_log_entries: u128_sat(b, 176),
    }
}
