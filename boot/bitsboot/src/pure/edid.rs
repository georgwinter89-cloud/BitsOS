//! The monitor's EDID from the firmware (BT-edid, SN-5 CR3): the GOP handle's EFI_EDID_ACTIVE_PROTOCOL, else
//! its EFI_EDID_DISCOVERED_PROTOCOL (UEFI 2.10 §12.9.2: Active is the EDID in use, i.e. the override or the
//! discovered one). The bytes go to the kernel as BootInfo record EDID (tag 21); the kernel's readers
//! (`display::edid`, hdacodec `Sink::from_edid`) decide what they use, so only what makes the bytes no EDID
//! at all is refused here: fewer than one 128-byte block or a wrong header (linux-6.6/drivers/gpu/drm/drm_edid.c
//! `edid_header`, `drm_edid_block_valid`). A bad checksum is handed on and logged (the kernel's readers report it),
//! except that a source whose base block checksum is bad gives way to the other source when that one is clean:
//! Linux drops such a base block (drm_edid.c:1891-1895 `edid_block_status_valid` accepts a bad checksum only on
//! a CEA extension).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

pub const BLOCK: usize = 128;
pub const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
/// The most an EDID can be: the base block plus the 255 extensions byte 126 can count. Bytes the firmware
/// claims beyond that are not EDID.
pub const MAX: usize = 256 * BLOCK;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Active,
    Discovered,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Active => "active",
            Source::Discovered => "discovered",
        }
    }
}

/// Why a protocol's bytes were not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// No such protocol on the GOP handle.
    Absent,
    /// The protocol is there with SizeOfEdid 0 or a null pointer (UEFI: no EDID available).
    Empty,
    /// Less than one block.
    Short(usize),
    /// The first eight bytes are not the EDID header.
    Header,
    /// An EDID whose base block checksum is bad, passed over for the other source's clean one (or, when the
    /// chosen one is bad too, the other source's equally bad one).
    BaseChecksum,
}

impl Refused {
    pub fn name(self) -> &'static str {
        match self {
            Refused::Absent => "absent",
            Refused::Empty => "empty",
            Refused::Short(_) => "short",
            Refused::Header => "bad_header",
            Refused::BaseChecksum => "bad_checksum",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked<'a> {
    /// What goes into the BootInfo: whole blocks only, at most base + byte-126 extensions.
    pub bytes: &'a [u8],
    pub blocks: usize,
    /// Extensions byte 126 announces but the firmware did not give.
    pub missing: usize,
    /// Blocks whose 128 bytes do not sum to 0 mod 256.
    pub bad_checksums: usize,
    /// The base block's checksum is right (Linux refuses the whole EDID otherwise).
    pub base_ok: bool,
    /// PNP id, three letters `A`..`Z` (bytes 8-9, big endian, 5 bits each).
    pub manufacturer: [u8; 3],
    pub product: u16,
}

pub fn check(raw: &[u8]) -> Result<Checked<'_>, Refused> {
    if raw.len() < BLOCK {
        return Err(Refused::Short(raw.len()));
    }
    if raw[..8] != HEADER {
        return Err(Refused::Header);
    }
    let wanted = 1 + raw[126] as usize;
    let have = raw.len().min(MAX) / BLOCK;
    let blocks = have.min(wanted);
    let bytes = &raw[..blocks * BLOCK];
    let sums_to_zero = |b: &[u8]| b.iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0;
    let bad_checksums = bytes.chunks_exact(BLOCK).filter(|b| !sums_to_zero(b)).count();
    let id = u16::from_be_bytes([raw[8], raw[9]]);
    let letter = |shift: u16| b'@' + ((id >> shift) & 0x1F) as u8;
    Ok(Checked {
        bytes,
        blocks,
        missing: wanted - blocks,
        bad_checksums,
        base_ok: sums_to_zero(&bytes[..BLOCK]),
        manufacturer: [letter(10), letter(5), letter(0)],
        product: u16::from_le_bytes([raw[10], raw[11]]),
    })
}

/// What the firmware gave, per protocol: `None` = protocol absent, `Some(&[])` = present but empty.
pub struct Pick<'a> {
    pub chosen: Option<(Source, Checked<'a>)>,
    /// Every source looked at before the chosen one (or all of them), with the reason it was not taken.
    pub passed: Vec<(Source, Refused)>,
}

/// The firmware's length claim for one protocol as the bytes to copy: 0 for a null pointer or SizeOfEdid 0
/// (UEFI: no EDID), never more than [`MAX`] whatever SizeOfEdid says.
pub fn claimed_len(null: bool, size_of_edid: u32) -> usize {
    if null { 0 } else { (size_of_edid as usize).min(MAX) }
}

/// Active first (UEFI: the EDID in use); Discovered when Active is unusable, or when Active's base block
/// checksum is bad and Discovered's is right.
pub fn pick<'a>(active: Option<&'a [u8]>, discovered: Option<&'a [u8]>) -> Pick<'a> {
    let checked = |raw: Option<&'a [u8]>| match raw {
        None => Err(Refused::Absent),
        Some([]) => Err(Refused::Empty),
        Some(b) => check(b),
    };
    let mut passed = Vec::new();
    // A usable EDID whose base checksum is bad, kept in case the other source is no better.
    let mut tainted: Option<(Source, Checked<'a>)> = None;
    for (source, raw) in [(Source::Active, active), (Source::Discovered, discovered)] {
        match checked(raw) {
            Ok(c) if c.base_ok => {
                if let Some((s, _)) = tainted.take() {
                    passed.push((s, Refused::BaseChecksum));
                }
                return Pick { chosen: Some((source, c)), passed };
            }
            Ok(c) if tainted.is_none() => tainted = Some((source, c)),
            Ok(_) => passed.push((source, Refused::BaseChecksum)),
            Err(e) => passed.push((source, e)),
        }
    }
    Pick { chosen: tainted, passed }
}

/// The loader log line: `edid.found source=active bytes=256 blocks=2 manufacturer=IVM product=0x6633
/// checksum=ok` or `edid.none active=absent discovered=empty`.
pub fn line(p: &Pick) -> String {
    let mut s = String::new();
    match &p.chosen {
        Some((src, c)) => {
            let m = core::str::from_utf8(&c.manufacturer).unwrap_or("???");
            let _ = write!(s, "edid.found source={} bytes={} blocks={} manufacturer={m} product={:#06x} checksum={}",
                src.name(), c.bytes.len(), c.blocks, c.product, if c.bad_checksums == 0 { "ok" } else { "bad" });
            if c.missing > 0 {
                let _ = write!(s, " missing_extensions={}", c.missing);
            }
        }
        None => s.push_str("edid.none"),
    }
    for (src, why) in &p.passed {
        let _ = write!(s, " {}={}", src.name(), why.name());
        if let Refused::Short(n) = why {
            let _ = write!(s, "({n} B)");
        }
    }
    s
}

/// Whether the line is worth a warning: a source was there but its bytes were no EDID.
pub fn suspicious(p: &Pick) -> bool {
    p.passed.iter().any(|(_, r)| matches!(r, Refused::Short(_) | Refused::Header))
        || p.chosen.as_ref().is_some_and(|(_, c)| c.bad_checksums > 0 || c.missing > 0)
}

/// A generic test monitor (manufacturer GEN, product 0x2560, name "BitsOS test"): EDID 1.4, one detailed
/// timing 2560x1440 at 59.95 Hz (CVT reduced blanking: 241.5 MHz, htotal 2720, vtotal 1481) and a CEA-861
/// extension with stereo LPCM 32/44.1/48 kHz 16/20/24 bit and front left/right speakers. For the
/// hypervisor-only test hooks (QEMU's OVMF installs no EDID protocol) and the host tests.
pub fn fixture() -> Vec<u8> {
    let mut e = alloc::vec![0u8; 2 * BLOCK];
    e[..8].copy_from_slice(&HEADER);
    e[8..10].copy_from_slice(&((7u16 << 10) | (5 << 5) | 14).to_be_bytes());
    e[10..12].copy_from_slice(&0x2560u16.to_le_bytes());
    e[16] = 1; // week
    e[17] = 36; // 2026
    e[18] = 1;
    e[19] = 4;
    e[20] = 0xA5; // digital, 8 bit per colour, DisplayPort
    e[21] = 60;
    e[22] = 34;
    e[23] = 120; // gamma 2.2
    e[24] = 0x06;
    e[38..54].fill(0x01); // no standard timings
    e[54..72].copy_from_slice(&[0x56, 0x5E, 0x00, 0xA0, 0xA0, 0xA0, 0x29, 0x50, 0x30, 0x20, 0x35, 0x00, 0x55, 0x50, 0x21, 0, 0, 0x1A]);
    e[72..77].copy_from_slice(&[0, 0, 0, 0xFC, 0]);
    e[77..90].copy_from_slice(b"BitsOS test\n ");
    e[90..95].copy_from_slice(&[0, 0, 0, 0x10, 0]);
    e[108..113].copy_from_slice(&[0, 0, 0, 0x10, 0]);
    e[126] = 1;
    // CEA-861 rev 3: basic audio; audio data block (LPCM, 2 ch, 32/44.1/48 kHz, 16/20/24 bit), speaker
    // allocation (FL/FR); no detailed timings (offset 12 = end of the data blocks).
    e[128..140].copy_from_slice(&[0x02, 0x03, 12, 0x40, 0x23, 0x09, 0x07, 0x07, 0x83, 0x01, 0x00, 0x00]);
    for blk in e.chunks_exact_mut(BLOCK) {
        let sum = blk[..127].iter().fold(0u8, |s, &x| s.wrapping_add(x));
        blk[127] = sum.wrapping_neg();
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    /// A generic EDID 1.4 (manufacturer GEN, product 0x1234), one 1920x1080@60 detailed timing (148.5 MHz),
    /// `ext` extension blocks of CEA filler; every checksum fixed.
    fn sample(ext: u8) -> Vec<u8> {
        let mut e = vec![0u8; BLOCK * (1 + ext as usize)];
        e[..8].copy_from_slice(&HEADER);
        e[8..10].copy_from_slice(&((7u16 << 10) | (5 << 5) | 14).to_be_bytes());
        e[10..12].copy_from_slice(&0x1234u16.to_le_bytes());
        e[18] = 1;
        e[19] = 4;
        e[54..66].copy_from_slice(&[0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C, 0x45, 0x00]);
        e[126] = ext;
        for b in 1..=ext as usize {
            e[b * BLOCK] = 0x02;
            e[b * BLOCK + 1] = 0x03;
        }
        for blk in e.chunks_exact_mut(BLOCK) {
            let sum = blk[..127].iter().fold(0u8, |s, &x| s.wrapping_add(x));
            blk[127] = sum.wrapping_neg();
        }
        e
    }

    #[test]
    fn a_good_edid_is_taken_whole() {
        for ext in [0u8, 1, 3] {
            let e = sample(ext);
            let c = check(&e).unwrap();
            assert_eq!(c.bytes, &e[..]);
            assert_eq!((c.blocks, c.missing, c.bad_checksums), (1 + ext as usize, 0, 0));
            assert_eq!(&c.manufacturer, b"GEN");
            assert_eq!(c.product, 0x1234);
        }
    }

    #[test]
    fn short_and_headerless_bytes_are_refused() {
        let e = sample(0);
        assert_eq!(check(&e[..127]), Err(Refused::Short(127)));
        assert_eq!(check(&[]), Err(Refused::Short(0)));
        let mut bad = e.clone();
        bad[0] = 0xFF;
        assert_eq!(check(&bad), Err(Refused::Header));
    }

    #[test]
    fn padding_is_cut_and_missing_extensions_counted() {
        // The firmware hands more bytes than byte 126 announces: only the announced blocks go on.
        let mut e = sample(1);
        e.extend_from_slice(&[0xAA; 300]);
        let c = check(&e).unwrap();
        assert_eq!((c.bytes.len(), c.blocks, c.missing), (256, 2, 0));
        // It announces two extensions but gives one and a half: whole blocks only.
        let mut e = sample(1);
        e[126] = 2;
        e.extend_from_slice(&[0; 64]);
        let c = check(&e).unwrap();
        assert_eq!((c.bytes.len(), c.blocks, c.missing), (256, 2, 1));
        // byte 126 changed without a new checksum: handed on, counted.
        assert_eq!(c.bad_checksums, 1);
    }

    #[test]
    fn huge_claims_stop_at_the_format_limit() {
        let mut e = sample(0);
        e[126] = 255;
        e.resize(MAX + 4096, 0);
        let c = check(&e).unwrap();
        assert_eq!((c.bytes.len(), c.blocks, c.missing), (MAX, 256, 0));
    }

    #[test]
    fn active_wins_and_discovered_is_the_fallback() {
        let a = sample(1);
        let d = sample(0);
        let p = pick(Some(&a), Some(&d));
        let (src, c) = p.chosen.as_ref().unwrap();
        assert_eq!((*src, c.bytes.len()), (Source::Active, 256));
        assert!(p.passed.is_empty());
        assert!(line(&p).starts_with("edid.found source=active bytes=256 blocks=2 manufacturer=GEN product=0x1234 checksum=ok"));
        assert!(!suspicious(&p));

        for active in [None, Some(&[][..]), Some(&a[..100])] {
            let p = pick(active, Some(&d));
            assert_eq!(p.chosen.as_ref().map(|(s, c)| (*s, c.bytes.len())), Some((Source::Discovered, 128)));
            assert_eq!(p.passed.len(), 1);
        }
        let p = pick(Some(&a[..100]), Some(&d));
        assert!(line(&p).ends_with(" active=short(100 B)"), "{}", line(&p));
        assert!(suspicious(&p));
    }

    #[test]
    fn the_fixture_is_a_clean_two_block_edid() {
        let f = fixture();
        let c = check(&f).unwrap();
        assert_eq!((c.bytes.len(), c.blocks, c.missing, c.bad_checksums), (256, 2, 0, 0));
        assert_eq!((&c.manufacturer, c.product), (b"GEN", 0x2560));
        // The detailed timing: 2560x1440, clock 241.5 MHz, totals 2720 x 1481.
        let d = &f[54..72];
        let clock = u16::from_le_bytes([d[0], d[1]]) as u32 * 10_000;
        let (ha, hb) = (d[2] as u32 | (d[4] as u32 >> 4) << 8, d[3] as u32 | (d[4] as u32 & 0xF) << 8);
        let (va, vb) = (d[5] as u32 | (d[7] as u32 >> 4) << 8, d[6] as u32 | (d[7] as u32 & 0xF) << 8);
        assert_eq!((clock, ha, ha + hb, va, va + vb), (241_500_000, 2560, 2720, 1440, 1481));
    }

    #[test]
    fn nothing_usable_is_none() {
        let p = pick(None, None);
        assert!(p.chosen.is_none());
        assert_eq!(line(&p), "edid.none active=absent discovered=absent");
        assert!(!suspicious(&p));
        let mut bad = sample(0);
        bad[3] = 0;
        let p = pick(Some(&[]), Some(&bad));
        assert_eq!(line(&p), "edid.none active=empty discovered=bad_header");
        assert!(suspicious(&p));
    }

    #[test]
    fn a_clean_base_block_beats_a_bad_one() {
        let mut a = sample(1);
        a[20] ^= 1;
        let d = sample(0);
        let p = pick(Some(&a), Some(&d));
        assert_eq!(p.chosen.as_ref().map(|(s, c)| (*s, c.bytes.len(), c.base_ok)), Some((Source::Discovered, 128, true)));
        assert_eq!(p.passed, [(Source::Active, Refused::BaseChecksum)]);
        assert!(line(&p).ends_with("checksum=ok active=bad_checksum"), "{}", line(&p));
        assert!(!suspicious(&p));

        // Both bad: Active is still the one in use, and the line says so.
        let mut d2 = d.clone();
        d2[21] ^= 1;
        let p = pick(Some(&a), Some(&d2));
        assert_eq!(p.chosen.as_ref().map(|(s, c)| (*s, c.base_ok)), Some((Source::Active, false)));
        assert_eq!(p.passed, [(Source::Discovered, Refused::BaseChecksum)]);
        assert!(line(&p).contains("checksum=bad"));
        assert!(suspicious(&p));

        // A bad checksum only in an extension does not count against the base block (Linux keeps CEA blocks).
        let mut e = sample(1);
        e[BLOCK + 5] ^= 1;
        let p = pick(Some(&e), Some(&d));
        assert_eq!(p.chosen.as_ref().map(|(s, c)| (*s, c.base_ok, c.bad_checksums)), Some((Source::Active, true, 1)));
    }

    #[test]
    fn the_length_claim_is_clamped() {
        assert_eq!(claimed_len(true, 256), 0);
        assert_eq!(claimed_len(false, 0), 0);
        assert_eq!(claimed_len(false, 128), 128);
        assert_eq!(claimed_len(false, MAX as u32 + 1), MAX);
        assert_eq!(claimed_len(false, u32::MAX), MAX);
        assert_eq!(claimed_len(true, u32::MAX), 0);
    }

    #[test]
    fn a_bad_checksum_is_handed_on() {
        let mut e = sample(0);
        e[20] ^= 1;
        let p = pick(Some(&e), None);
        let (_, c) = p.chosen.as_ref().unwrap();
        assert_eq!((c.bytes.len(), c.bad_checksums), (128, 1));
        assert!(line(&p).contains("checksum=bad"));
        assert!(suspicious(&p));
    }
}
