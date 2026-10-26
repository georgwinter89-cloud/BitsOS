//! Test signals of the mixer WAV checks (`host/tests/sn/mixcheck.py` computes the same), host tests and ktests.
//! `a`: left a 1 kHz sine of amplitude 24000, right 1..=32767 counting (frame 0 is not silent). `b`: left a
//! hash noise in -16000..=16000, right 0: `a + 0.75 b` exceeds 16 bit, so saturation shows. `c` (switching):
//! left the 12000 sine, right the frame number + 1 (mod 2^16): a gap or repeat across outputs shows exactly.
//! `tone441`: a 1 kHz sine of amplitude 16000 at 44.1 kHz (the resampler's input).

use crate::fmath;
use hda::signal::SINE48;

pub fn a(i: u64) -> [i16; 2] {
    [SINE48[(i % 48) as usize] * 2, (i % 32767 + 1) as i16]
}

pub fn b(i: u64) -> [i16; 2] {
    let h = ((i as u32).wrapping_mul(2_654_435_761) >> 16) % 32001;
    [(h as i32 - 16000) as i16, 0]
}

pub fn c(i: u64) -> [i16; 2] {
    [SINE48[(i % 48) as usize], ((i + 1) & 0xFFFF) as u16 as i16]
}

pub fn tone441(i: u64) -> [i16; 2] {
    let s = fmath::floor(16000.0 * fmath::sin_pi((i % 441) as f64 / 22.05) + 0.5) as i16;
    [s, s]
}

/// What the mixer must make of `a(i)` at gain `ga` and `b(i)` at `gb` (Q15), master `m`.
pub fn mixed(i: u64, ga: u32, gb: u32, m: [u32; 2]) -> [i16; 2] {
    let (x, y) = (a(i), b(i));
    let mut o = [0i16; 2];
    for ch in 0..2 {
        let s = ((x[ch] as i64 * ga as i64 + 0x4000) >> 15) + ((y[ch] as i64 * gb as i64 + 0x4000) >> 15);
        o[ch] = ((s * m[ch] as i64 + 0x4000) >> 15).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
    }
    o
}

pub fn frames(f: fn(u64) -> [i16; 2], start: u64, n: usize) -> alloc::vec::Vec<[i16; 2]> {
    (start..start + n as u64).map(f).collect()
}
