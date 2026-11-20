//! UPDATE (Georg 15.11.: "eine Nummer für alles"): the version number of this build. The desk gives a build its
//! number (`BITSOS_VERSION=<N>` while it builds, `cargo xtask update-release`); the same code carries the same
//! number on every machine and boot medium. 0 = a development build without a number (lanes, tests, QEMU).
//!
//! The kernel keeps [`MARK`] in its read-only data, so the install path reads a KERNEL file's number from its
//! loaded segments (`find`) without starting it; the initramfs carries the same number in `etc/bitsos/version`.
#![no_std]
#![forbid(unsafe_code)]

/// `BITSOS-VERSION=` + 8 digits + `;` + 0xB7 + newline: the digits are fixed in width so the mark has one length.
pub const MARK_LEN: usize = 26;
pub const PREFIX: &[u8] = b"BITSOS-VERSION=";

include!(concat!(env!("OUT_DIR"), "/stamp.rs"));

/// The initramfs entry that names the bundle's version (decimal, a newline after it).
pub const INITRD_ENTRY: &[u8] = b"etc/bitsos/version";

/// The number a mark at the start of `b` holds (None: no whole mark there).
fn mark_at(b: &[u8]) -> Option<u32> {
    let m = b.get(..MARK_LEN)?;
    if !m.starts_with(PREFIX) || m[PREFIX.len() + 8..] != *b";\xb7\n" {
        return None;
    }
    let digits = &m[PREFIX.len()..PREFIX.len() + 8];
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(digits.iter().fold(0u32, |a, d| a * 10 + (d - b'0') as u32))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    None,
    One(u32),
    /// Two marks with different numbers: no number can be trusted.
    Conflict(u32, u32),
}

/// The marks in `hay` (one loaded segment of a KERNEL file). The kernel's own [`PREFIX`] constant stands in its
/// data too, but without the digits and the end bytes after it, so it is no mark.
pub fn find(hay: &[u8], mut seen: Found) -> Found {
    let mut at = 0;
    while let Some(i) = hay.get(at..).and_then(|h| h.windows(PREFIX.len()).position(|w| w == PREFIX)) {
        let start = at + i;
        if let Some(n) = mark_at(&hay[start..]) {
            seen = match seen {
                Found::None => Found::One(n),
                Found::One(o) if o == n => Found::One(n),
                Found::One(o) => Found::Conflict(o, n),
                c => c,
            };
        }
        at = start + 1;
    }
    seen
}

/// The number an initramfs's `etc/bitsos/version` holds: decimal digits, white space around them allowed.
pub fn parse_entry(b: &[u8]) -> Option<u32> {
    let t = core::str::from_utf8(b).ok()?.trim();
    if t.is_empty() || t.len() > 8 || !t.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse().ok().filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(n: u32) -> [u8; MARK_LEN] {
        let mut m = [0u8; MARK_LEN];
        m[..PREFIX.len()].copy_from_slice(PREFIX);
        let mut d = n;
        for i in (0..8).rev() {
            m[PREFIX.len() + i] = b'0' + (d % 10) as u8;
            d /= 10;
        }
        m[PREFIX.len() + 8..].copy_from_slice(b";\xb7\n");
        m
    }

    #[test]
    fn the_build_mark_reads_back_as_the_build_number() {
        assert_eq!(MARK.len(), MARK_LEN);
        let f = find(&MARK, Found::None);
        assert_eq!(f, if NUMBER == 0 { Found::One(0) } else { Found::One(NUMBER) });
        assert!(UTS_VERSION.len() < 65, "fits a utsname field");
    }

    #[test]
    fn marks_in_data() {
        let mut hay = [0u8; 200];
        hay[10..10 + PREFIX.len()].copy_from_slice(PREFIX);
        assert_eq!(find(&hay, Found::None), Found::None, "the prefix constant alone is no mark");
        hay[50..50 + MARK_LEN].copy_from_slice(&mark(165));
        assert_eq!(find(&hay, Found::None), Found::One(165));
        hay[120..120 + MARK_LEN].copy_from_slice(&mark(165));
        assert_eq!(find(&hay, Found::None), Found::One(165), "the same number twice");
        hay[150..150 + MARK_LEN].copy_from_slice(&mark(166));
        assert_eq!(find(&hay, Found::None), Found::Conflict(165, 166));
        let cut = &hay[..50 + MARK_LEN - 1];
        assert_eq!(find(cut, Found::None), Found::None, "a mark cut at the end of a segment");
        let mut bad = mark(7);
        bad[PREFIX.len() + 3] = b'x';
        assert_eq!(find(&bad, Found::None), Found::None);
        assert_eq!(find(&mark(9), Found::One(8)), Found::Conflict(8, 9), "across segments");
    }

    #[test]
    fn initrd_entries() {
        assert_eq!(parse_entry(b"165\n"), Some(165));
        assert_eq!(parse_entry(b" 7 "), Some(7));
        for bad in [&b""[..], b"0", b"-1", b"1.5", b"123456789", b"\xff", b"16 5"] {
            assert_eq!(parse_entry(bad), None, "{bad:?}");
        }
    }
}
