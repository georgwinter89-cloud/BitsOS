//! Boot-time pieces of gopfb: the write-speed probe of the mapped framebuffer and the CONFIG refresh key.
//!
//! Speed probe (V1 `P/fbspeed.asm`, spec/gpu.md §1.1): one timed write of 64 KiB of black through the window
//! right after mapping. A WC window writes at about 0.10-0.13 ms/MB (V1, Mini-PC); far slower means the window
//! is not WC after all (bitsboot's fallback mapping, a PAT that was not programmed), and a full present would
//! take tens of ms. MTRRs cannot cause this with PAT on: a PAT WC entry gives WC whatever the MTRR type (SDM
//! vol. 3A "Effective Page-Level Memory Types", the WC row; Linux's `arch_phys_wc_add` does nothing with PAT).
//! The probe writes black, which is what the RAM frame holds before the first present.

use crate::FbTarget;

/// Bytes the probe writes.
pub const PROBE_BYTES: usize = 64 << 10;
/// Above this the window is reported slow (about 8x V1's WC speed; UC writes are ~25x slower).
pub const SLOW_US_PER_MB: u64 = 1_000;

/// Writes `PROBE_BYTES` of black at offset 0 (fewer if the target is smaller) and returns (bytes, ns).
pub fn probe(t: &dyn FbTarget, now_ns: &dyn Fn() -> u64) -> Option<(usize, u64)> {
    let n = PROBE_BYTES.min(t.len()) & !3;
    if n == 0 {
        return None;
    }
    let zero = [0u32; 256];
    let t0 = now_ns();
    let mut off = 0;
    while off < n {
        let k = (n - off).min(zero.len() * 4) / 4;
        if !t.write_pixels(off, &zero[..k]) {
            return None;
        }
        off += k * 4;
    }
    t.flush();
    Some((n, now_ns().saturating_sub(t0)))
}

/// Microseconds per MiB for `bytes` written in `ns` (None when the clock did not move: no clock yet).
pub fn us_per_mb(bytes: usize, ns: u64) -> Option<u64> {
    if ns == 0 || bytes == 0 {
        return None;
    }
    Some(ns.saturating_mul(1 << 20) / bytes as u64 / 1_000)
}

/// CONFIG `display_refresh=<Hz>` (e.g. `59.95`, `60`, `143.981`) in millihertz; None when it is not a rate
/// between 20 and 1000 Hz with at most three decimals.
pub fn parse_refresh_mhz(v: &str) -> Option<u32> {
    let v = v.trim();
    let (int, frac) = v.split_once('.').unwrap_or((v, ""));
    if int.is_empty() || frac.len() > 3 || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hz: u32 = int.parse().ok()?;
    let mut milli = 0u32;
    for (i, d) in frac.bytes().enumerate() {
        milli += (d - b'0') as u32 * [100, 10, 1][i];
    }
    let mhz = hz.checked_mul(1000)?.checked_add(milli)?;
    (20_000..=1_000_000).contains(&mhz).then_some(mhz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RamTarget;
    use core::cell::Cell;

    #[test]
    fn refresh_values() {
        assert_eq!(parse_refresh_mhz("59.95"), Some(59_950));
        assert_eq!(parse_refresh_mhz(" 60 "), Some(60_000));
        assert_eq!(parse_refresh_mhz("143.981"), Some(143_981));
        assert_eq!(parse_refresh_mhz("59.9"), Some(59_900));
        for bad in ["", ".5", "60.1234", "-60", "6e1", "60,0", "10", "5000", "99999999999", "59.95Hz"] {
            assert_eq!(parse_refresh_mhz(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn probe_writes_black_only_inside() {
        let t = RamTarget::new(100_000, 0xAB);
        let clock = Cell::new(0u64);
        let now = || {
            clock.set(clock.get() + 5_000);
            clock.get()
        };
        assert_eq!(probe(&t, &now), Some((PROBE_BYTES, 5_000)));
        let b = t.to_vec();
        assert!(b[..PROBE_BYTES].iter().all(|&x| x == 0));
        assert!(b[PROBE_BYTES..].iter().all(|&x| x == 0xAB));
        let small = RamTarget::new(10, 1);
        assert_eq!(probe(&small, &now).map(|p| p.0), Some(8));
        assert_eq!(probe(&RamTarget::new(0, 1), &now), None);
    }

    #[test]
    fn speed_maths() {
        // V1's WC: 64 KiB in ~7.3 us = ~117 us/MB; UC-like: 64 KiB in 200 us.
        assert_eq!(us_per_mb(64 << 10, 7_300), Some(116));
        assert!(us_per_mb(64 << 10, 200_000).unwrap() > SLOW_US_PER_MB);
        assert_eq!(us_per_mb(64 << 10, 0), None);
    }
}
