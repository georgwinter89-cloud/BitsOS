//! SMBIOS (DMI): the entry point and the four identity strings bitsboot uses to recognise a machine (BT-5).
//! Pure: the caller copies the entry point and the table out of firmware memory; every offset here is checked
//! against the slice it came with, so a lying length or a cut table ends the walk instead of a panic.
//! Follows linux-6.6/drivers/firmware/dmi_scan.c: `dmi_present` (:566), `dmi_smbios3_present` (:634),
//! `dmi_decode_table` (:87), `dmi_string_nosave` (:46), the type 1 and 2 slots of `dmi_decode` (:479-494).

use alloc::string::String;

/// Where the structure table is, as the entry point says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub table: u64,
    /// Exact length (32-bit entry point) or the maximum size (SMBIOS 3: the table ends at type 127).
    pub len: u64,
    /// Number of structures (32-bit entry point only; SMBIOS 3 has none).
    pub count: Option<u16>,
    pub major: u8,
    pub minor: u8,
}

/// Bytes an entry point can take (the SMBIOS 3 one is 24, the 2.x one 31; Linux copies at most 32).
pub const ENTRY_MAX: usize = 32;
/// The most of a structure table bitsboot copies (real tables are under 64 KiB; the 2.x length and the 3.x
/// maximum come from firmware, and a lying u32 must not make the loader allocate gigabytes).
pub const TABLE_MAX: u64 = 1 << 20;

fn sum0(b: &[u8]) -> bool {
    b.iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0
}

fn le(b: &[u8], at: usize, n: usize) -> u64 {
    b[at..at + n].iter().rev().fold(0u64, |v, &x| v << 8 | x as u64)
}

/// The entry point at the start of `buf` (`_SM3_` or `_SM_` with its `_DMI_` part), checksums checked.
pub fn entry(buf: &[u8]) -> Option<Entry> {
    if buf.len() >= 24 && &buf[..5] == b"_SM3_" {
        let n = buf[6] as usize;
        if !(24..=32).contains(&n) || buf.len() < n || !sum0(&buf[..n]) {
            return None;
        }
        return Some(Entry { table: le(buf, 16, 8), len: le(buf, 12, 4), count: None, major: buf[7], minor: buf[8] });
    }
    if buf.len() >= 31 && &buf[..4] == b"_SM_" {
        // A bad `_SM_` checksum only loses the version (Linux: smbios_ver = 0); the `_DMI_` part decides.
        let n = buf[5] as usize;
        let sm_ok = (30..=32).contains(&n) && buf.len() >= n && sum0(&buf[..n]);
        let d = &buf[16..];
        if &d[..5] != b"_DMI_" || !sum0(&d[..15]) {
            return None;
        }
        let (major, minor) = if sm_ok { (buf[6], buf[7]) } else { (d[14] >> 4, d[14] & 0x0F) };
        return Some(Entry { table: le(d, 8, 4), len: le(d, 6, 2), count: Some(le(d, 12, 2) as u16), major, minor });
    }
    None
}

/// The strings that name the machine (`/sys/class/dmi/id/{sys_vendor,product_name,board_vendor,board_name}`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ident {
    pub sys_vendor: Option<String>,
    pub product: Option<String>,
    pub board_vendor: Option<String>,
    pub board_name: Option<String>,
}

impl Ident {
    /// No system or board name at all (the table is empty, cut or has no type 1/2 structure with strings).
    pub fn is_empty(&self) -> bool {
        [&self.sys_vendor, &self.product, &self.board_vendor, &self.board_name].iter().all(|s| s.is_none())
    }

    /// One line for the loader log.
    pub fn text(&self) -> String {
        let f = |s: &Option<String>| s.clone().unwrap_or_else(|| String::from("-"));
        alloc::format!(
            "system \"{}\" \"{}\", board \"{}\" \"{}\"",
            f(&self.sys_vendor),
            f(&self.product),
            f(&self.board_vendor),
            f(&self.board_name)
        )
    }
}

/// String number `idx` (1-based) of a structure's string set; 0, a missing number or only spaces = none.
fn string(strings: &[u8], idx: u8) -> Option<String> {
    if idx == 0 {
        return None;
    }
    let s = strings.split(|&b| b == 0).nth(idx as usize - 1)?;
    let t = core::str::from_utf8(s).map(String::from).unwrap_or_else(|_| String::from_utf8_lossy(s).into_owned());
    let t = t.trim();
    if t.is_empty() { None } else { Some(String::from(t)) }
}

/// Fills a slot once (the first structure of a type wins, as `dmi_save_ident`), when the formatted area is long
/// enough to hold the string number at `off`.
fn save(slot: &mut Option<String>, fmt: &[u8], strings: &[u8], off: usize) {
    if slot.is_none()
        && let Some(&i) = fmt.get(off)
    {
        *slot = string(strings, i);
    }
}

/// Walks the structure table: stops at `count` structures, at type 127 (SMBIOS 3 only, as Linux), at a
/// structure that would run off the end, or at a header shorter than 4 bytes.
pub fn ident(table: &[u8], count: Option<u16>) -> Ident {
    let mut id = Ident::default();
    let (mut at, mut seen) = (0usize, 0u32);
    while count.is_none_or(|c| seen < c as u32) && at + 4 <= table.len() {
        let (ty, hl) = (table[at], table[at + 1] as usize);
        if hl < 4 || at + hl > table.len() {
            break;
        }
        let strings_at = at + hl;
        // The string set ends with two NUL bytes (also when it is empty).
        let mut end = strings_at;
        while end + 1 < table.len() && (table[end] != 0 || table[end + 1] != 0) {
            end += 1;
        }
        if end + 1 >= table.len() {
            break;
        }
        let (fmt, strings) = (&table[at..strings_at], &table[strings_at..end + 1]);
        match ty {
            1 => {
                save(&mut id.sys_vendor, fmt, strings, 4);
                save(&mut id.product, fmt, strings, 5);
            }
            2 => {
                save(&mut id.board_vendor, fmt, strings, 4);
                save(&mut id.board_name, fmt, strings, 5);
            }
            _ => {}
        }
        at = end + 2;
        seen += 1;
        if count.is_none() && ty == 127 {
            break;
        }
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Sets the byte at `sum_at` so that `b[from..to]` sums to 0.
    fn fix(mut b: Vec<u8>, sum_at: usize, from: usize, to: usize) -> Vec<u8> {
        b[sum_at] = 0;
        let s = b[from..to].iter().fold(0u8, |s, &x| s.wrapping_add(x));
        b[sum_at] = 0u8.wrapping_sub(s);
        b
    }

    fn sm3(table: u64, max: u32) -> Vec<u8> {
        let mut b = alloc::vec![0u8; 24];
        b[..5].copy_from_slice(b"_SM3_");
        b[6] = 24;
        b[7] = 3;
        b[8] = 6;
        b[12..16].copy_from_slice(&max.to_le_bytes());
        b[16..24].copy_from_slice(&table.to_le_bytes());
        fix(b, 5, 0, 24)
    }

    fn sm2(table: u32, len: u16, count: u16) -> Vec<u8> {
        let mut b = alloc::vec![0u8; 31];
        b[..4].copy_from_slice(b"_SM_");
        b[5] = 31;
        b[6] = 2;
        b[7] = 8;
        b[16..21].copy_from_slice(b"_DMI_");
        b[22..24].copy_from_slice(&len.to_le_bytes());
        b[24..28].copy_from_slice(&table.to_le_bytes());
        b[28..30].copy_from_slice(&count.to_le_bytes());
        b[30] = 0x28;
        let b = fix(b, 16 + 5, 16, 16 + 15);
        fix(b, 4, 0, 31)
    }

    fn structure(ty: u8, fmt_extra: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut v = alloc::vec![ty, (4 + fmt_extra.len()) as u8, 0, 0];
        v.extend_from_slice(fmt_extra);
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

    fn gaming_pc_table() -> Vec<u8> {
        let mut t = structure(0, &[1, 2, 0, 0], &["American Megatrends International, LLC.", "1.0"]);
        t.extend(structure(1, &[1, 2, 0, 0], &["ASRock", "Z890 Taichi OCF"]));
        t.extend(structure(2, &[1, 2, 0, 0], &["ASRock", "Z890 Taichi OCF"]));
        t.extend(structure(127, &[], &[]));
        t
    }

    #[test]
    fn entry_points() {
        assert_eq!(entry(&sm3(0x7f00_0000, 0x1234)), Some(Entry { table: 0x7f00_0000, len: 0x1234, count: None, major: 3, minor: 6 }));
        assert_eq!(entry(&sm2(0xE_0000, 300, 9)), Some(Entry { table: 0xE_0000, len: 300, count: Some(9), major: 2, minor: 8 }));
        let mut bad = sm3(1, 2);
        bad[10] ^= 1;
        assert_eq!(entry(&bad), None, "SMBIOS 3 checksum");
        let mut bad = sm2(1, 2, 3);
        bad[25] ^= 1;
        assert_eq!(entry(&bad), None, "_DMI_ checksum");
        let mut sm_bad = sm2(1, 2, 3);
        sm_bad[8] ^= 1; // only the _SM_ checksum breaks: the version comes from _DMI_ (BCD 0x28 = 2.8)
        assert_eq!(entry(&sm_bad).map(|e| (e.table, e.major, e.minor)), Some((1, 2, 8)));
        assert_eq!(entry(&sm3(1, 2)[..20]), None, "too short");
        assert_eq!(entry(b"garbage that is long enough to be read as one"), None);
        let mut long = sm3(5, 6);
        long[6] = 33;
        assert_eq!(entry(&fix(long, 5, 0, 24)), None, "entry length out of range");
    }

    #[test]
    fn identity_strings() {
        let id = ident(&gaming_pc_table(), None);
        assert_eq!(id.sys_vendor.as_deref(), Some("ASRock"));
        assert_eq!(id.product.as_deref(), Some("Z890 Taichi OCF"));
        assert_eq!(id.board_vendor.as_deref(), Some("ASRock"));
        assert_eq!(id.board_name.as_deref(), Some("Z890 Taichi OCF"));
        assert_eq!(id.text(), "system \"ASRock\" \"Z890 Taichi OCF\", board \"ASRock\" \"Z890 Taichi OCF\"");
        assert!(!id.is_empty());
        // Empty, cut before the first strings, or a table of only an end marker: nothing to match (dmi.unusable).
        let t = gaming_pc_table();
        for bad in [&[][..], &t[..6], &[127, 4, 0, 0, 0, 0][..]] {
            assert!(ident(bad, None).is_empty());
        }
    }

    #[test]
    fn count_and_end_marker() {
        // A 32-bit table: the count decides, structures after type 127 still count (Linux does not trust it).
        let mut t = structure(127, &[], &[]);
        t.extend(structure(1, &[1, 2], &["QEMU", "Standard PC"]));
        assert_eq!(ident(&t, Some(2)).sys_vendor.as_deref(), Some("QEMU"));
        assert_eq!(ident(&t, Some(1)).sys_vendor, None);
        assert_eq!(ident(&t, None).sys_vendor, None, "SMBIOS 3 stops at type 127");
    }

    #[test]
    fn odd_strings() {
        // Number 0, a number past the set, only spaces, padding around a name, a short formatted area.
        let t = structure(1, &[0, 3], &["x", "y"]);
        let id = ident(&t, None);
        assert_eq!((id.sys_vendor, id.product), (None, None));
        let t = structure(2, &[1, 2], &["   ", "  Board  "]);
        let id = ident(&t, None);
        assert_eq!((id.board_vendor, id.board_name.as_deref()), (None, Some("Board")));
        let t = structure(1, &[1], &["OnlyVendor"]);
        let id = ident(&t, None);
        assert_eq!((id.sys_vendor.as_deref(), id.product), (Some("OnlyVendor"), None));
        let t = structure(1, &[1, 2], &["\u{e4}rger", "x"]);
        assert_eq!(ident(&t, None).sys_vendor.as_deref(), Some("\u{e4}rger"));
        let mut t = structure(1, &[1, 2], &["V", "P"]);
        t.extend(structure(1, &[1, 2], &["Second", "Q"]));
        assert_eq!(ident(&t, None).sys_vendor.as_deref(), Some("V"), "the first structure of a type wins");
    }

    #[test]
    fn broken_tables_never_panic() {
        let good = gaming_pc_table();
        for cut in 0..good.len() {
            let _ = ident(&good[..cut], None);
            let _ = ident(&good[..cut], Some(u16::MAX));
        }
        // Header length 0..3, a length past the end, no double NUL.
        for hl in 0..4u8 {
            assert_eq!(ident(&[1, hl, 0, 0, b'A', 0, 0], None), Ident::default());
        }
        assert_eq!(ident(&[1, 200, 0, 0, 1, 2, 0, 0], None), Ident::default());
        assert_eq!(ident(&[1, 6, 0, 0, 1, 2, b'A', 0, b'B'], None), Ident::default());
        // Every byte value at every place of a small table.
        for pos in 0..good.len().min(64) {
            for v in [0u8, 1, 3, 4, 127, 255] {
                let mut t = good.clone();
                t[pos] = v;
                let _ = ident(&t, None);
            }
        }
    }
}
