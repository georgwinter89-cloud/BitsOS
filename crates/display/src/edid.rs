//! EDID detailed timings (DP-3): the exact refresh of the scanout mode. GOP reports a resolution but no rate;
//! the monitor's EDID (UEFI's EDID Active/Discovered protocol, handed on by the loader) has the pixel clock and
//! the totals of each detailed timing, so `clock / (htotal * vtotal)` is the rate the panel really runs at
//! (2560x1440 CVT-RB: 241.5 MHz / (2720 * 1481) = 59.9506 Hz, V1 FlipPace measured 16,680 us).
//!
//! Decoding follows linux-6.6/drivers/gpu/drm/drm_edid.c `drm_mode_detailed` (:3437) and the block walk of
//! `cea_for_each_detailed_block` (:3033); the rate is `drm_mode_vrefresh` (drm_modes.c:1286) in millihertz.
//! Block validity is Linux's `edid_block_check`/`edid_block_status_valid` (:1854, :1891): a base header with at
//! least 6 of 8 bytes right is repaired (`edid_fixup`), the base checksum and version 1 must hold, a CTA
//! extension with a bad checksum still counts.
//! Pure and bounded by the input's length (hostile bytes give None, never a panic).
//!
//! Only detailed timings (base block and CTA-861 extensions) give a rate. CTA short video descriptors and
//! standard timings are left out on purpose: a VIC names both the 60 Hz and the 59.94 Hz variant of a mode
//! (drm_match_cea_mode accepts either clock) and a standard timing has a whole-Hz rate only, so neither tells the
//! scanout's exact period better than the 60 Hz default; DisplayID timings are not decoded yet.
//!
//! GOP reports no rate, so a resolution that the EDID lists at several rates (a 144 Hz preferred timing plus a
//! 60 Hz one) is ambiguous: `matches` names them all and `timing_for` refuses (`Ambiguous`) instead of
//! guessing which one the firmware set.

/// One 128-byte EDID block.
pub const BLOCK: usize = 128;
const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
/// CTA-861 extension block tag.
const CTA_TAG: u8 = 0x02;
const PT_INTERLACED: u8 = 1 << 7;
const PT_STEREO: u8 = 1 << 5;

/// A detailed timing descriptor, decoded as `drm_mode_detailed` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub clock_hz: u64,
    pub hactive: u32,
    pub vactive: u32,
    pub htotal: u32,
    pub vtotal: u32,
    pub interlaced: bool,
}

impl Timing {
    /// Decodes an 18-byte descriptor; None for a display descriptor (pixel clock 0), a tiny, stereo or broken
    /// timing (Linux drops the same ones).
    pub fn decode(d: &[u8]) -> Option<Timing> {
        let d: &[u8; 18] = d.get(..18)?.try_into().ok()?;
        let clock = u16::from_le_bytes([d[0], d[1]]) as u64;
        if clock == 0 {
            return None;
        }
        let hactive = ((d[4] as u32 & 0xf0) << 4) | d[2] as u32;
        let hblank = ((d[4] as u32 & 0x0f) << 8) | d[3] as u32;
        let vactive = ((d[7] as u32 & 0xf0) << 4) | d[5] as u32;
        let vblank = ((d[7] as u32 & 0x0f) << 8) | d[6] as u32;
        let hsync_offset = ((d[11] as u32 & 0xc0) << 2) | d[8] as u32;
        let hsync_width = ((d[11] as u32 & 0x30) << 4) | d[9] as u32;
        let vsync_offset = ((d[11] as u32 & 0x0c) << 2) | (d[10] as u32 >> 4);
        let vsync_width = ((d[11] as u32 & 0x03) << 4) | (d[10] as u32 & 0x0f);
        let misc = d[17];
        if hactive < 64 || vactive < 64 || misc & PT_STEREO != 0 || hsync_width == 0 || vsync_width == 0 {
            return None;
        }
        let mut htotal = hactive + hblank;
        let mut vtotal = vactive + vblank;
        // "Some EDIDs have bogus h/vtotal values" (drm_mode_detailed).
        let hsync_end = hactive + hsync_offset + hsync_width;
        let vsync_end = vactive + vsync_offset + vsync_width;
        if hsync_end > htotal {
            htotal = hsync_end + 1;
        }
        if vsync_end > vtotal {
            vtotal = vsync_end + 1;
        }
        Some(Timing { clock_hz: clock * 10_000, hactive, vactive, htotal, vtotal, interlaced: misc & PT_INTERLACED != 0 })
    }

    /// The frame rate in millihertz, rounded to the nearest (`drm_mode_vrefresh` in Hz; interlaced: fields).
    pub fn refresh_mhz(&self) -> u32 {
        let num = self.clock_hz * 1000 * if self.interlaced { 2 } else { 1 };
        let den = self.htotal as u64 * self.vtotal as u64;
        if den == 0 {
            return 0;
        }
        u32::try_from((num + den / 2) / den).unwrap_or(u32::MAX)
    }

    /// One frame in ns, rounded to the nearest (exact to the clock; the millihertz rate alone is up to
    /// ~140 ns off per frame at 60 Hz).
    pub fn period_ns(&self) -> u64 {
        let num = self.htotal as u64 * self.vtotal as u64 * 1_000_000_000;
        let den = self.clock_hz * if self.interlaced { 2 } else { 1 };
        if den == 0 {
            return 0;
        }
        (num + den / 2) / den
    }
}

/// Why an EDID gave no rate for the mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdidError {
    /// Shorter than a block, wrong header or a bad checksum of the base block.
    Invalid,
    /// No detailed timing of that resolution.
    NoTiming,
    /// Several progressive timings of that resolution with different rates (`matches` lists them).
    Ambiguous,
}

/// `edid_fixup`'s default: a base header with at least this many of its 8 bytes right is repaired.
const HEADER_FIXUP: usize = 6;

/// Linux's base-block check: a header with at most two wrong bytes is repaired before the checksum (which the
/// monitor computed over the right header) is checked; the EDID version must be 1.
fn base_valid(base: &[u8]) -> bool {
    let score = base[..8].iter().zip(HEADER.iter()).filter(|(a, b)| a == b).count();
    if score < HEADER_FIXUP {
        return false;
    }
    let sum = HEADER.iter().chain(base[8..BLOCK].iter()).fold(0u8, |s, &x| s.wrapping_add(x));
    sum == 0 && base[18] == 1
}

/// The detailed timings of the base block (four descriptors at 54) and of every valid CTA-861 extension
/// (descriptors from the offset in byte 2), in EDID order: the first one of the base block is the preferred mode.
pub fn timings(edid: &[u8]) -> Result<impl Iterator<Item = Timing> + '_, EdidError> {
    let base = edid.get(..BLOCK).ok_or(EdidError::Invalid)?;
    if !base_valid(base) {
        return Err(EdidError::Invalid);
    }
    let base_dtds = (0..4).map(move |i| &base[54 + 18 * i..72 + 18 * i]);
    // Byte 126 counts the extensions; only those present in the buffer are read (a short copy is no error).
    // A CTA block counts even with a bad checksum (`edid_block_status_valid`); an all-zero block is no CTA block.
    let ext_count = base[126] as usize;
    let exts = edid[BLOCK..].as_chunks::<BLOCK>().0.iter().take(ext_count).filter(|b| b[0] == CTA_TAG).flat_map(|b| {
        let d = b[2] as usize;
        let n = if (4..=127).contains(&d) { (127 - d) / 18 } else { 0 };
        (0..n).map(move |i| &b[d + 18 * i..d + 18 * (i + 1)])
    });
    Ok(base_dtds.chain(exts).filter_map(Timing::decode))
}

/// The progressive detailed timings of `width` x `height`, one per rate (the first of each, in EDID order).
pub fn matches(edid: &[u8], width: u32, height: u32) -> Result<alloc::vec::Vec<Timing>, EdidError> {
    let mut v: alloc::vec::Vec<Timing> = alloc::vec::Vec::new();
    for t in timings(edid)?.filter(|t| t.hactive == width && t.vactive == height && !t.interlaced) {
        if !v.iter().any(|o| o.refresh_mhz() == t.refresh_mhz()) {
            v.push(t);
        }
    }
    Ok(v)
}

/// The rate the scanout mode `width` x `height` runs at: its progressive detailed timing, when the EDID lists
/// that resolution at one rate only (`Ambiguous` when at several: GOP does not say which one it set).
pub fn timing_for(edid: &[u8], width: u32, height: u32) -> Result<Timing, EdidError> {
    match matches(edid, width, height)?.as_slice() {
        [] => Err(EdidError::NoTiming),
        [t] => Ok(*t),
        _ => Err(EdidError::Ambiguous),
    }
}

/// EDID bytes from timing numbers, for host tests and ktests (the encoding `drm_mode_detailed` reverses).
pub mod sample {
    use super::{BLOCK, HEADER};
    use alloc::vec;
    use alloc::vec::Vec;

    /// An 18-byte descriptor from the raw timing numbers (the encoding `drm_mode_detailed` reverses).
    #[allow(clippy::too_many_arguments)]
    pub fn dtd(clock_10khz: u16, ha: u32, hb: u32, va: u32, vb: u32, hso: u32, hsw: u32, vso: u32, vsw: u32, misc: u8) -> [u8; 18] {
        let c = clock_10khz.to_le_bytes();
        [
            c[0],
            c[1],
            ha as u8,
            hb as u8,
            ((ha >> 8) << 4 | (hb >> 8)) as u8,
            va as u8,
            vb as u8,
            ((va >> 8) << 4 | (vb >> 8)) as u8,
            hso as u8,
            hsw as u8,
            ((vso & 0xf) << 4 | (vsw & 0xf)) as u8,
            ((hso >> 8) << 6 | (hsw >> 8) << 4 | (vso >> 4) << 2 | (vsw >> 4)) as u8,
            0x58,
            0x36,
            0x21,
            0,
            0,
            misc,
        ]
    }

    pub fn fix_checksum(b: &mut [u8]) {
        let s = b[..127].iter().fold(0u8, |s, &x| s.wrapping_add(x));
        b[127] = 0u8.wrapping_sub(s);
    }

    /// A base block with the given descriptors (unused slots: a monitor-name display descriptor).
    pub fn edid(dtds: &[[u8; 18]], extensions: u8) -> Vec<u8> {
        let mut b = vec![0u8; BLOCK];
        b[..8].copy_from_slice(&HEADER);
        b[18] = 1;
        b[19] = 4;
        for i in 0..4 {
            let slot = &mut b[54 + 18 * i..72 + 18 * i];
            match dtds.get(i) {
                Some(d) => slot.copy_from_slice(d),
                None => slot[..5].copy_from_slice(&[0, 0, 0, 0xfc, 0]),
            }
        }
        b[126] = extensions;
        fix_checksum(&mut b);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::sample::{dtd, edid, fix_checksum};
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// 2560x1440 CVT reduced blanking: 241.5 MHz, 2720 x 1481 (the Mini-PC's 59.95 Hz).
    fn qhd() -> [u8; 18] {
        dtd(24150, 2560, 160, 1440, 41, 48, 32, 3, 5, 0x1a)
    }

    #[test]
    fn the_mini_pc_mode() {
        let e = edid(&[qhd()], 0);
        let t = timing_for(&e, 2560, 1440).unwrap();
        assert_eq!((t.clock_hz, t.htotal, t.vtotal), (241_500_000, 2720, 1481));
        assert_eq!(t.refresh_mhz(), 59_951);
        // 2720 * 1481 / 241.5 MHz = 16,680,414.08 ns.
        assert_eq!(t.period_ns(), 16_680_414);
        // 1080p60 CEA: 148.5 MHz, 2200 x 1125 = exactly 60 Hz.
        let t = Timing::decode(&dtd(14850, 1920, 280, 1080, 45, 88, 44, 4, 5, 0x1e)).unwrap();
        assert_eq!((t.refresh_mhz(), t.period_ns()), (60_000, 16_666_667));
    }

    #[test]
    fn preferred_first_and_by_resolution() {
        let fhd = dtd(14850, 1920, 280, 1080, 45, 88, 44, 4, 5, 0x1e);
        let e = edid(&[qhd(), fhd], 0);
        assert_eq!(timings(&e).unwrap().count(), 2);
        assert_eq!(timing_for(&e, 1920, 1080).unwrap().clock_hz, 148_500_000);
        assert_eq!(timing_for(&e, 1024, 768), Err(EdidError::NoTiming));
    }

    #[test]
    fn cta_extension_timings() {
        let mut e = edid(&[qhd()], 1);
        let mut ext = vec![0u8; BLOCK];
        ext[0] = CTA_TAG;
        ext[1] = 3;
        ext[2] = 4; // descriptors right after the header: no data blocks
        ext[4..22].copy_from_slice(&dtd(59400, 3840, 560, 2160, 90, 176, 88, 8, 10, 0x1e));
        fix_checksum(&mut ext);
        e.extend_from_slice(&ext);
        let t = timing_for(&e, 3840, 2160).unwrap();
        assert_eq!(t.refresh_mhz(), 60_000);
        // A broken CTA checksum is tolerated, as Linux does (edid_block_status_valid); another tag is not read.
        let n = e.len();
        e[n - 1] ^= 1;
        assert_eq!(timing_for(&e, 3840, 2160).unwrap().refresh_mhz(), 60_000);
        e[BLOCK] = 0x70;
        assert_eq!(timing_for(&e, 3840, 2160), Err(EdidError::NoTiming), "a DisplayID block is not decoded");
        assert!(timing_for(&e, 2560, 1440).is_ok());
        // Extensions counted in byte 126 but not in the buffer: no error.
        let e = edid(&[qhd()], 3);
        assert!(timing_for(&e, 2560, 1440).is_ok());
    }

    #[test]
    fn refused_input() {
        assert_eq!(timing_for(&[], 1, 1).err(), Some(EdidError::Invalid));
        let mut e = edid(&[qhd()], 0);
        e[60] ^= 0x40;
        assert_eq!(timing_for(&e, 2560, 1440).err(), Some(EdidError::Invalid), "bad checksum");
        let mut e = edid(&[qhd()], 0);
        e[1] = 0;
        e[2] = 0;
        e[3] = 0;
        fix_checksum(&mut e);
        assert_eq!(timing_for(&e, 2560, 1440).err(), Some(EdidError::Invalid), "bad header (3 bytes)");
        // Two wrong header bytes are repaired, and the checksum is the one over the right header.
        let mut e = edid(&[qhd()], 0);
        e[1] = 0;
        e[6] = 0x7f;
        assert!(timing_for(&e, 2560, 1440).is_ok(), "header repair");
        let mut e = edid(&[qhd()], 0);
        e[18] = 2;
        fix_checksum(&mut e);
        assert_eq!(timing_for(&e, 2560, 1440).err(), Some(EdidError::Invalid), "version 2");
        assert_eq!(timing_for(&[0u8; BLOCK], 2560, 1440).err(), Some(EdidError::Invalid), "zero block");
        // Zero sync widths, stereo, tiny and interlaced timings are not the scanout's rate.
        for d in [
            dtd(24150, 2560, 160, 1440, 41, 48, 0, 3, 5, 0x1a),
            dtd(24150, 2560, 160, 1440, 41, 48, 32, 3, 5, 0x1a | PT_STEREO),
            dtd(24150, 2560, 160, 1440, 41, 48, 32, 3, 5, 0x1a | PT_INTERLACED),
        ] {
            assert_eq!(timing_for(&edid(&[d], 0), 2560, 1440), Err(EdidError::NoTiming), "{d:?}");
        }
        assert_eq!(Timing::decode(&dtd(100, 32, 8, 32, 8, 1, 1, 1, 1, 0)), None, "tiny");
        assert_eq!(Timing::decode(&[1, 2, 3]), None, "short");
        // Bogus totals are stretched past the sync end, as Linux does.
        let t = Timing::decode(&dtd(24150, 2560, 10, 1440, 2, 48, 32, 3, 5, 0x1a)).unwrap();
        assert_eq!((t.htotal, t.vtotal), (2560 + 48 + 32 + 1, 1440 + 3 + 5 + 1));
    }

    #[test]
    fn several_rates_at_one_size() {
        // 2560x1440 at 144 Hz as the preferred timing (586.59 MHz, 2720 x 1497 = 144.06 Hz) and at 59.95 Hz in a CTA block:
        // GOP may run either one, so no rate is taken.
        let hz144 = dtd(58659, 2560, 160, 1440, 57, 48, 32, 3, 5, 0x1a);
        let mut e = edid(&[hz144], 1);
        let mut ext = vec![0u8; BLOCK];
        ext[0] = CTA_TAG;
        ext[1] = 3;
        ext[2] = 4;
        ext[4..22].copy_from_slice(&qhd());
        fix_checksum(&mut ext);
        e.extend_from_slice(&ext);
        let m = matches(&e, 2560, 1440).unwrap();
        assert_eq!(m.iter().map(|t| t.refresh_mhz()).collect::<Vec<_>>(), vec![144_060, 59_951]);
        assert_eq!(timing_for(&e, 2560, 1440), Err(EdidError::Ambiguous));
        // The same rate twice (base block and CTA) is one rate, not an ambiguity.
        let mut e = edid(&[qhd(), qhd()], 0);
        assert_eq!(timing_for(&e, 2560, 1440).unwrap().refresh_mhz(), 59_951);
        // An interlaced timing of the same size does not count against the progressive one.
        e = edid(&[qhd(), dtd(24150, 2560, 160, 1440, 41, 48, 32, 3, 5, 0x1a | PT_INTERLACED)], 0);
        assert!(timing_for(&e, 2560, 1440).is_ok());
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for len in [0usize, 17, 127, 128, 129, 255, 256, 384, 1000] {
            for _ in 0..200 {
                let mut b: Vec<u8> = (0..len)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        x as u8
                    })
                    .collect();
                if len >= BLOCK {
                    b[..8].copy_from_slice(&HEADER);
                    fix_checksum(&mut b[..BLOCK]);
                }
                if let Ok(it) = timings(&b) {
                    for t in it {
                        let _ = (t.refresh_mhz(), t.period_ns());
                    }
                }
            }
        }
    }
}
