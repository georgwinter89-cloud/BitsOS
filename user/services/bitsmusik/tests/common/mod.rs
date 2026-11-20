//! Files the tests make themselves (no recorded music anywhere): a sine as 16-bit PCM, written as WAV or as
//! FLAC with VERBATIM subframes (a valid stream symphonia decodes; no encoder needed), ID3v2 tags and an Ogg
//! stream's header packets.
#![allow(dead_code)]

use std::path::PathBuf;

/// A fresh directory under cargo's per-test tmp dir.
pub fn tmp(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("bitsmusik-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Interleaved 16-bit samples of a sine of `hz` (both channels the same, the second at half amplitude).
pub fn sine(rate: u32, channels: u32, secs: f64, hz: f64) -> Vec<i16> {
    let n = (rate as f64 * secs) as usize;
    let mut v = Vec::with_capacity(n * channels as usize);
    for i in 0..n {
        let s = (i as f64 * hz * std::f64::consts::TAU / rate as f64).sin() * 12000.0;
        for c in 0..channels {
            v.push(if c == 0 { s as i16 } else { (s / 2.0) as i16 });
        }
    }
    v
}

pub fn wav(rate: u32, channels: u32, pcm: &[i16]) -> Vec<u8> {
    let data = pcm.len() as u32 * 2;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&(channels as u16).to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * channels * 2).to_le_bytes());
    b.extend_from_slice(&((channels * 2) as u16).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

fn crc8(b: &[u8]) -> u8 {
    let mut c = 0u8;
    for &x in b {
        c ^= x;
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
    }
    c
}

fn crc16(b: &[u8]) -> u16 {
    let mut c = 0u16;
    for &x in b {
        c ^= (x as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
        }
    }
    c
}

/// FLAC's UTF-8-like coding of the frame number.
fn utf8(n: u32, out: &mut Vec<u8>) {
    if n < 0x80 {
        out.push(n as u8);
    } else if n < 0x800 {
        out.extend_from_slice(&[0xc0 | (n >> 6) as u8, 0x80 | (n & 0x3f) as u8]);
    } else {
        out.extend_from_slice(&[
            0xe0 | (n >> 12) as u8,
            0x80 | ((n >> 6) & 0x3f) as u8,
            0x80 | (n & 0x3f) as u8,
        ]);
    }
}

/// A FLAC stream: STREAMINFO, a VORBIS_COMMENT block of `comments` ("KEY=value"), frames of 4096 samples.
pub fn flac(rate: u32, channels: u32, pcm: &[i16], comments: &[&str]) -> Vec<u8> {
    const BS: usize = 4096;
    let frames = pcm.len() / channels as usize;
    let mut b = b"fLaC".to_vec();
    b.extend_from_slice(&[0, 0, 0, 34]);
    b.extend_from_slice(&(BS as u16).to_be_bytes());
    b.extend_from_slice(&(BS as u16).to_be_bytes());
    b.extend_from_slice(&[0; 6]);
    let packed: u64 =
        ((rate as u64) << 44) | (((channels - 1) as u64) << 41) | (15u64 << 36) | frames as u64;
    b.extend_from_slice(&packed.to_be_bytes());
    b.extend_from_slice(&[0; 16]);
    let mut vc = Vec::new();
    vc.extend_from_slice(&4u32.to_le_bytes());
    vc.extend_from_slice(b"test");
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    b.push(0x80 | 4);
    b.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
    b.extend_from_slice(&vc);
    let rate_code = match rate {
        8000 => 4,
        16000 => 5,
        22050 => 6,
        24000 => 7,
        32000 => 8,
        44100 => 9,
        48000 => 10,
        96000 => 11,
        _ => 0,
    };
    for (n, start) in (0..frames).step_by(BS).enumerate() {
        let len = BS.min(frames - start);
        let mut f = vec![0xff, 0xf8];
        f.push(if len == BS { 12 << 4 } else { 7 << 4 } | rate_code);
        f.push((((channels - 1) as u8) << 4) | (4 << 1));
        utf8(n as u32, &mut f);
        if len != BS {
            f.extend_from_slice(&((len - 1) as u16).to_be_bytes());
        }
        f.push(crc8(&f));
        for c in 0..channels as usize {
            f.push(0x02);
            for i in 0..len {
                f.extend_from_slice(&pcm[(start + i) * channels as usize + c].to_be_bytes());
            }
        }
        let crc = crc16(&f);
        f.extend_from_slice(&crc.to_be_bytes());
        b.extend_from_slice(&f);
    }
    b
}

fn syncsafe(n: usize) -> [u8; 4] {
    [
        (n >> 21) as u8 & 0x7f,
        (n >> 14) as u8 & 0x7f,
        (n >> 7) as u8 & 0x7f,
        n as u8 & 0x7f,
    ]
}

/// An ID3v2 tag (`major` 2, 3 or 4) of text frames `(id, encoding, bytes)`, with `pad` bytes of padding.
pub fn id3(major: u8, frames: &[(&str, u8, &[u8])], pad: usize) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, enc, text) in frames {
        let len = text.len() + 1;
        body.extend_from_slice(id.as_bytes());
        match major {
            2 => body.extend_from_slice(&(len as u32).to_be_bytes()[1..]),
            3 => {
                body.extend_from_slice(&(len as u32).to_be_bytes());
                body.extend_from_slice(&[0, 0]);
            }
            _ => {
                body.extend_from_slice(&syncsafe(len));
                body.extend_from_slice(&[0, 0]);
            }
        }
        body.push(*enc);
        body.extend_from_slice(text);
    }
    body.extend(std::iter::repeat_n(0u8, pad));
    let mut b = b"ID3".to_vec();
    b.extend_from_slice(&[major, 0, 0]);
    b.extend_from_slice(&syncsafe(body.len()));
    b.extend_from_slice(&body);
    b
}

fn ogg_page(serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
    let mut lacing: Vec<u8> = vec![255; packet.len() / 255];
    lacing.push((packet.len() % 255) as u8);
    let mut p = b"OggS".to_vec();
    p.extend_from_slice(&[0, if seq == 0 { 2 } else { 0 }]);
    p.extend_from_slice(&0u64.to_le_bytes());
    p.extend_from_slice(&serial.to_le_bytes());
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(lacing.len() as u8);
    p.extend_from_slice(&lacing);
    p.extend_from_slice(packet);
    p
}

/// The first two pages of an Ogg Vorbis stream: identification header and comment header.
pub fn ogg_vorbis_head(comments: &[&str]) -> Vec<u8> {
    let mut id = b"\x01vorbis".to_vec();
    id.extend_from_slice(&[0; 23]);
    let mut c = b"\x03vorbis".to_vec();
    c.extend_from_slice(&4u32.to_le_bytes());
    c.extend_from_slice(b"test");
    c.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for x in comments {
        c.extend_from_slice(&(x.len() as u32).to_le_bytes());
        c.extend_from_slice(x.as_bytes());
    }
    c.push(1);
    let mut b = ogg_page(7, 0, &id);
    b.extend(ogg_page(7, 1, &c));
    b
}

/// A small xorshift for the cut-and-garbage tests (fixed seed: the same bytes each run).
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
