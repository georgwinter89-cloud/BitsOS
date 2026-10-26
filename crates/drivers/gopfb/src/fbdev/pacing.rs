//! DP-3: present-pacing info for fbdev programs (`FBIO_BITSOS_PACING`). Linux's fbdev has no way to tell a
//! program the exact refresh or when a vblank happened (DRM's emulation zeroes the timings, FBIOGET_VBLANK is
//! ENOTTY there); DRM programs get both from `drmWaitVBlank`'s reply and the mode's clock. V2 answers one BitsOS
//! ioctl in the fbdev number space that Linux leaves unused ('F' 0xB0; linux-6.6/Documentation/userspace-api/
//! ioctl/ioctl-number.rst lists 'F' 00-0F, 20, 80-8F, DD), so the same program runs on Linux (ENOTTY there: it
//! measures instead) and on V2 (the kernel's own numbers). Pure: the layout and its encoding, host-tested.
//!
//! ```c
//! struct bitsos_fb_pacing {      /* 64 bytes, all little-endian, times on CLOCK_MONOTONIC */
//!     __u32 version;             /* 1 */
//!     __u32 flags;               /* BITSOS_PACING_RATE_KNOWN | BITSOS_PACING_VBLANK_ESTIMATED */
//!     __u32 refresh_mhz;         /* the scanout's rate in millihertz (60000 while unknown) */
//!     __u32 source;              /* 0 default 60 Hz, 1 CONFIG display_refresh, 2 EDID, 3 hardware vblank */
//!     __u64 period_ns;           /* one frame (exact to the ns with EDID) */
//!     __u64 vblank_seq;          /* the last vblank at or before the call */
//!     __u64 vblank_ns;           /* its time */
//!     __u64 next_vblank_ns;      /* the next one (FBIO_WAITFORVSYNC returns at it) */
//!     __u64 present_seq;         /* presents of fb0's picture since boot (pan, mode, deferred-io flush) */
//!     __u64 present_ns;          /* when the last one reached the scanout; 0 = none yet */
//! };
//! #define FBIO_BITSOS_PACING _IOR('F', 0xB0, struct bitsos_fb_pacing)
//! ```

/// `_IOR('F', 0xB0, struct bitsos_fb_pacing)`: read direction (2 << 30), size 64 << 16, type 'F', number 0xB0.
pub const FBIO_BITSOS_PACING: u64 = 0x8040_46B0;
pub const SIZE: usize = 64;
pub const VERSION: u32 = 1;
/// The rate came from EDID, CONFIG or the display engine (not the 60 Hz default).
pub const RATE_KNOWN: u32 = 1;
/// The vblank times are a timer estimate (GOP has no vblank): pacing only, tearing possible.
pub const VBLANK_ESTIMATED: u32 = 2;

/// The flags for a vblank of `source`: the rate is known unless it is the default, the times are an estimate
/// unless a display engine gives them (GX-3).
pub fn flags(source: display::RefreshSource) -> u32 {
    let mut f = 0;
    if source != display::RefreshSource::Default {
        f |= RATE_KNOWN;
    }
    if source != display::RefreshSource::Hardware {
        f |= VBLANK_ESTIMATED;
    }
    f
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pacing {
    pub flags: u32,
    pub refresh_mhz: u32,
    pub source: u32,
    pub period_ns: u64,
    pub vblank_seq: u64,
    pub vblank_ns: u64,
    pub next_vblank_ns: u64,
    pub present_seq: u64,
    pub present_ns: u64,
}

impl Pacing {
    pub fn to_bytes(&self) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        for (i, w) in [VERSION, self.flags, self.refresh_mhz, self.source].iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        let q = [self.period_ns, self.vblank_seq, self.vblank_ns, self.next_vblank_ns, self.present_seq, self.present_ns];
        for (i, w) in q.iter().enumerate() {
            b[16 + i * 8..24 + i * 8].copy_from_slice(&w.to_le_bytes());
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_and_layout() {
        // _IOC(dir, type, nr, size) = dir << 30 | size << 16 | type << 8 | nr (include/uapi/asm-generic/ioctl.h).
        assert_eq!(FBIO_BITSOS_PACING, 2 << 30 | (SIZE as u64) << 16 | (b'F' as u64) << 8 | 0xB0);
        let p = Pacing {
            flags: RATE_KNOWN | VBLANK_ESTIMATED,
            refresh_mhz: 59_951,
            source: 2,
            period_ns: 16_680_414,
            vblank_seq: 7,
            vblank_ns: 0x0102_0304_0506_0708,
            next_vblank_ns: 9,
            present_seq: 10,
            present_ns: 11,
        };
        let b = p.to_bytes();
        assert_eq!(&b[..16], &[1, 0, 0, 0, 3, 0, 0, 0, 0x2f, 0xea, 0, 0, 2, 0, 0, 0]);
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 16_680_414);
        assert_eq!(&b[32..40], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(u64::from_le_bytes(b[56..64].try_into().unwrap()), 11);
    }
}
