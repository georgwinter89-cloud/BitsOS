//! 32-byte directory entries: the short (8.3) entry with its fields, LFN entries, and FAT time stamps.

use crate::name::{self, Short};
use alloc::vec::Vec;

pub const ENTRY: usize = 32;
pub const ATTR_RO: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME: u8 = 0x08;
pub const ATTR_DIR: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LFN: u8 = 0x0F;
pub const DELETED: u8 = 0xE5;

/// The short entry of a file or directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: Short,
    pub attr: u8,
    pub ntres: u8,
    pub ctime_tenth: u8,
    pub ctime: u16,
    pub cdate: u16,
    pub adate: u16,
    pub first: u32,
    pub wtime: u16,
    pub wdate: u16,
    pub size: u32,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

impl Entry {
    pub fn parse(e: &[u8]) -> Entry {
        let mut name = [0u8; 11];
        name.copy_from_slice(&e[..11]);
        Entry {
            name,
            attr: e[11],
            ntres: e[12],
            ctime_tenth: e[13],
            ctime: u16_at(e, 14),
            cdate: u16_at(e, 16),
            adate: u16_at(e, 18),
            first: ((u16_at(e, 20) as u32) << 16 | u16_at(e, 26) as u32) & 0x0FFF_FFFF,
            wtime: u16_at(e, 22),
            wdate: u16_at(e, 24),
            size: u32::from_le_bytes([e[28], e[29], e[30], e[31]]),
        }
    }
    pub fn bytes(&self) -> [u8; ENTRY] {
        let mut e = [0u8; ENTRY];
        e[..11].copy_from_slice(&self.name);
        e[11] = self.attr;
        e[12] = self.ntres;
        e[13] = self.ctime_tenth;
        e[14..16].copy_from_slice(&self.ctime.to_le_bytes());
        e[16..18].copy_from_slice(&self.cdate.to_le_bytes());
        e[18..20].copy_from_slice(&self.adate.to_le_bytes());
        e[20..22].copy_from_slice(&((self.first >> 16) as u16).to_le_bytes());
        e[22..24].copy_from_slice(&self.wtime.to_le_bytes());
        e[24..26].copy_from_slice(&self.wdate.to_le_bytes());
        e[26..28].copy_from_slice(&(self.first as u16).to_le_bytes());
        e[28..32].copy_from_slice(&self.size.to_le_bytes());
        e
    }
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }
    /// A new entry stamped with local time `t` (FAT date, time, tenths).
    pub fn new(name: Short, attr: u8, first: u32, t: Stamp) -> Entry {
        Entry { name, attr, ntres: 0, ctime_tenth: t.tenth, ctime: t.time, cdate: t.date, adate: t.date, first, wtime: t.time, wdate: t.date, size: 0 }
    }
}

/// The LFN entries of the long name `u` for the alias with checksum `sum`, in disk order (the last part, with
/// the 0x40 flag, first). A name that fills its last part exactly has no terminator (as Windows writes it).
pub fn lfn_entries(u: &[u16], sum: u8) -> Vec<[u8; ENTRY]> {
    const AT: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    let n = u.len().div_ceil(13);
    let mut out = Vec::with_capacity(n);
    for k in (1..=n).rev() {
        let mut e = [0xFFu8; ENTRY];
        e[0] = k as u8 | if k == n { 0x40 } else { 0 };
        e[11] = ATTR_LFN;
        e[12] = 0;
        e[13] = sum;
        e[26] = 0;
        e[27] = 0;
        for (i, &o) in AT.iter().enumerate() {
            let j = (k - 1) * 13 + i;
            let w: u16 = if j < u.len() {
                u[j]
            } else if j == u.len() {
                0
            } else {
                0xFFFF
            };
            e[o..o + 2].copy_from_slice(&w.to_le_bytes());
        }
        out.push(e);
    }
    out
}

/// The 13 UTF-16 units of one LFN entry.
pub fn lfn_units(e: &[u8]) -> [u16; 13] {
    const AT: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    let mut u = [0u16; 13];
    for (i, &o) in AT.iter().enumerate() {
        u[i] = u16_at(e, o);
    }
    u
}

/// A FAT time stamp (local time; FAT has no zone).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Stamp {
    pub date: u16,
    pub time: u16,
    pub tenth: u8,
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Local seconds since 1970 of a FAT date and time (`fat_time_fat2unix`); a broken field gives 1980-01-01.
pub fn local_secs(date: u16, time: u16) -> i64 {
    let (y, m, d) = (1980 + (date >> 9) as i64, ((date >> 5) & 15) as i64, (date & 31) as i64);
    let (h, mi, s) = ((time >> 11) as i64, ((time >> 5) & 63) as i64, ((time & 31) * 2) as i64);
    if !(1..=12).contains(&m) || d == 0 || h > 23 || mi > 59 || s > 59 {
        return days_from_civil(1980, 1, 1) * 86_400;
    }
    days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s
}

/// The FAT stamp of local seconds since 1970, clamped to FAT's range 1980-01-01 .. 2107-12-31.
pub fn stamp(local: i64) -> Stamp {
    let lo = days_from_civil(1980, 1, 1) * 86_400;
    let hi = days_from_civil(2107, 12, 31) * 86_400 + 86_399;
    let t = local.clamp(lo, hi);
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    Stamp {
        date: (((y - 1980) as u16) << 9) | ((m as u16) << 5) | d as u16,
        time: (((s / 3600) as u16) << 11) | ((((s / 60) % 60) as u16) << 5) | ((s % 60) / 2) as u16,
        tenth: ((s % 2) * 100) as u8,
    }
}

/// The "." and ".." entries of a new directory (`fat_alloc_new_dir`): ".." names cluster 0 for the root.
pub fn dot_entries(me: u32, parent: u32, t: Stamp) -> [[u8; ENTRY]; 2] {
    let dot = Entry::new(*b".          ", ATTR_DIR, me, t);
    let dotdot = Entry::new(*b"..         ", ATTR_DIR, parent, t);
    [dot.bytes(), dotdot.bytes()]
}

/// The short entry name a lookup by alias matches ("BASE.EXT").
pub fn alias_key(e: &Entry) -> Vec<u8> {
    name::short_key(&e.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_round_trip() {
        let mut e = Entry::new(*b"KERNEL     ", ATTR_ARCHIVE, 0x0012_3456, stamp(0));
        e.size = 70_000;
        assert_eq!(Entry::parse(&e.bytes()), e);
        assert_eq!(e.bytes()[20..22], [0x12, 0x00]);
        assert_eq!(e.bytes()[26..28], [0x56, 0x34]);
    }

    #[test]
    fn lfn_layout() {
        let u: Vec<u16> = "abcdefghijklm".encode_utf16().collect();
        let es = lfn_entries(&u, 0x5A);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0][0], 0x41);
        assert_eq!(lfn_units(&es[0]).to_vec(), u, "13 units fill one part: no terminator");
        let u2: Vec<u16> = "abcdefghijklmn".encode_utf16().collect();
        let es = lfn_entries(&u2, 1);
        assert_eq!((es.len(), es[0][0], es[1][0]), (2, 0x42, 0x01));
        let tail = lfn_units(&es[0]);
        assert_eq!((tail[0], tail[1], tail[2]), (b'n' as u16, 0, 0xFFFF));
    }

    #[test]
    fn times() {
        let t = days_from_civil(2025, 7, 14) * 86_400 + 12 * 3600 + 34 * 60 + 57;
        let s = stamp(t);
        assert_eq!(s.date, (45 << 9) | (7 << 5) | 14);
        assert_eq!(s.time, (12 << 11) | (34 << 5) | 28);
        assert_eq!(s.tenth, 100);
        assert_eq!(local_secs(s.date, s.time), t - 1);
        assert_eq!(stamp(0).date, (1 << 5) | 1, "before 1980: clamped");
        assert_eq!(local_secs(0, 0), days_from_civil(1980, 1, 1) * 86_400, "a zero date is 1980");
        for z in [-1000, 0, 3652, 20_000, 50_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }
}
