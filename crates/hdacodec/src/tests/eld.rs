//! SN-5: the sink from the monitor's EDID and the endpoint values Linux's `dce_aud_az_configure` makes of it.

use std::vec;
use std::vec::Vec;

use crate::eld::{AzConfig, DEFAULT_SPEAKERS, EdidError, Sad, Sink};

fn base(name: Option<&[u8]>, exts: u8) -> Vec<u8> {
    let mut e = vec![0u8; 128];
    e[..8].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
    e[8] = 0x4C; // "SAM"
    e[9] = 0x2D;
    e[10] = 0x34;
    e[11] = 0x12;
    if let Some(n) = name {
        e[90..95].copy_from_slice(&[0, 0, 0, 0xFC, 0]);
        let mut text = [0x20u8; 13];
        text[..n.len()].copy_from_slice(n);
        if n.len() < 13 {
            text[n.len()] = 0x0A;
        }
        e[95..108].copy_from_slice(&text);
    }
    e[126] = exts;
    fix(&mut e);
    e
}

fn fix(block: &mut [u8]) {
    let cs = block[..127].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    block[127] = 0u8.wrapping_sub(cs);
}

fn cea(rev: u8, blocks: &[&[u8]]) -> Vec<u8> {
    let mut x = vec![0u8; 128];
    x[0] = 2;
    x[1] = rev;
    let mut i = 4;
    for b in blocks {
        x[i..i + b.len()].copy_from_slice(b);
        i += b.len();
    }
    x[2] = i as u8;
    fix(&mut x);
    x
}

fn edid(name: Option<&[u8]>, exts: &[Vec<u8>]) -> Vec<u8> {
    let mut e = base(name, exts.len() as u8);
    for x in exts {
        e.extend_from_slice(x);
    }
    e
}

#[test]
fn identity_name_and_audio() {
    let audio: &[u8] = &[0x26, 0x09, 0x07, 0x01, 0x0D, 0x7F, 0x07]; // LPCM 2 ch 32-48 16 bit; LPCM 6 ch all rates 16-24 bit
    let spk: &[u8] = &[0x83, 0x4F, 0, 0];
    let vsdb: &[u8] = &[0x6A, 0x03, 0x0C, 0x00, 0x10, 0x00, 0x00, 0x00, 0x80, 0x15, 0x0B]; // latency present: 0x15 0x0B
    let s = Sink::from_edid(&edid(Some(b"SAMSUNG"), &[cea(3, &[audio, spk, vsdb])])).unwrap();
    assert_eq!(s.manufacturer_id, 0x2D4C);
    assert_eq!(s.product_id, 0x1234);
    assert_eq!(s.name, b"SAMSUNG");
    assert_eq!(s.sads, [Sad { format: 1, channels: 1, freq: 7, byte2: 1 }, Sad { format: 1, channels: 5, freq: 0x7F, byte2: 7 }]);
    assert_eq!(s.speakers, 0x4F);
    assert_eq!((s.video_latency, s.audio_latency), (Some(0x15), Some(0x0B)));
    assert!(!s.bad_checksum);
    let az = AzConfig::from_sink(&s);
    // Of the two LPCM entries the one with more channels (is_audio_format_supported).
    assert_eq!(az.descriptors[0], 5 | 0x7F << 8 | 7 << 16 | 0x7F << 24);
    assert!(az.descriptors[1..].iter().all(|&d| d == 0));
    assert_eq!(az.speakers, 0x4F);
    assert_eq!((az.video_lipsync, az.audio_lipsync), (0x15, 0x0B));
    assert_eq!(az.sink_info[0], 0x1234_2D4C);
    assert_eq!(az.sink_info[1], 8, "the name's length + the terminating zero");
    assert_eq!(az.sink_info[4], u32::from_le_bytes(*b"SAMS"));
    assert_eq!(az.sink_info[5], u32::from_le_bytes(*b"UNG\0"));
    assert_eq!(&az.sink_info[6..], &[0, 0, 0]);
}

#[test]
fn defaults_and_linux_rules() {
    // No speaker allocation block: DEFAULT_SPEAKER_LOCATION; no SAD at all: speaker flags stay 0.
    let audio: &[u8] = &[0x23, 0x09, 0x07, 0x01];
    let s = Sink::from_edid(&edid(None, &[cea(3, &[audio])])).unwrap();
    assert_eq!(s.speakers, DEFAULT_SPEAKERS);
    assert!(s.name.is_empty());
    assert_eq!(AzConfig::from_sink(&s).sink_info[1], 1);
    let s = Sink::from_edid(&edid(None, &[])).unwrap();
    assert!(s.sads.is_empty() && s.speakers == 0 && !s.has_lpcm());
    // A CEA extension before revision 3 carries no data blocks.
    let s = Sink::from_edid(&edid(None, &[cea(2, &[audio])])).unwrap();
    assert!(s.sads.is_empty());
    // Only the first audio data block counts (_drm_edid_to_sad breaks after it).
    let second: &[u8] = &[0x23, 0x15, 0x07, 0x50];
    let s = Sink::from_edid(&edid(None, &[cea(3, &[audio, second])])).unwrap();
    assert_eq!(s.sads.len(), 1);
    // 13 name characters, no line feed: all kept.
    let s = Sink::from_edid(&edid(Some(b"ABCDEFGHIJKLM"), &[])).unwrap();
    assert_eq!(s.name, b"ABCDEFGHIJKLM");
    assert_eq!(AzConfig::from_sink(&s).sink_info[1], 14);
    // AC-3: channels and rates, byte 2 0 (amdgpu never fills max_bit_rate), no stereo rates.
    let ac3: &[u8] = &[0x26, 0x09, 0x07, 0x01, 0x15, 0x07, 0x50];
    let az = AzConfig::from_sink(&Sink::from_edid(&edid(None, &[cea(3, &[ac3])])).unwrap());
    assert_eq!(az.descriptors[1], 5 | 7 << 8);
    // A bad checksum is noted, not refused (Linux goes on with EDID_BAD_CHECKSUM).
    let mut e = edid(Some(b"X"), &[]);
    e[20] ^= 1;
    assert!(Sink::from_edid(&e).unwrap().bad_checksum);
}

#[test]
fn generic_sink() {
    let g = Sink::generic();
    assert!(g.has_lpcm());
    let az = AzConfig::from_sink(&g);
    assert_eq!(az.descriptors[0], 0x0701_0701, "what V1 wrote for the measured sink");
    assert_eq!(az.speakers, 1);
    assert_eq!(az.sink_info, [0, 1, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn hostile_edids_never_panic() {
    assert_eq!(Sink::from_edid(&[]), Err(EdidError::Short));
    assert_eq!(Sink::from_edid(&[0; 128]), Err(EdidError::Header));
    assert_eq!(Sink::from_edid(&[0xFF; 128]), Err(EdidError::Header));
    // 255 extensions announced, one half there; a data block longer than its collection; d out of range.
    let mut e = base(Some(b"abc"), 255);
    let mut x = cea(3, &[&[0x3F, 1, 2]]);
    x[2] = 6;
    e.extend_from_slice(&x);
    e.extend_from_slice(&[2, 3, 200, 0]);
    let s = Sink::from_edid(&e).unwrap();
    assert!(s.sads.is_empty());
    let mut y = cea(3, &[]);
    y[2] = 255;
    let _ = Sink::from_edid(&edid(None, &[y]));
    let mut z = cea(3, &[]);
    z[2] = 0;
    let _ = Sink::from_edid(&edid(None, &[z]));
    // Pseudo-random bodies behind a valid header.
    let mut seed = 0x1234_5678u32;
    for _ in 0..2000 {
        let mut b = edid(None, &[cea(3, &[])]);
        for v in b[8..].iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = (seed >> 24) as u8;
        }
        b[128] = 2;
        b[129] = 3;
        if let Ok(s) = Sink::from_edid(&b) {
            let az = AzConfig::from_sink(&s);
            assert!(az.sink_info[1] <= 18);
            assert!(s.name.len() <= 13);
        }
    }
}
