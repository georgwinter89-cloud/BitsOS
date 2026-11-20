//! A picture as PNG (RGB, 8 bits, zlib through miniz_oxide) or as the PPM bitscomp writes.

/// PPM P6 with maxval 255: (width, height, the RGB bytes).
pub fn read_ppm(data: &[u8]) -> Result<(u32, u32, &[u8]), String> {
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while i < data.len() && data[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < data.len() && !data[i].is_ascii_whitespace() {
            i += 1;
        }
        if start == i {
            return Err("kein vollständiges PPM".into());
        }
        fields.push(String::from_utf8_lossy(&data[start..i]).to_string());
    }
    let n = |k: usize| fields[k].parse::<u32>().map_err(|_| "PPM: keine Zahl".to_string());
    if fields[0] != "P6" || n(3)? != 255 {
        return Err("kein PPM (P6, 255)".into());
    }
    let (w, h) = (n(1)?, n(2)?);
    let px = &data[i + 1..];
    if px.len() < (w as usize) * (h as usize) * 3 {
        return Err("PPM: zu wenige Bildpunkte".into());
    }
    Ok((w, h, &px[..(w as usize) * (h as usize) * 3]))
}

fn crc32(chunks: &[&[u8]]) -> u32 {
    let mut c: u32 = 0xffff_ffff;
    for d in chunks {
        for &b in *d {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

pub fn png(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let row = w as usize * 3;
    let mut raw = Vec::with_capacity((row + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * row..(y + 1) * row]);
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6));
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ppm_becomes_a_png_with_the_same_pixels() {
        let ppm = b"P6\n2 1\n255\n\x01\x02\x03\x04\x05\x06".to_vec();
        let (w, h, px) = read_ppm(&ppm).unwrap();
        assert_eq!((w, h, px), (2, 1, &[1u8, 2, 3, 4, 5, 6][..]));
        let p = png(w, h, px);
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(crc32(&[b"IEND"]), 0xae42_6082, "the well-known CRC of an empty IEND");
        let idat_len = u32::from_be_bytes(p[33..37].try_into().unwrap()) as usize;
        let back = miniz_oxide::inflate::decompress_to_vec_zlib(&p[41..41 + idat_len]).unwrap();
        assert_eq!(back, vec![0, 1, 2, 3, 4, 5, 6]);
    }
}
