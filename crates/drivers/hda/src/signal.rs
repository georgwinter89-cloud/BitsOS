//! The test signal of the WAV checks (`host/tests/sn/wavcheck.py` computes the same): 16-bit stereo, left a
//! 1 kHz sine at 48 kHz (a 48-sample table, amplitude 12000), right the frame counter (mod 2^16), so a gap, a
//! repeat or a swap shows sample-exactly. `tone` puts the sine on both sides (what Georg hears on the jack).

/// round(12000 * sin(2 pi i / 48)).
pub const SINE48: [i16; 48] = [
    0, 1566, 3106, 4592, 6000, 7305, 8485, 9520, 10392, 11087, 11591, 11897, 12000, 11897, 11591, 11087, 10392, 9520, 8485, 7305, 6000,
    4592, 3106, 1566, 0, -1566, -3106, -4592, -6000, -7305, -8485, -9520, -10392, -11087, -11591, -11897, -12000, -11897, -11591, -11087,
    -10392, -9520, -8485, -7305, -6000, -4592, -3106, -1566,
];

pub fn frame(i: u64, tone: bool) -> [i16; 2] {
    let s = SINE48[(i % 48) as usize];
    if tone { [s / 4, s / 4] } else { [s, i as u16 as i16] }
}

/// Frames `start..start+n` as little-endian bytes.
pub fn bytes(start: u64, n: usize, tone: bool) -> alloc::vec::Vec<u8> {
    let mut v = alloc::vec::Vec::with_capacity(n * 4);
    for i in 0..n as u64 {
        let [l, r] = frame(start + i, tone);
        v.extend_from_slice(&l.to_le_bytes());
        v.extend_from_slice(&r.to_le_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    extern crate std;

    #[test]
    fn table_is_the_rounded_sine() {
        for (i, &v) in super::SINE48.iter().enumerate() {
            let want = (12000.0 * (2.0 * core::f64::consts::PI * i as f64 / 48.0).sin()).round() as i16;
            assert_eq!(v, want, "{i}");
        }
        assert_eq!(super::frame(65537, false), [super::SINE48[65537 % 48], 1]);
    }
}
