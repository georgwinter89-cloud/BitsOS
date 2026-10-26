//! PCM formats: the 16-bit format word for converters and stream descriptors (Linux
//! `snd_hdac_calc_stream_format`, `linux-6.6/sound/hda/hdac_device.c:700-790`) and the formats a converter's
//! PCM caps (parameter 0x0A) allow.

use alloc::vec::Vec;
use devmodel::audio::PcmFormat;

/// (rate, base 44.1 kHz, multiplier, divisor) in the order of the PCM-caps rate bits 0..11.
const RATES: [(u32, bool, u16, u16); 12] = [
    (8000, false, 1, 6),
    (11025, true, 1, 4),
    (16000, false, 1, 3),
    (22050, true, 1, 2),
    (32000, false, 2, 3),
    (44100, true, 1, 1),
    (48000, false, 1, 1),
    (88200, true, 2, 1),
    (96000, false, 2, 1),
    (176400, true, 4, 1),
    (192000, false, 4, 1),
    (384000, false, 8, 1),
];

/// Sample sizes and their PCM-caps bit (16 = 8 bit, 17 = 16, 18 = 20, 19 = 24, 20 = 32).
const BITS: [(u8, u32, u16); 3] = [(16, 17, 1), (24, 19, 3), (32, 20, 4)];

/// The only channel count the paths of `hdacodec` set up (stereo converters, channel 0 and 1).
pub const CHANNELS: u8 = 2;

/// The format word, or None for a format HDA cannot carry or we do not play.
pub fn format_word(f: PcmFormat) -> Option<u16> {
    let &(_, base44, mult, div) = RATES.iter().find(|r| r.0 == f.rate)?;
    let &(_, _, code) = BITS.iter().find(|b| b.0 == f.bits)?;
    if f.channels != CHANNELS {
        return None;
    }
    Some(if base44 { 1 << 14 } else { 0 } | (mult - 1) << 11 | (div - 1) << 8 | code << 4 | (f.channels as u16 - 1))
}

/// Bytes per frame in memory (24-bit samples sit in 32-bit containers, as HDA reads them).
pub fn frame_bytes(f: PcmFormat) -> usize {
    let s = if f.bits == 16 { 2 } else { 4 };
    s * f.channels as usize
}

/// Every format a converter with these PCM caps takes, 48 kHz/16 bit first (what SN-3's mixer plays).
pub fn formats(pcm_caps: u32) -> Vec<PcmFormat> {
    let mut v = Vec::new();
    for &(bits, bit, _) in &BITS {
        if pcm_caps & 1 << bit == 0 {
            continue;
        }
        for (i, &(rate, ..)) in RATES.iter().enumerate() {
            if pcm_caps & 1 << i != 0 {
                v.push(PcmFormat { rate, channels: CHANNELS, bits });
            }
        }
    }
    v.sort_by_key(|f| (f.rate != 48000 || f.bits != 16, f.bits, f.rate));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_match_linux() {
        let f = |rate, bits| format_word(PcmFormat { rate, channels: 2, bits });
        // V1's SND_FORMAT and Linux's HDA_RATE(48,1,1) | BITS_16 | 1.
        assert_eq!(f(48000, 16), Some(0x0011));
        assert_eq!(f(44100, 16), Some(0x4011));
        assert_eq!(f(96000, 24), Some(0x0831));
        assert_eq!(f(8000, 16), Some(0x0511));
        assert_eq!(f(32000, 32), Some(0x0A41));
        assert_eq!(f(12345, 16), None);
        assert_eq!(f(48000, 8), None);
        assert_eq!(format_word(PcmFormat { rate: 48000, channels: 1, bits: 16 }), None);
    }

    #[test]
    fn caps_to_formats() {
        // ALC269 group caps 0x000E0560: 16/20/24 bit at 44.1, 48, 96, 192 kHz.
        let v = formats(0x000E_0560);
        assert_eq!(v[0], PcmFormat { rate: 48000, channels: 2, bits: 16 });
        assert!(v.contains(&PcmFormat { rate: 44100, channels: 2, bits: 16 }));
        assert!(v.contains(&PcmFormat { rate: 96000, channels: 2, bits: 24 }));
        assert!(!v.iter().any(|f| f.bits == 32));
        assert_eq!(v.len(), 8);
        assert!(!v.iter().any(|f| f.rate == 88200));
        assert!(formats(0).is_empty());
        assert_eq!(frame_bytes(PcmFormat { rate: 48000, channels: 2, bits: 24 }), 8);
    }
}
