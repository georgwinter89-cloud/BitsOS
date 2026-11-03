//! The IP discovery binary (GX-6): what IP blocks the GPU has, in which versions and instances, at which register
//! bases, plus the GC, harvest, VCN and MALL tables. The layout and the checks follow Linux
//! (linux-6.6/drivers/gpu/drm/amd/include/discovery.h, amdgpu/amdgpu_discovery.c:326-498 init, :1185-1347
//! reg_base_init); the parser is BitsOS's own: every offset and length is checked against the binary before it is
//! read, a blob that does not hold up is refused with a reason (`Why`), never trusted in part.
//!
//! Where Linux reads past what it checked (die count above the header's 16 slots, `binary_size` below its header,
//! table offsets past the end), BitsOS refuses. Where Linux deliberately does not check (the MALL table, `0 &&` at
//! amdgpu_discovery.c:465), BitsOS notes the problem and goes on, so a blob Linux takes is not refused here for it.

use crate::ip::IpVersion;
use alloc::vec::Vec;
use core::fmt;

use super::hwid;

pub const BINARY_SIGNATURE: u32 = 0x2821_1407;
pub const IP_TABLE_SIGNATURE: u32 = 0x5344_5049;
pub const GC_TABLE_ID: u32 = 0x4347;
pub const HARVEST_SIGNATURE: u32 = 0x5652_4148;
pub const VCN_TABLE_ID: u32 = 0x004E_4356;
pub const MALL_TABLE_ID: u32 = 0x4C4C_414D;
/// Where Linux reads the binary: VRAM size minus 64 KiB, 8 KiB long (linux-6.6 amdgpu_discovery.h:27-28).
pub const TMR_OFFSET: u64 = 64 << 10;
pub const TMR_SIZE: usize = 8 << 10;

/// binary_header: signature, version major/minor, checksum, size, then six table_info entries of 8 bytes.
const BINARY_HEADER: usize = 12 + 6 * 8;
/// The checksum covers the binary from the byte after its own field (offsetof(binary_checksum) + 2).
const CHECKSUM_FROM: usize = 10;
/// ip_discovery_header: the same 80 bytes in table versions 1-4 (v4 uses the last two bytes as flags).
const IP_HEADER: usize = 80;
/// The header's die_info array (the format's size, not a BitsOS limit).
const DIE_SLOTS: u16 = 16;
const IP_ENTRY: usize = 8;
/// gpu_info_header, vcn_info_header and mall_info_header: id, major, minor, size.
const INFO_HEADER: usize = 12;
/// harvest_table: 8-byte header and its fixed list of 32 four-byte entries (Linux checksums all of it).
const HARVEST_TABLE: usize = 8 + 32 * 4;
const VCN_SLOTS: u32 = 4;

/// The tables of the binary header's table_list, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    Ip,
    Gc,
    Harvest,
    Vcn,
    Mall,
}

impl Table {
    pub const fn name(self) -> &'static str {
        match self {
            Table::Ip => "IP discovery",
            Table::Gc => "GC",
            Table::Harvest => "harvest",
            Table::Vcn => "VCN",
            Table::Mall => "MALL",
        }
    }
}

/// Why a discovery binary is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The buffer is shorter than the binary header.
    Short { len: usize },
    BinarySignature { found: u32 },
    /// `binary_size` is smaller than the header or larger than what was read.
    BinarySize { size: u16, len: usize },
    BinaryChecksum { want: u16, got: u16 },
    /// The binary names no IP discovery table (offset 0): nothing to build on.
    NoIpTable,
    /// A table's header or its declared size reaches past `binary_size`.
    TableOutside { table: Table, offset: u16, size: u32 },
    /// A table's declared size is smaller than its own header.
    TableTooSmall { table: Table, size: u32 },
    TableSignature { table: Table, found: u32 },
    TableChecksum { table: Table, want: u16, got: u16 },
    /// A table version the parser does not know.
    TableVersion { table: Table, major: u16 },
    /// Two tables' declared ranges overlap.
    Overlap { a: Table, b: Table },
    /// No die, or more than the header's 16 die slots.
    DieCount { dies: u16 },
    DieOutside { die: u16, offset: u16 },
    /// A die header names another die than its slot (Linux: "invalid die id").
    DieId { die: u16, found: u16 },
    /// More IP entries than the rest of the binary can hold.
    IpCount { die: u16, ips: u16 },
    /// An IP entry or its base addresses reach past `binary_size`.
    IpOutside { die: u16, index: u16, offset: usize },
    /// The VCN table lists more instances than its four slots.
    VcnInstances { count: u32 },
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Why::Short { len } => write!(f, "only {len} bytes, shorter than the binary header ({BINARY_HEADER})"),
            Why::BinarySignature { found } => {
                write!(f, "binary signature {found:#010x}, want {BINARY_SIGNATURE:#010x}")
            }
            Why::BinarySize { size, len } => {
                write!(f, "binary_size {size} outside {BINARY_HEADER}..={len} (header .. bytes read)")
            }
            Why::BinaryChecksum { want, got } => write!(f, "binary checksum {got:#06x}, header says {want:#06x}"),
            Why::NoIpTable => write!(f, "no IP discovery table (offset 0)"),
            Why::TableOutside { table, offset, size } => {
                write!(f, "{} table at {offset:#x} with {size} bytes reaches past the binary", table.name())
            }
            Why::TableTooSmall { table, size } => {
                write!(f, "{} table declares {size} bytes, less than its header", table.name())
            }
            Why::TableSignature { table, found } => write!(f, "{} table signature/id {found:#010x}", table.name()),
            Why::TableChecksum { table, want, got } => {
                write!(f, "{} table checksum {got:#06x}, header says {want:#06x}", table.name())
            }
            Why::TableVersion { table, major } => write!(f, "{} table version {major} unknown", table.name()),
            Why::Overlap { a, b } => write!(f, "{} and {} tables overlap", a.name(), b.name()),
            Why::DieCount { dies } => write!(f, "{dies} dies (want 1..={DIE_SLOTS})"),
            Why::DieOutside { die, offset } => write!(f, "die {die} header at {offset:#x} past the binary"),
            Why::DieId { die, found } => write!(f, "die slot {die} holds die id {found}"),
            Why::IpCount { die, ips } => write!(f, "die {die} claims {ips} IPs, more than the binary can hold"),
            Why::IpOutside { die, index, offset } => {
                write!(f, "die {die} IP {index} at {offset:#x} reaches past the binary")
            }
            Why::VcnInstances { count } => write!(f, "VCN table lists {count} instances (slots: {VCN_SLOTS})"),
        }
    }
}

/// Something odd that Linux accepts too: reported, not refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// The MALL table has a bad id, checksum, size or version; Linux does not check it either.
    MallUnchecked(Why),
    /// A die's IP list lies outside the IP table's own declared size (still inside the binary).
    DieBeyondIpTable { die: u16 },
    /// An IP Linux skips (instance >= 44 or hw_id >= 300); kept here with its number.
    LinuxSkips { die: u16, index: u16, hw_id: u16 },
    /// Table version below 4 with the 64-bit base flag set: Linux reads the flag in every version.
    Bases64BeforeV4,
    /// An unknown GC or VCN table version: its fields are not decoded (Linux returns -EINVAL for it).
    InfoVersion { table: Table, major: u16, minor: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpEntry {
    pub die: u16,
    pub hw_id: u16,
    pub instance: u8,
    /// `revision` with VCN's capability bits (7:6) taken out, as Linux does (amdgpu_discovery.c:1247-1253).
    pub version: IpVersion,
    /// The entry's eighth byte: v1 harvest (bits 3:0); v3/v4 sub-revision (3:0) and variant (7:4).
    pub byte7: u8,
    /// VCN only: revision bits 7:6 (0b10 encode off, 0b01 decode off).
    pub vcn_config: u8,
    /// Register segment bases in dwords. With 64-bit bases the low 32 bits with bits 31:30 cleared (Linux).
    pub bases: Vec<u32>,
}

impl IpEntry {
    pub fn name(&self) -> Option<&'static str> {
        hwid::name(self.hw_id)
    }
    pub fn variant(&self) -> u8 {
        self.byte7 >> 4
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Harvested {
    pub hw_id: u16,
    pub instance: u8,
}

/// The GC table: its version and the fields of that version, by Linux's field names (gc_info_v1_0..v2_1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcInfo {
    pub major: u16,
    pub minor: u16,
    pub fields: Vec<(&'static str, u32)>,
}

impl GcInfo {
    pub fn get(&self, name: &str) -> Option<u32> {
        self.fields.iter().find(|(n, _)| *n == name).map(|&(_, v)| v)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VcnInstance {
    pub instance: u32,
    /// Fuse bits: 0 AV1, 1 VP9, 2 HEVC, 3 H.264 decode disabled.
    pub fuses: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VcnInfo {
    pub major: u16,
    pub minor: u16,
    pub instances: Vec<VcnInstance>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MallInfo {
    pub major: u16,
    pub minor: u16,
    /// v1: mall_size_per_m, m_s_present, m_half_use, m_mall_config; v2: mall_size_per_umc.
    pub fields: Vec<(&'static str, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    pub binary_major: u16,
    pub binary_minor: u16,
    pub binary_size: u16,
    pub table_version: u16,
    pub dies: u16,
    pub bases_64: bool,
    pub ips: Vec<IpEntry>,
    pub gc: Option<GcInfo>,
    pub harvest: Option<Vec<Harvested>>,
    pub vcn: Option<VcnInfo>,
    pub mall: Option<MallInfo>,
    pub notes: Vec<Note>,
}

/// Little-endian reads that fail instead of reading past `end`.
struct Bytes<'a> {
    b: &'a [u8],
}

impl Bytes<'_> {
    fn u8(&self, o: usize) -> Option<u8> {
        self.b.get(o).copied()
    }
    fn u16(&self, o: usize) -> Option<u16> {
        let s = self.b.get(o..o.checked_add(2)?)?;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&self, o: usize) -> Option<u32> {
        let s = self.b.get(o..o.checked_add(4)?)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&self, o: usize) -> Option<u64> {
        Some(u64::from(self.u32(o)?) | u64::from(self.u32(o + 4)?) << 32)
    }
}

/// Linux's 16-bit byte sum.
pub fn checksum(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |s, &b| s.wrapping_add(u16::from(b)))
}

/// One table_list entry: offset and checksum (Linux ignores the size field and uses the table's own).
fn table_entry(b: &Bytes, t: Table) -> (u16, u16) {
    let at = 12 + t as usize * 8;
    // Inside the checked header.
    (b.u16(at).unwrap_or(0), b.u16(at + 2).unwrap_or(0))
}

/// Checks a table's place and checksum; gives back its byte range.
fn place(b: &Bytes, t: Table, offset: u16, size: u32, header: usize, want: u16) -> Result<(usize, usize), Why> {
    let start = usize::from(offset);
    let end = start.checked_add(size as usize).filter(|&e| e <= b.b.len());
    let Some(end) = end.filter(|_| start.checked_add(header).is_some_and(|h| h <= b.b.len())) else {
        return Err(Why::TableOutside { table: t, offset, size });
    };
    if (size as usize) < header {
        return Err(Why::TableTooSmall { table: t, size });
    }
    let got = checksum(&b.b[start..end]);
    if got != want {
        return Err(Why::TableChecksum { table: t, want, got });
    }
    Ok((start, end))
}

/// Parses a discovery binary as read from the reserved region (8 KiB) or from a file. Never panics; every
/// refusal names its reason.
pub fn parse(buf: &[u8]) -> Result<Discovery, Why> {
    if buf.len() < BINARY_HEADER {
        return Err(Why::Short { len: buf.len() });
    }
    let all = Bytes { b: buf };
    let sig = all.u32(0).unwrap_or(0);
    if sig != BINARY_SIGNATURE {
        return Err(Why::BinarySignature { found: sig });
    }
    let size = all.u16(10).unwrap_or(0);
    if usize::from(size) < BINARY_HEADER || usize::from(size) > buf.len() {
        return Err(Why::BinarySize { size, len: buf.len() });
    }
    // From here on nothing past binary_size is read.
    let b = Bytes { b: &buf[..usize::from(size)] };
    let want = all.u16(8).unwrap_or(0);
    let got = checksum(&b.b[CHECKSUM_FROM..]);
    if got != want {
        return Err(Why::BinaryChecksum { want, got });
    }
    let mut notes = Vec::new();
    let mut ranges: Vec<(Table, usize, usize)> = Vec::new();

    let (ip_off, ip_sum) = table_entry(&b, Table::Ip);
    if ip_off == 0 {
        return Err(Why::NoIpTable);
    }
    let ip_at = usize::from(ip_off);
    let ihdr_sig = b.u32(ip_at).ok_or(Why::TableOutside { table: Table::Ip, offset: ip_off, size: 0 })?;
    if ihdr_sig != IP_TABLE_SIGNATURE {
        return Err(Why::TableSignature { table: Table::Ip, found: ihdr_sig });
    }
    let ip_size = u32::from(b.u16(ip_at + 6).unwrap_or(0));
    let (ip_start, ip_end) = place(&b, Table::Ip, ip_off, ip_size, IP_HEADER, ip_sum)?;
    ranges.push((Table::Ip, ip_start, ip_end));
    let table_version = b.u16(ip_at + 4).unwrap_or(0);
    if !(1..=4).contains(&table_version) {
        return Err(Why::TableVersion { table: Table::Ip, major: table_version });
    }
    let dies = b.u16(ip_at + 12).unwrap_or(0);
    if dies == 0 || dies > DIE_SLOTS {
        return Err(Why::DieCount { dies });
    }
    // Linux tests the flag byte whatever the version (amdgpu_discovery.c:1300): so does BitsOS, with a note.
    let bases_64 = b.u8(ip_at + 78).unwrap_or(0) & 1 != 0;
    if bases_64 && table_version < 4 {
        notes.push(Note::Bases64BeforeV4);
    }

    let gc = info_table(&b, Table::Gc, GC_TABLE_ID, &mut ranges)?.map(|(start, major, minor)| {
        let names = gc_fields(major, minor);
        if names.is_empty() {
            notes.push(Note::InfoVersion { table: Table::Gc, major, minor });
        }
        GcInfo { major, minor, fields: read_fields(&b, start + INFO_HEADER, names) }
    });
    let harvest = harvest_table(&b, &mut ranges)?;
    let vcn = match info_table(&b, Table::Vcn, VCN_TABLE_ID, &mut ranges)? {
        None => None,
        Some((start, major, minor)) if major == 1 => Some(vcn_table(&b, start, major, minor)?),
        Some((_, major, minor)) => {
            notes.push(Note::InfoVersion { table: Table::Vcn, major, minor });
            Some(VcnInfo { major, minor, instances: Vec::new() })
        }
    };
    let mall = mall_table(&b, &mut notes);
    for (i, &(ta, sa, ea)) in ranges.iter().enumerate() {
        for &(tb, sb, eb) in &ranges[i + 1..] {
            if sa < eb && sb < ea {
                return Err(Why::Overlap { a: ta, b: tb });
            }
        }
    }

    let mut ips = Vec::new();
    for die in 0..dies {
        let die_off = b.u16(ip_at + 14 + usize::from(die) * 4 + 2).unwrap_or(0);
        let d = usize::from(die_off);
        let (Some(id), Some(num_ips)) = (b.u16(d), b.u16(d + 2)) else {
            return Err(Why::DieOutside { die, offset: die_off });
        };
        if id != die {
            return Err(Why::DieId { die, found: id });
        }
        let mut at = d + 4;
        if usize::from(num_ips) * IP_ENTRY > b.b.len() - at.min(b.b.len()) {
            return Err(Why::IpCount { die, ips: num_ips });
        }
        let start = at;
        for index in 0..num_ips {
            let outside = Why::IpOutside { die, index, offset: at };
            let (Some(hw_id), Some(instance), Some(n), Some(major), Some(minor), Some(rev), Some(byte7)) =
                (b.u16(at), b.u8(at + 2), b.u8(at + 3), b.u8(at + 4), b.u8(at + 5), b.u8(at + 6), b.u8(at + 7))
            else {
                return Err(outside);
            };
            let width = if bases_64 { 8 } else { 4 };
            let list = at + IP_ENTRY;
            let next = list + usize::from(n) * width;
            if next > b.b.len() {
                return Err(outside);
            }
            let bases = (0..usize::from(n))
                .map(|k| match bases_64 {
                    true => (b.u64(list + k * 8).unwrap_or(0) as u32) & 0x3FFF_FFFF,
                    false => b.u32(list + k * 4).unwrap_or(0),
                })
                .collect();
            let (revision, vcn_config) = if hw_id == hwid::VCN { (rev & !0xC0, rev & 0xC0) } else { (rev, 0) };
            if instance >= hwid::LINUX_MAX_INSTANCE || hw_id >= hwid::LINUX_MAX {
                notes.push(Note::LinuxSkips { die, index, hw_id });
            }
            ips.push(IpEntry {
                die,
                hw_id,
                instance,
                version: IpVersion { major, minor, rev: revision },
                byte7,
                vcn_config,
                bases,
            });
            at = next;
        }
        if start < ip_start || at > ip_end {
            notes.push(Note::DieBeyondIpTable { die });
        }
    }
    Ok(Discovery {
        binary_major: b.u16(4).unwrap_or(0),
        binary_minor: b.u16(6).unwrap_or(0),
        binary_size: size,
        table_version,
        dies,
        bases_64,
        ips,
        gc,
        harvest,
        vcn,
        mall,
        notes,
    })
}

/// A table with the gpu_info/vcn_info header (id, major, minor, u32 size): checked; (start, major, minor).
fn info_table(
    b: &Bytes,
    t: Table,
    id: u32,
    ranges: &mut Vec<(Table, usize, usize)>,
) -> Result<Option<(usize, u16, u16)>, Why> {
    let (off, sum) = table_entry(b, t);
    if off == 0 {
        return Ok(None);
    }
    let at = usize::from(off);
    let (Some(found), Some(major), Some(minor), Some(size)) = (b.u32(at), b.u16(at + 4), b.u16(at + 6), b.u32(at + 8))
    else {
        return Err(Why::TableOutside { table: t, offset: off, size: INFO_HEADER as u32 });
    };
    if found != id {
        return Err(Why::TableSignature { table: t, found });
    }
    let (s, e) = place(b, t, off, size, INFO_HEADER, sum)?;
    ranges.push((t, s, e));
    Ok(Some((s, major, minor)))
}

fn read_fields(b: &Bytes, from: usize, names: &[&'static str]) -> Vec<(&'static str, u32)> {
    // `place` checked the declared size; a version whose fields run past it yields only the ones inside.
    names.iter().enumerate().map_while(|(i, &n)| Some((n, b.u32(from + i * 4)?))).collect()
}

const GC_V1_0: &[&str] = &[
    "gc_num_se",
    "gc_num_wgp0_per_sa",
    "gc_num_wgp1_per_sa",
    "gc_num_rb_per_se",
    "gc_num_gl2c",
    "gc_num_gprs",
    "gc_num_max_gs_thds",
    "gc_gs_table_depth",
    "gc_gsprim_buff_depth",
    "gc_parameter_cache_depth",
    "gc_double_offchip_lds_buffer",
    "gc_wave_size",
    "gc_max_waves_per_simd",
    "gc_max_scratch_slots_per_cu",
    "gc_lds_size",
    "gc_num_sc_per_se",
    "gc_num_sa_per_se",
    "gc_num_packer_per_sc",
    "gc_num_gl2a",
    // v1.1
    "gc_num_tcp_per_sa",
    "gc_num_sdp_interface",
    "gc_num_tcps",
    // v1.2
    "gc_num_tcp_per_wpg",
    "gc_tcp_l1_size",
    "gc_num_sqc_per_wgp",
    "gc_l1_instruction_cache_size_per_sqc",
    "gc_l1_data_cache_size_per_sqc",
    "gc_gl1c_per_sa",
    "gc_gl1c_size_per_instance",
    "gc_gl2c_per_gpu",
];
const GC_V2_0: &[&str] = &[
    "gc_num_se",
    "gc_num_cu_per_sh",
    "gc_num_sh_per_se",
    "gc_num_rb_per_se",
    "gc_num_tccs",
    "gc_num_gprs",
    "gc_num_max_gs_thds",
    "gc_gs_table_depth",
    "gc_gsprim_buff_depth",
    "gc_parameter_cache_depth",
    "gc_double_offchip_lds_buffer",
    "gc_wave_size",
    "gc_max_waves_per_simd",
    "gc_max_scratch_slots_per_cu",
    "gc_lds_size",
    "gc_num_sc_per_se",
    "gc_num_packer_per_sc",
    // v2.1
    "gc_num_tcp_per_sh",
    "gc_tcp_size_per_cu",
    "gc_num_sdp_interface",
    "gc_num_cu_per_sqc",
    "gc_instruction_cache_size_per_sqc",
    "gc_scalar_data_cache_size_per_sqc",
    "gc_tcc_size",
];

/// The field names of a GC table version (empty for an unknown one).
fn gc_fields(major: u16, minor: u16) -> &'static [&'static str] {
    match (major, minor) {
        (1, 0) => &GC_V1_0[..19],
        (1, 1) => &GC_V1_0[..22],
        (1, _) => GC_V1_0,
        (2, 0) => &GC_V2_0[..17],
        (2, _) => GC_V2_0,
        _ => &[],
    }
}

fn harvest_table(b: &Bytes, ranges: &mut Vec<(Table, usize, usize)>) -> Result<Option<Vec<Harvested>>, Why> {
    let (off, sum) = table_entry(b, Table::Harvest);
    if off == 0 {
        return Ok(None);
    }
    let at = usize::from(off);
    let found = b.u32(at).ok_or(Why::TableOutside { table: Table::Harvest, offset: off, size: 8 })?;
    if found != HARVEST_SIGNATURE {
        return Err(Why::TableSignature { table: Table::Harvest, found });
    }
    let (s, e) = place(b, Table::Harvest, off, HARVEST_TABLE as u32, HARVEST_TABLE, sum)?;
    ranges.push((Table::Harvest, s, e));
    let list = (0..32)
        .map(|i| (b.u16(at + 8 + i * 4).unwrap_or(0), b.u8(at + 10 + i * 4).unwrap_or(0)))
        .take_while(|&(hw_id, _)| hw_id != 0)
        .map(|(hw_id, instance)| Harvested { hw_id, instance })
        .collect();
    Ok(Some(list))
}

fn vcn_table(b: &Bytes, start: usize, major: u16, minor: u16) -> Result<VcnInfo, Why> {
    // v1: num_of_instances, four 16-byte instance infos, four reserved dwords.
    let count = b.u32(start + INFO_HEADER).unwrap_or(0);
    if count > VCN_SLOTS {
        return Err(Why::VcnInstances { count });
    }
    let instances = (0..count as usize)
        .map_while(|i| {
            let at = start + INFO_HEADER + 4 + i * 16;
            Some(VcnInstance { instance: b.u32(at)?, fuses: b.u32(at + 4)? })
        })
        .collect();
    Ok(VcnInfo { major, minor, instances })
}

fn mall_table(b: &Bytes, notes: &mut Vec<Note>) -> Option<MallInfo> {
    let (off, sum) = table_entry(b, Table::Mall);
    if off == 0 {
        return None;
    }
    let at = usize::from(off);
    let unchecked = |w: Why, notes: &mut Vec<Note>| {
        notes.push(Note::MallUnchecked(w));
        None
    };
    let (Some(found), Some(major), Some(minor), Some(size)) = (b.u32(at), b.u16(at + 4), b.u16(at + 6), b.u32(at + 8))
    else {
        return unchecked(Why::TableOutside { table: Table::Mall, offset: off, size: INFO_HEADER as u32 }, notes);
    };
    if found != MALL_TABLE_ID {
        return unchecked(Why::TableSignature { table: Table::Mall, found }, notes);
    }
    if let Err(w) = place(b, Table::Mall, off, size, INFO_HEADER, sum) {
        notes.push(Note::MallUnchecked(w));
    }
    let names: &[&'static str] = match major {
        1 => &["mall_size_per_m", "m_s_present", "m_half_use", "m_mall_config"],
        2 => &["mall_size_per_umc"],
        _ => return unchecked(Why::TableVersion { table: Table::Mall, major }, notes),
    };
    Some(MallInfo { major, minor, fields: read_fields(b, at + INFO_HEADER, names) })
}

impl Discovery {
    pub fn find(&self, hw_id: u16, instance: u8) -> Option<&IpEntry> {
        self.ips.iter().find(|e| e.hw_id == hw_id && e.instance == instance)
    }

    /// The version of an IP's instance 0 (Linux's `ip_versions[HWIP][0]`).
    pub fn version(&self, hw_id: u16) -> Option<IpVersion> {
        self.find(hw_id, 0).map(|e| e.version)
    }

    /// Harvested (fused off): listed in the harvest table, or, before GC 10.2 (not 9.4.3), variant 1 in its own
    /// entry (linux-6.6 amdgpu_discovery.c:1349-1370 harvest_ip).
    pub fn harvested(&self, e: &IpEntry) -> bool {
        let per_ip = self.version(hwid::GC).is_some_and(|v| {
            v < IpVersion { major: 10, minor: 2, rev: 0 } && v != IpVersion { major: 9, minor: 4, rev: 3 }
        });
        if per_ip {
            return e.variant() == 1;
        }
        self.harvest.as_ref().is_some_and(|h| h.iter().any(|x| x.hw_id == e.hw_id && x.instance == e.instance))
    }

    /// A register's dword address: the IP instance's segment base plus the header's offset (SOC15 addressing).
    pub fn reg(&self, hw_id: u16, instance: u8, base_idx: usize, dword: u32) -> Option<u32> {
        self.find(hw_id, instance)?.bases.get(base_idx)?.checked_add(dword)
    }
}
