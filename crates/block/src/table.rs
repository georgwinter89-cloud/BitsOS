//! The partition-table look at registration (R8a): does this disk hold a Windows partition or an EFI System
//! Partition, and where do its partitions start (the boot-stick match compares their first bytes)?
//!
//! Stricter than Linux on purpose (a doubt refuses writes, it never allows them):
//! - MBR (linux-6.6/block/partitions/msdos.c:610-650): signature 55AAh; a boot indicator other than 0/80h
//!   means "a file system's boot sector, no table" (msdos.c:623-639). Types 07h (NTFS/exFAT), 17h (hidden
//!   NTFS), 27h (Windows RE), 42h (dynamic disk), 86h/87h (NT volume sets) = Windows, EFh = EFI, EEh = GPT
//!   follows, 05h/0Fh/85h = extended: the EBR chain is walked (at most `EBR_HOPS` links, as msdos.c:148's
//!   `loopct > 100`; a bound against a hostile disk, not a count of things).
//! - GPT (linux-6.6/block/partitions/efi.c:335-433 `is_gpt_valid`, :581-640 `find_valid_gpt`): header
//!   "EFI PART", header size 92..=block size, header CRC, my_lba, entry size a multiple of 128, entry-array CRC;
//!   the backup header (alternate_lba, else the last block) when the primary is bad. Unlike Linux the GPT is
//!   looked for even without a protective MBR (an "EFI PART" at LBA 1 is enough), and a GPT that is announced
//!   but unreadable makes the table **unknown** (writes refused) instead of "no partitions".
//! - The whole disk as one Windows file system (NTFS, BitLocker, ReFS boot sector at LBA 0) counts as Windows.
//!   Every partition's first block is checked the same way.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Links of an MBR extended-partition chain followed at most (Linux: 100). Bound against a hostile disk.
pub const EBR_HOPS: usize = 100;
/// Largest GPT entry array read (count x size). Bound against a hostile header; a real one is 16 KiB.
pub const GPT_ENTRIES_MAX_BYTES: u64 = 4 << 20;
/// Bytes of a partition's first block kept for the boot-stick match (BootInfo `Stick::block0`).
pub const FIRST_BYTES: usize = 96;

/// A GPT partition type GUID in its on-disk byte order (the first three fields little-endian).
pub const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

pub const GUID_EFI_SYSTEM: [u8; 16] = guid(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
pub const GUID_MS_BASIC_DATA: [u8; 16] = guid(0xEBD0A0A2, 0xB9E5, 0x4433, [0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7]);
pub const GUID_MS_RESERVED: [u8; 16] = guid(0xE3C9E316, 0x0B5C, 0x4DB8, [0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02, 0x15, 0xAE]);
pub const GUID_MS_RECOVERY: [u8; 16] = guid(0xDE94BBA4, 0x06D1, 0x4D40, [0xA1, 0x6A, 0xBF, 0xD5, 0x01, 0x79, 0xD6, 0xAC]);
pub const GUID_MS_LDM_META: [u8; 16] = guid(0x5808C8AA, 0x7E8F, 0x42E0, [0x85, 0xD2, 0xE1, 0xE9, 0x04, 0x34, 0xCF, 0xB3]);
pub const GUID_MS_LDM_DATA: [u8; 16] = guid(0xAF9B60A0, 0x1431, 0x4F62, [0xBC, 0x68, 0x33, 0x11, 0x71, 0x4A, 0x69, 0xAD]);
pub const GUID_MS_STORAGE_SPACES: [u8; 16] = guid(0xE75CAF8F, 0xF680, 0x4CEE, [0xAF, 0xA3, 0xB0, 0x01, 0xE5, 0x6E, 0xFC, 0x2D]);
pub const GUID_LINUX_DATA: [u8; 16] = guid(0x0FC63DAF, 0x8483, 0x4772, [0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);

const WINDOWS_GUIDS: [([u8; 16], &str); 6] = [
    (GUID_MS_BASIC_DATA, "Microsoft basic data"),
    (GUID_MS_RESERVED, "Microsoft reserved"),
    (GUID_MS_RECOVERY, "Windows recovery"),
    (GUID_MS_LDM_META, "LDM metadata"),
    (GUID_MS_LDM_DATA, "LDM data"),
    (GUID_MS_STORAGE_SPACES, "Storage Spaces"),
];

/// MBR types of Windows partitions.
pub fn mbr_windows(t: u8) -> Option<&'static str> {
    match t {
        0x07 => Some("NTFS/exFAT (07h)"),
        0x17 => Some("hidden NTFS (17h)"),
        0x27 => Some("Windows RE (27h)"),
        0x42 => Some("dynamic disk (42h)"),
        0x86 | 0x87 => Some("NT volume set (86h/87h)"),
        _ => None,
    }
}

pub const MBR_EFI: u8 = 0xEF;
pub const MBR_GPT: u8 = 0xEE;

fn is_extended(t: u8) -> bool {
    matches!(t, 0x05 | 0x0F | 0x85)
}

/// A Windows file system's boot sector (NTFS, BitLocker, ReFS) by its OEM name at byte 3.
pub fn windows_fs(b: &[u8]) -> Option<&'static str> {
    let oem = b.get(3..11)?;
    if oem == b"NTFS    " {
        Some("NTFS boot sector")
    } else if oem == b"-FVE-FS-" {
        Some("BitLocker boot sector")
    } else if &oem[..4] == b"ReFS" {
        Some("ReFS boot sector")
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// No 55AAh signature and no GPT: blank or a file system without a table.
    None,
    /// 55AAh but a file system's boot sector (FAT superfloppy, the xtask image).
    BootSector,
    Mbr,
    Gpt,
}

/// What the look found.
#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub scheme: Option<Scheme>,
    /// Windows partitions (what and where), empty when none.
    pub windows: Vec<String>,
    /// EFI System Partitions.
    pub efi: Vec<String>,
    /// Set when the table could not be read or understood (writes refused).
    pub unknown: Option<String>,
    /// Partitions (first LBA, blocks, type text, the number Linux gives it: `sdb<n>`). The number is the GPT
    /// slot (efi.c:738), the MBR slot 1-4 (msdos.c:679, 686) or, for logical partitions, 5 on in chain order
    /// (msdos.c:662, 193): empty slots keep their number free.
    pub parts: Vec<(u64, u64, String, u32)>,
    /// First bytes of block 0 and of every partition's first block: (LBA, bytes).
    pub firsts: Vec<(u64, [u8; FIRST_BYTES])>,
    /// A partition's first block could not be read: `firsts` may lack the very block the boot-stick match looks
    /// for, and `windows_fs` could not look for a Windows file system there. A doubt, so writes are refused on
    /// every kind of disk (desk decision R8a, INT-A1e: `verdict` says `Unknown`).
    pub incomplete: bool,
    /// The first partition start that could not be read ("what at LBA n"), for the verdict's text.
    pub unreadable: Option<String>,
}

impl Scan {
    pub fn verdict(&self) -> Verdict {
        if let Some(w) = self.windows.first() {
            Verdict::Windows(w.clone())
        } else if let Some(e) = self.efi.first() {
            Verdict::Efi(e.clone())
        } else if let Some(u) = &self.unknown {
            Verdict::Unknown(u.clone())
        } else if self.incomplete {
            let at = self.unreadable.as_deref().unwrap_or("a partition start");
            Verdict::Unknown(format!("{at} unreadable: the scan is incomplete"))
        } else {
            Verdict::Clean
        }
    }

    pub fn scheme_str(&self) -> &'static str {
        match self.scheme {
            None => "unread",
            Some(Scheme::None) => "none",
            Some(Scheme::BootSector) => "boot-sector",
            Some(Scheme::Mbr) => "mbr",
            Some(Scheme::Gpt) => "gpt",
        }
    }

    /// Whether block 0 or a partition's first block starts with these bytes (the boot-stick match).
    pub fn holds_first(&self, b: &[u8; FIRST_BYTES]) -> bool {
        self.firsts.iter().any(|(_, f)| f == b)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    Windows(String),
    Efi(String),
    Unknown(String),
}

/// CRC-32 (IEEE 802.3, reflected; UEFI's and zlib's).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &x in data {
        crc ^= x as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3], b[i + 4], b[i + 5], b[i + 6], b[i + 7]])
}

/// `read(lba, blocks)` answers that many blocks or None. `bs` = block size, `total` = blocks of the disk.
pub fn scan(read: &mut dyn FnMut(u64, u64) -> Option<Vec<u8>>, bs: u32, total: u64) -> Scan {
    let mut s = Scan::default();
    let bs64 = bs as u64;
    if total == 0 {
        s.scheme = Some(Scheme::None);
        return s;
    }
    if bs < 512 {
        s.unknown = Some(format!("block size {bs}"));
        return s;
    }
    let Some(b0) = read(0, 1).filter(|b| b.len() >= 512) else {
        s.unknown = Some("block 0 unreadable".into());
        return s;
    };
    push_first(&mut s, 0, &b0);
    if let Some(w) = windows_fs(&b0) {
        s.windows.push(format!("{w} at LBA 0"));
    }
    let mut gpt_announced = false;
    if u16_at(&b0, 510) == 0xAA55 {
        let entries: Vec<(u8, u8, u32, u32)> =
            (0..4).map(|i| 0x1BE + 16 * i).map(|o| (b0[o], b0[o + 4], u32_at(&b0, o + 8), u32_at(&b0, o + 12))).collect();
        let empty = entries.iter().all(|e| e.1 == 0);
        let jump = matches!(b0[0], 0xEB | 0xE9);
        if entries.iter().any(|e| e.0 != 0 && e.0 != 0x80) || (empty && jump) {
            // msdos.c:623: not a partition table (a file system's boot sector; mkfs.fat leaves the table area
            // zero, so an empty table behind a jump instruction counts as a boot sector too).
            s.scheme = Some(Scheme::BootSector);
        } else {
            s.scheme = Some(Scheme::Mbr);
            let mut logical = 5u32;
            for (i, &(_, t, start, len)) in entries.iter().enumerate() {
                if t == 0 || len == 0 {
                    continue;
                }
                if t == MBR_GPT {
                    gpt_announced = true;
                    continue;
                }
                mbr_part(&mut s, read, bs64, total, format!("MBR {}", i + 1), t, start as u64, len as u64, (i + 1) as u32);
                if is_extended(t) {
                    walk_ebr(&mut s, read, bs64, total, start as u64, &mut logical);
                }
            }
        }
    }
    // GPT: announced by a protective MBR, or simply present at LBA 1.
    let b1 = if total > 1 { read(1, 1) } else { None };
    let sig_at_1 = b1.as_ref().is_some_and(|b| b.len() >= 8 && &b[..8] == b"EFI PART");
    if gpt_announced && b1.is_none() {
        s.unknown = Some("GPT header unreadable".into());
        return s;
    }
    if gpt_announced || sig_at_1 {
        let primary = b1.and_then(|b| gpt_header(&b, 1, bs, total));
        let hdr = match primary {
            Some(h) => Some(h),
            None => {
                let alt = total - 1;
                read(alt, 1).and_then(|b| gpt_header(&b, alt, bs, total))
            }
        };
        let Some(h) = hdr else {
            s.unknown = Some("GPT announced but no valid header".into());
            return s;
        };
        let bytes = h.count as u64 * h.size as u64;
        let blocks = bytes.div_ceil(bs64);
        let Some(arr) = read(h.entries_lba, blocks).filter(|a| a.len() as u64 >= bytes) else {
            s.unknown = Some("GPT entries unreadable".into());
            return s;
        };
        if crc32(&arr[..bytes as usize]) != h.entries_crc {
            s.unknown = Some("GPT entry array CRC".into());
            return s;
        }
        s.scheme = Some(Scheme::Gpt);
        for i in 0..h.count as usize {
            let e = &arr[i * h.size as usize..i * h.size as usize + 128];
            let mut t = [0u8; 16];
            t.copy_from_slice(&e[..16]);
            if t == [0; 16] {
                continue;
            }
            let (first, last) = (u64_at(e, 32), u64_at(e, 40));
            let what = format!("GPT {}", i + 1);
            if t == GUID_EFI_SYSTEM {
                s.efi.push(format!("{what}: EFI System at LBA {first}"));
            }
            if let Some((_, n)) = WINDOWS_GUIDS.iter().find(|(g, _)| *g == t) {
                s.windows.push(format!("{what}: {n} at LBA {first}"));
            }
            let blocks = last.saturating_sub(first).saturating_add(1);
            s.parts.push((first, blocks, format!("{what} {}", guid_text(&t)), (i + 1) as u32));
            check_first(&mut s, read, total, &what, first);
        }
    }
    if s.scheme.is_none() {
        s.scheme = Some(Scheme::None);
    }
    s
}

struct GptHeader {
    entries_lba: u64,
    count: u32,
    size: u32,
    entries_crc: u32,
}

/// `is_gpt_valid` (efi.c:335-433) on one header block read from `lba`.
fn gpt_header(b: &[u8], lba: u64, bs: u32, total: u64) -> Option<GptHeader> {
    if b.len() < 92 || &b[..8] != b"EFI PART" {
        return None;
    }
    let hsize = u32_at(b, 12);
    if hsize < 92 || hsize > bs || hsize as usize > b.len() {
        return None;
    }
    let mut h = b[..hsize as usize].to_vec();
    h[16..20].fill(0);
    if crc32(&h) != u32_at(b, 16) {
        return None;
    }
    if u64_at(b, 24) != lba {
        return None;
    }
    let (first_usable, last_usable) = (u64_at(b, 40), u64_at(b, 48));
    if first_usable > last_usable || last_usable >= total {
        return None;
    }
    let entries_lba = u64_at(b, 72);
    let (count, size) = (u32_at(b, 80), u32_at(b, 84));
    if size < 128 || size % 128 != 0 {
        return None;
    }
    let bytes = count as u64 * size as u64;
    if bytes > GPT_ENTRIES_MAX_BYTES || entries_lba >= total || entries_lba + bytes.div_ceil(bs as u64) > total {
        return None;
    }
    Some(GptHeader { entries_lba, count, size, entries_crc: u32_at(b, 88) })
}

fn guid_text(g: &[u8; 16]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32_at(g, 0),
        u16_at(g, 4),
        u16_at(g, 6),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

fn push_first(s: &mut Scan, lba: u64, b: &[u8]) {
    let mut f = [0u8; FIRST_BYTES];
    let n = b.len().min(FIRST_BYTES);
    f[..n].copy_from_slice(&b[..n]);
    s.firsts.push((lba, f));
}

/// A partition's first block: the boot-stick match and a Windows file system inside any partition type.
fn check_first(s: &mut Scan, read: &mut dyn FnMut(u64, u64) -> Option<Vec<u8>>, total: u64, what: &str, first: u64) {
    if first == 0 || first >= total || s.firsts.iter().any(|(l, _)| *l == first) {
        return;
    }
    // An unreadable partition start leaves the scan incomplete: a Windows file system could sit there unseen,
    // and the boot-stick match cannot answer "not the stick". Both are doubts, and a doubt refuses writes (R8a,
    // desk decision INT-A1e): `verdict` is `Unknown`.
    match read(first, 1) {
        Some(b) if b.len() >= FIRST_BYTES => {
            push_first(s, first, &b);
            if let Some(w) = windows_fs(&b) {
                s.windows.push(format!("{what}: {w} at LBA {first}"));
            }
        }
        _ => {
            s.incomplete = true;
            if s.unreadable.is_none() {
                s.unreadable = Some(format!("{what} at LBA {first}"));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn mbr_part(
    s: &mut Scan,
    read: &mut dyn FnMut(u64, u64) -> Option<Vec<u8>>,
    _bs: u64,
    total: u64,
    what: String,
    t: u8,
    start: u64,
    len: u64,
    number: u32,
) {
    if let Some(w) = mbr_windows(t) {
        s.windows.push(format!("{what}: {w} at LBA {start}"));
    }
    if t == MBR_EFI {
        s.efi.push(format!("{what}: EFI System (EFh) at LBA {start}"));
    }
    s.parts.push((start, len, format!("{what} type {t:02X}h"), number));
    if !is_extended(t) {
        check_first(s, read, total, &what, start);
    }
}

/// The logical partitions of an extended partition (msdos.c:130-220 `parse_extended`): each EBR holds one
/// logical partition (relative to this EBR) and a link to the next EBR (relative to the extended start).
/// `logical` is the next logical partition's number (Linux's `state->next`, shared by all extended partitions).
fn walk_ebr(s: &mut Scan, read: &mut dyn FnMut(u64, u64) -> Option<Vec<u8>>, bs: u64, total: u64, ext_start: u64, logical: &mut u32) {
    let mut at = ext_start;
    let mut seen: Vec<u64> = Vec::new();
    for hop in 0..=EBR_HOPS {
        if hop == EBR_HOPS {
            s.unknown = Some(format!("extended partition chain longer than {EBR_HOPS} links"));
            return;
        }
        if at >= total || seen.contains(&at) {
            if at >= total {
                s.unknown = Some(format!("EBR at LBA {at} past the end"));
            }
            return;
        }
        seen.push(at);
        let Some(b) = read(at, 1).filter(|b| b.len() >= 512) else {
            s.unknown = Some(format!("EBR at LBA {at} unreadable"));
            return;
        };
        if u16_at(&b, 510) != 0xAA55 {
            return;
        }
        let mut next = None;
        for k in 0..4 {
            let o = 0x1BE + 16 * k;
            let (t, rel, len) = (b[o + 4], u32_at(&b, o + 8) as u64, u32_at(&b, o + 12) as u64);
            if t == 0 || len == 0 {
                continue;
            }
            if is_extended(t) {
                next.get_or_insert(ext_start + rel);
            } else {
                mbr_part(s, read, bs, total, format!("logical at EBR {at}"), t, at + rel, len, *logical);
                *logical += 1;
            }
        }
        match next {
            Some(n) => at = n,
            None => return,
        }
    }
}
