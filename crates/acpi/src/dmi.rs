//! SYS-K: the SMBIOS identity strings Linux shows under `/sys/class/dmi/id` (linux-7.3.1/drivers/firmware/dmi_scan.c
//! `dmi_decode` :488, `dmi_string_nosave` :50, `dmi_save_uuid` :234, `dmi_save_release`; dmi-id.c for the names and
//! modes). Read once at boot from the table the entry point bitsboot handed over names (firmware memory, through
//! the same reader as the ACPI tables); every offset is checked against the bytes read, so a lying length ends the
//! walk. Serial numbers, the UUID and nothing else are root-only (0400), as Linux has them.

use crate::mem::PhysMem;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One attribute: its file name, text (without the newline) and mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub text: String,
    pub mode: u32,
}

/// Where the structure table is and the SMBIOS version (major, minor, docrev).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub table: u64,
    pub len: u64,
    pub count: Option<u16>,
    pub version: u32,
}

/// The most of a structure table read (real ones are a few KiB).
pub const TABLE_MAX: u64 = 1 << 20;

fn sum0(b: &[u8]) -> bool {
    b.iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0
}

fn le(b: &[u8], at: usize, n: usize) -> u64 {
    b[at..at + n].iter().rev().fold(0u64, |v, &x| v << 8 | x as u64)
}

/// The entry point (`_SM3_`, or `_SM_` with its `_DMI_` part; `dmi_smbios3_present`/`dmi_present`).
pub fn entry(b: &[u8]) -> Option<Entry> {
    if b.len() >= 24 && &b[..5] == b"_SM3_" {
        let n = b[6] as usize;
        if !(24..=32).contains(&n) || b.len() < n || !sum0(&b[..n]) {
            return None;
        }
        let version = (b[7] as u32) << 16 | (b[8] as u32) << 8 | b[9] as u32;
        return Some(Entry { table: le(b, 0x10, 8), len: le(b, 0x0c, 4).min(TABLE_MAX), count: None, version });
    }
    if b.len() >= 31 && &b[..4] == b"_SM_" && &b[16..21] == b"_DMI_" {
        let n = (b[5] as usize).min(31);
        if !sum0(&b[..n.max(16)]) || !sum0(&b[16..31]) {
            return None;
        }
        let version = ((b[6] as u32) << 8 | b[7] as u32) << 8;
        return Some(Entry { table: le(b, 0x18, 4), len: le(b, 0x16, 2), count: Some(le(b, 0x1c, 2) as u16), version });
    }
    None
}

/// String `s` (1-based) of the structure at `at` with formatted length `len`; None for 0 or a string of spaces
/// only (Linux's empty string, still shown).
fn string(t: &[u8], at: usize, len: usize, s: u8) -> Option<String> {
    let mut p = at + len;
    for _ in 1..s {
        let n = t.get(p..)?.iter().position(|&c| c == 0)?;
        if n == 0 {
            return None;
        }
        p += n + 1;
    }
    let rest = t.get(p..)?;
    let n = rest.iter().position(|&c| c == 0)?;
    let text = &rest[..n];
    (s != 0 && text.iter().any(|&c| c != b' ')).then(|| String::from_utf8_lossy(text).into_owned())
}

/// `%pUl` (SMBIOS 2.6 and later: the first three fields little-endian) or `%pUb`.
fn uuid(d: &[u8], little: bool) -> Option<String> {
    if d.iter().all(|&x| x == 0) || d.iter().all(|&x| x == 0xff) {
        return None;
    }
    let order: [usize; 16] = if little { [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15] } else { core::array::from_fn(|i| i) };
    let mut s = String::new();
    for (i, &j) in order.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{:02x}", d[j]));
    }
    Some(s)
}

/// (structure type, string offset, attribute, mode) of `dmi_decode` and dmi-id.c.
const STRINGS: [(u8, usize, &str, u32); 17] = [
    (0, 4, "bios_vendor", 0o444),
    (0, 5, "bios_version", 0o444),
    (0, 8, "bios_date", 0o444),
    (1, 4, "sys_vendor", 0o444),
    (1, 5, "product_name", 0o444),
    (1, 6, "product_version", 0o444),
    (1, 7, "product_serial", 0o400),
    (1, 25, "product_sku", 0o444),
    (1, 26, "product_family", 0o444),
    (2, 4, "board_vendor", 0o444),
    (2, 5, "board_name", 0o444),
    (2, 6, "board_version", 0o444),
    (2, 7, "board_serial", 0o400),
    (2, 8, "board_asset_tag", 0o444),
    (3, 4, "chassis_vendor", 0o444),
    (3, 6, "chassis_version", 0o444),
    (3, 7, "chassis_serial", 0o400),
];

/// The attributes of a structure table, sorted by name; the first structure of a type wins (`dmi_ident[slot]`
/// set once).
pub fn decode(t: &[u8], e: &Entry) -> Vec<Field> {
    let mut out: Vec<Field> = Vec::new();
    let put = |out: &mut Vec<Field>, name: &'static str, text: String, mode: u32| {
        if !out.iter().any(|f| f.name == name) {
            out.push(Field { name, text, mode });
        }
    };
    let mut at = 0usize;
    let mut seen = 0u32;
    while at + 4 <= t.len() && e.count.is_none_or(|c| seen < c as u32) {
        let (ty, len) = (t[at], t[at + 1] as usize);
        if len < 4 || at + len > t.len() {
            break;
        }
        let d = &t[at..at + len];
        for (sty, off, name, mode) in STRINGS {
            if sty == ty && len > off {
                put(&mut out, name, string(t, at, len, d[off]).unwrap_or_default(), mode);
            }
        }
        match ty {
            0 if len >= 22 && !(d[20] == 0xff && d[21] == 0xff) => put(&mut out, "bios_release", format!("{}.{}", d[20], d[21]), 0o444),
            1 if len >= 24 => {
                if let Some(u) = uuid(&d[8..24], e.version >= 0x020600) {
                    put(&mut out, "product_uuid", u, 0o400);
                }
            }
            3 if len > 5 => put(&mut out, "chassis_type", format!("{}", d[5] & 0x7f), 0o444),
            _ => {}
        }
        // The strings end with two zeros (an empty set is just those two).
        let mut p = at + len;
        while p + 1 < t.len() && (t[p] != 0 || t[p + 1] != 0) {
            p += 1;
        }
        at = p + 2;
        seen += 1;
        if ty == 127 {
            break;
        }
    }
    out.sort_by(|a, b| a.name.cmp(b.name));
    out
}

/// Reads the entry point at `phys` and the table it names; None when either cannot be read or makes no sense.
pub fn read(mem: &mut dyn PhysMem, phys: u64) -> Option<(Entry, Vec<Field>)> {
    let mut b = [0u8; 32];
    mem.read(phys, &mut b).ok()?;
    let e = entry(&b)?;
    let mut t = Vec::new();
    t.try_reserve_exact(e.len as usize).ok()?;
    t.resize(e.len as usize, 0);
    mem.read(e.table, &mut t).ok()?;
    Some((e, decode(&t, &e)))
}

static IDENT: ksync::Once<Vec<Field>> = ksync::Once::new();

/// Keeps what boot read (once).
pub fn set(fields: Vec<Field>) {
    IDENT.get_or_init(|| fields);
}

/// The attributes, empty without an SMBIOS table.
pub fn fields() -> &'static [Field] {
    IDENT.get().map_or(&[], |v| v.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structure(ty: u8, formatted: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut v = alloc::vec![ty, (formatted.len() + 4) as u8, 0, 0];
        v.extend_from_slice(formatted);
        for s in strings {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        if strings.is_empty() {
            v.push(0);
        }
        v.push(0);
        v
    }

    fn table() -> Vec<u8> {
        // Type 0: vendor 1, version 2, date 3 at 4/5/8, release 1.20 at 20/21.
        let mut bios = alloc::vec![0u8; 20];
        (bios[0], bios[1], bios[4]) = (1, 2, 3);
        bios[16] = 1;
        bios[17] = 20;
        let mut t = structure(0, &bios, &["AMI", "1.07", "03/15/2024"]);
        // Type 1: vendor, name, version, serial, uuid, sku, family.
        let mut sys = alloc::vec![0u8; 23];
        (sys[0], sys[1], sys[2], sys[3]) = (1, 2, 3, 4);
        sys[4..20].copy_from_slice(&[0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        (sys[21], sys[22]) = (5, 0);
        t.extend(structure(1, &sys, &["Bee", "Mini 8", "   ", "SERIAL-1", "SKU1"]));
        t.extend(structure(2, &[1, 2, 0, 0, 0], &["Bee", "Board X"]));
        t.extend(structure(1, &[1, 2], &["Other", "Later"]));
        t.extend(structure(127, &[], &[]));
        t
    }

    #[test]
    fn identity_strings_as_linux_saves_them() {
        let e = Entry { table: 0, len: 0, count: None, version: 0x030500 };
        let f = decode(&table(), &e);
        let get = |n: &str| f.iter().find(|x| x.name == n).map(|x| (x.text.as_str(), x.mode));
        assert_eq!(get("bios_vendor"), Some(("AMI", 0o444)));
        assert_eq!(get("bios_date"), Some(("03/15/2024", 0o444)));
        assert_eq!(get("bios_release"), Some(("1.20", 0o444)));
        assert_eq!(get("sys_vendor"), Some(("Bee", 0o444)), "the first type-1 structure wins");
        assert_eq!(get("product_name"), Some(("Mini 8", 0o444)));
        assert_eq!(get("product_version"), Some(("", 0o444)), "spaces only: Linux's empty string, still shown");
        assert_eq!(get("product_serial"), Some(("SERIAL-1", 0o400)));
        assert_eq!(get("product_uuid"), Some(("00112233-4455-6677-8899-aabbccddeeff", 0o400)), "%pUl");
        assert_eq!(get("product_sku"), Some(("SKU1", 0o444)));
        assert_eq!(get("product_family"), Some(("", 0o444)), "string 0: empty");
        assert_eq!(get("board_name"), Some(("Board X", 0o444)));
        assert_eq!(get("board_serial"), Some(("", 0o400)));
        assert_eq!(get("chassis_vendor"), None, "no type 3: no chassis files");
        assert!(f.windows(2).all(|w| w[0].name < w[1].name));
    }

    #[test]
    fn entry_points_and_cut_tables() {
        let mut ep = alloc::vec![0u8; 24];
        ep[..5].copy_from_slice(b"_SM3_");
        (ep[6], ep[7], ep[8], ep[9]) = (24, 3, 5, 0);
        ep[0x0c..0x10].copy_from_slice(&0x1234u32.to_le_bytes());
        ep[0x10..0x18].copy_from_slice(&0x7654_3000u64.to_le_bytes());
        let s = ep.iter().fold(0u8, |a, &x| a.wrapping_add(x));
        ep[5] = 0u8.wrapping_sub(s);
        assert_eq!(entry(&ep), Some(Entry { table: 0x7654_3000, len: 0x1234, count: None, version: 0x030500 }));
        ep[9] ^= 1;
        assert_eq!(entry(&ep), None, "bad checksum");
        let t = table();
        let e = Entry { table: 0, len: 0, count: None, version: 0x020500 };
        for cut in 0..t.len() {
            let _ = decode(&t[..cut], &e);
        }
        assert_eq!(decode(&t, &e).iter().find(|x| x.name == "product_uuid").map(|x| x.text.clone()).as_deref(), Some("33221100-5544-7766-8899-aabbccddeeff"), "%pUb before 2.6");
    }
}
