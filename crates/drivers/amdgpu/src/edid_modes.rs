//! GX-4b: the monitor's modes beyond its detailed timings, from the EDID base block, as Linux adds them after the
//! detailed ones: standard, then established timings (linux-6.6/drivers/gpu/drm/drm_edid.c:6730-6733;
//! :3296 `drm_mode_std`, :3859 `add_established_modes`, :608 `edid_est_modes`). Not read (open point): CVT 3-byte
//! and 0xFA standard-timing descriptors, CTA-861 VICs.
//! Sizes and nominal rates only: the scaled switch keeps the link's timing. The caller validated the block
//! (`display::edid::timings` is Ok); a short buffer yields nothing.

use crate::mode::Size;

const REVISION: usize = 0x13;
const ESTABLISHED: usize = 0x23;
const STANDARD: usize = 0x26;
const STANDARD_COUNT: usize = 8;

/// `edid_est_modes` (drm_edid.c:608-667), bit i of t1 | t2 << 8 | (mfg_rsvd & 0x80) << 9: (width, height, Hz,
/// interlaced).
const EST: [(u32, u32, u32, bool); 17] = [
    (800, 600, 60, false),
    (800, 600, 56, false),
    (640, 480, 75, false),
    (640, 480, 72, false),
    (640, 480, 67, false),
    (640, 480, 60, false),
    (720, 400, 88, false),
    (720, 400, 70, false),
    (1280, 1024, 75, false),
    (1024, 768, 75, false),
    (1024, 768, 70, false),
    (1024, 768, 60, false),
    (1024, 768, 43, true),
    (832, 624, 75, false),
    (800, 600, 75, false),
    (800, 600, 72, false),
    (1152, 864, 75, false),
];

/// One standard timing (two bytes) as `drm_mode_std` reads it; None for an unused slot (`bad_std_timing`).
fn standard(a: u8, b: u8, revision: u8) -> Option<(Size, u32)> {
    if matches!((a, b), (0, 0) | (1, 1) | (0x20, 0x20)) {
        return None;
    }
    let w = u32::from(a) * 8 + 248;
    let h = match b >> 6 {
        0 if revision < 3 => w,
        0 => w * 10 / 16,
        1 => w * 3 / 4,
        2 => w * 4 / 5,
        _ => w * 9 / 16,
    };
    let hz = u32::from(b & 0x3F) + 60;
    // The "HDTV hack" (drm_edid.c:3330).
    let (w, h) = if hz == 60 && matches!((w, h), (1360, 765) | (1368, 769)) { (1366, 768) } else { (w, h) };
    Some((Size::new(w, h), hz * 1000))
}

/// The standard timings, then the progressive established timings: (size, rate in mHz).
pub fn extra(edid: &[u8]) -> impl Iterator<Item = (Size, u32)> + '_ {
    let rev = edid.get(REVISION).copied().unwrap_or(0);
    let std = edid
        .get(STANDARD..STANDARD + 2 * STANDARD_COUNT)
        .into_iter()
        .flat_map(|s| s.chunks_exact(2))
        .filter_map(move |p| standard(p[0], p[1], rev));
    let bits = edid.get(ESTABLISHED..ESTABLISHED + 3).map_or(0, |e| {
        u32::from(e[0]) | u32::from(e[1]) << 8 | u32::from(e[2] & 0x80) << 9
    });
    let est = EST
        .iter()
        .enumerate()
        .filter(move |&(i, m)| bits & (1 << i) != 0 && !m.3)
        .map(|(_, &(w, h, hz, _))| (Size::new(w, h), hz * 1000));
    std.chain(est)
}
