//! PNG encoder for MCP `screen` (V2-PLAN §6): 8-bit RGB, one IDAT, zlib by `deflate.rs` (why not miniz_oxide's
//! deflate: see there; miniz_oxide's inflate reads the PNG back in the tests).
//! Rows are filtered and compressed as they come, so only the compressed picture is ever held in full. Scalar on
//! purpose (R10: no SSE/AVX in the kernel; a vector filter belongs in hal's guarded SIMD sections, KS-1).
//!
//! Filter: per row the better of Sub and Up (the smaller sum of bytes, as signed distance from zero, libpng's
//! heuristic restricted to the two filters that turn a screen's flat areas and repeated rows into zeros).
//! A console picture compresses to a few per cent of its raw size.

use crate::deflate::Deflater;
use alloc::vec;
use alloc::vec::Vec;

/// Filtered rows are handed to the compressor in pieces of about this size (fewer calls, small buffer).
const BATCH: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PngError {
    /// The compressed picture grew past `limit` bytes after `rows` of the rows.
    TooBig { rows: u32, bytes: usize },
    /// Zero or absurd size (the caller checks first; never a panic).
    BadSize,
    /// The heap could not hold the encoder's buffers or the growing PNG (every allocation here is fallible, so a
    /// large `screen` on a short heap is an answer, not the kernel's allocation-failure path).
    NoMemory,
}

/// `Vec` with room for `n` bytes, or NoMemory.
fn buffer(n: usize) -> Result<Vec<u8>, PngError> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| PngError::NoMemory)?;
    Ok(v)
}

fn reserve(v: &mut Vec<u8>, n: usize) -> Result<(), PngError> {
    v.try_reserve(n).map_err(|_| PngError::NoMemory)
}

/// CRC-32 (ISO-HDLC, as PNG wants), table built at compile time.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
};

pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for p in parts {
        for &b in *p {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFF_FFFF
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[ty, data]).to_be_bytes());
}

/// Writes the filter byte and the filtered row into `dst` (`1 + cur.len()` bytes). `prev` is the row above
/// (zeros for the first row).
fn filter_row(cur: &[u8], prev: &[u8], dst: &mut Vec<u8>) {
    // Sum of |byte as i8| for Sub and Up, the cheap estimate of how well the row compresses.
    let (mut sub, mut up) = (0u64, 0u64);
    for i in 0..cur.len() {
        let left = if i >= 3 { cur[i - 3] } else { 0 };
        sub += (cur[i].wrapping_sub(left) as i8).unsigned_abs() as u64;
        up += (cur[i].wrapping_sub(prev[i]) as i8).unsigned_abs() as u64;
    }
    if up <= sub {
        dst.push(2);
        dst.extend(cur.iter().zip(prev).map(|(c, p)| c.wrapping_sub(*p)));
    } else {
        dst.push(1);
        dst.extend_from_slice(&cur[..cur.len().min(3)]);
        dst.extend(cur.iter().skip(3).zip(cur.iter()).map(|(c, l)| c.wrapping_sub(*l)));
    }
}

/// Encodes a `width` x `height` RGB picture. `row(y, buf)` fills `buf` (3 * width bytes) with row `y`. Fails with
/// `TooBig` as soon as the PNG would exceed `limit` bytes (so a hopeless picture costs little time).
pub fn encode_rgb(width: u32, height: u32, limit: usize, mut row: impl FnMut(u32, &mut [u8])) -> Result<Vec<u8>, PngError> {
    let stride = (width as usize).checked_mul(3).ok_or(PngError::BadSize)?;
    if width == 0 || height == 0 || width > 1 << 20 || height > 1 << 20 {
        return Err(PngError::BadSize);
    }
    let mut out = buffer((stride * height as usize / 32).clamp(4096, limit.max(4096)))?;
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8; // bit depth
    ihdr[9] = 2; // colour type RGB; compression, filter, interlace 0
    chunk(&mut out, b"IHDR", &ihdr);
    // IDAT length and type now, its length patched at the end (one chunk: no copy of the compressed data).
    let idat_at = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(b"IDAT");

    // A piece is at most one batch plus one filtered row (see the loop).
    let piece = BATCH + stride + 1;
    let mut z = Deflater::try_new(piece).ok_or(PngError::NoMemory)?;
    let room = limit.saturating_sub(12 + 4);
    let too_big = |out: &Vec<u8>, rows: u32| if out.len() > room { Err(PngError::TooBig { rows, bytes: out.len() }) } else { Ok(()) };

    let mut cur = buffer(stride)?;
    cur.resize(stride, 0);
    let mut prev = buffer(stride)?;
    prev.resize(stride, 0);
    let mut batch = buffer(piece)?;
    for y in 0..height {
        row(y, &mut cur);
        filter_row(&cur, &prev, &mut batch);
        core::mem::swap(&mut cur, &mut prev);
        if batch.len() >= BATCH {
            reserve(&mut out, Deflater::worst_case(batch.len()))?;
            z.write(&mut out, &batch);
            batch.clear();
            too_big(&out, y + 1)?;
        }
    }
    // The last piece, the end of the stream (end code, Adler-32), the IDAT CRC and IEND.
    reserve(&mut out, Deflater::worst_case(batch.len()) + 64)?;
    z.write(&mut out, &batch);
    z.finish(&mut out);
    too_big(&out, height)?;
    let idat_len = out.len() - idat_at - 8;
    out[idat_at..idat_at + 4].copy_from_slice(&(idat_len as u32).to_be_bytes());
    let crc = crc32(&[&out[idat_at + 4..]]);
    out.extend_from_slice(&crc.to_be_bytes());
    chunk(&mut out, b"IEND", &[]);
    if out.len() > limit {
        return Err(PngError::TooBig { rows: height, bytes: out.len() });
    }
    Ok(out)
}

/// 0x00RRGGBB pixels (the RAM frame's format) to RGB bytes.
pub fn xrgb_to_rgb(src: &[u32], dst: &mut [u8]) {
    for (p, d) in src.iter().zip(dst.as_chunks_mut::<3>().0) {
        d[0] = (p >> 16) as u8;
        d[1] = (p >> 8) as u8;
        d[2] = *p as u8;
    }
}

/// Reads a PNG as `encode_rgb` writes it (8-bit RGB, no interlace) back into RGB bytes: chunks and CRCs
/// checked, zlib through miniz's inflate, filters 0-4. For the tests (host and ktest), never on input from
/// outside; still never panics.
pub fn decode_rgb(png: &[u8]) -> Result<(u32, u32, Vec<u8>), &'static str> {
    if png.get(..8) != Some(b"\x89PNG\r\n\x1a\n".as_slice()) {
        return Err("no PNG signature");
    }
    let (mut at, mut w, mut h, mut idat) = (8usize, 0u32, 0u32, Vec::new());
    loop {
        let be = |i: usize| png.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        let len = be(at).ok_or("cut off")? as usize;
        let ty = png.get(at + 4..at + 8).ok_or("cut off")?;
        let data = png.get(at + 8..at + 8 + len).ok_or("cut off")?;
        if be(at + 8 + len).ok_or("cut off")? != crc32(&[ty, data]) {
            return Err("bad chunk CRC");
        }
        match ty {
            b"IHDR" if len == 13 => {
                (w, h) = (be(at + 8).unwrap_or(0), be(at + 12).unwrap_or(0));
                if data[8..] != [8, 2, 0, 0, 0] {
                    return Err("not 8-bit RGB without interlace");
                }
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => return Err("unexpected chunk"),
        }
        at += 12 + len;
    }
    if at + 12 != png.len() {
        return Err("data after IEND");
    }
    if w == 0 || h == 0 || w > 1 << 20 || h > 1 << 20 {
        return Err("bad size");
    }
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).map_err(|_| "bad zlib stream")?;
    let stride = w as usize * 3;
    if raw.len() != (stride + 1) * h as usize {
        return Err("wrong amount of picture data");
    }
    let mut px = vec![0u8; stride * h as usize];
    for y in 0..h as usize {
        let f = raw[y * (stride + 1)];
        for x in 0..stride {
            let v = raw[y * (stride + 1) + 1 + x];
            let a = if x >= 3 { px[y * stride + x - 3] as i16 } else { 0 };
            let b = if y > 0 { px[(y - 1) * stride + x] as i16 } else { 0 };
            let c = if x >= 3 && y > 0 { px[(y - 1) * stride + x - 3] as i16 } else { 0 };
            let pred = match f {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
                }
                _ => return Err("unknown filter"),
            };
            px[y * stride + x] = v.wrapping_add(pred as u8);
        }
    }
    Ok((w, h, px))
}

#[cfg(test)]
pub(crate) mod tests {
    extern crate std;
    use super::*;

    pub(crate) fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        decode_rgb(png).expect("a valid PNG")
    }

    fn picture(w: u32, h: u32) -> Vec<u32> {
        let mut seed = 0x1234_5678u32;
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if (x / 7 + y / 5) % 3 == 0 {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    seed >> 8
                } else {
                    0x0010_1418 + (y / 16) * 0x10
                }
            })
            .collect()
    }

    #[test]
    fn crc_of_known_text() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"IEND"]), 0xAE42_6082);
    }

    #[test]
    fn round_trip_exact_pixels() {
        for (w, h) in [(1, 1), (2, 3), (17, 9), (640, 33), (300, 400)] {
            let px = picture(w, h);
            let png = encode_rgb(w, h, usize::MAX, |y, buf| xrgb_to_rgb(&px[(y * w) as usize..((y + 1) * w) as usize], buf)).unwrap();
            let (dw, dh, rgb) = decode(&png);
            assert_eq!((dw, dh), (w, h));
            for (i, p) in px.iter().enumerate() {
                assert_eq!([rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]], [(p >> 16) as u8, (p >> 8) as u8, *p as u8], "{w}x{h} pixel {i}");
            }
        }
    }

    #[test]
    fn too_big_stops_early_and_bad_sizes_are_errors() {
        let (w, h) = (512, 512);
        let mut seed = 7u32;
        let noise: Vec<u32> = (0..w * h).map(|_| { seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); seed }).collect();
        let r = encode_rgb(w, h, 64 * 1024, |y, buf| xrgb_to_rgb(&noise[(y * w) as usize..((y + 1) * w) as usize], buf));
        match r {
            Err(PngError::TooBig { rows, bytes }) => assert!(rows < h && bytes <= 64 * 1024 + 2 * BATCH, "{rows} {bytes}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(encode_rgb(0, 5, usize::MAX, |_, _| {}), Err(PngError::BadSize));
        assert_eq!(encode_rgb(5, 0, usize::MAX, |_, _| {}), Err(PngError::BadSize));
        assert_eq!(encode_rgb(u32::MAX, 1, usize::MAX, |_, _| {}), Err(PngError::BadSize));
        let png = encode_rgb(3, 2, usize::MAX, |_, b| b.fill(7)).unwrap();
        assert!(decode_rgb(&png[..png.len() - 1]).is_err() && decode_rgb(b"nope").is_err());
        let mut broken = png.clone();
        broken[40] ^= 1;
        assert!(decode_rgb(&broken).is_err());
    }

    /// Time and size of a 2560x1440 console-like picture on the host (release: `cargo test -p mcp --release
    /// png_1440p -- --ignored --nocapture`). The Mini-PC number is the desk's (check list).
    #[test]
    #[ignore]
    fn png_1440p_timing() {
        let (w, h) = (2560u32, 1440u32);
        let font = include_bytes!("../../display/src/font/cp437-8x16.bin");
        let mut px = vec![0x0010_1418u32; (w * h) as usize];
        let text = b"[    1.234567] mcp http.request peer=10.0.2.2 what=\"POST /mcp\" status=200 bytes=176 ms=3 rpc=tools/call tool=screen";
        for row in 0..(h / 32) {
            for (col, &c) in text.iter().cycle().skip(row as usize * 7).take(150).enumerate() {
                for gy in 0..16u32 {
                    let bits = font[c as usize * 16 + gy as usize];
                    for gx in 0..8u32 {
                        if bits & (0x80 >> gx) != 0 {
                            for s in 0..4 {
                                let (x, y) = (col as u32 * 16 + gx * 2 + (s & 1), row * 32 + gy * 2 + (s >> 1));
                                px[(y * w + x) as usize] = if row % 5 == 0 { 0x00FF_C040 } else { 0x00C8_C8C8 };
                            }
                        }
                    }
                }
            }
        }
        let t = std::time::Instant::now();
        let png = encode_rgb(w, h, usize::MAX, |y, buf| xrgb_to_rgb(&px[(y * w) as usize..((y + 1) * w) as usize], buf)).unwrap();
        std::println!("1440p console picture: {} bytes in {:?}", png.len(), t.elapsed());
        let (_, _, rgb) = decode(&png);
        assert_eq!(rgb.len(), (w * h * 3) as usize);
    }
}
