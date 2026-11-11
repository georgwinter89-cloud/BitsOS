//! PK-L1a: the library's PNG decoder (PNG spec, 3rd edition: chunks, CRC-32, filters, colour types) on miniz_oxide's
//! inflate, and the box filter that brings an icon to the tile size in premultiplied ARGB (Pixman's a8r8g8b8). Every
//! colour type and bit depth, no Adam7 interlace (such a file gets the fallback icon); at most `MAX_SIDE` per side.
//! A broken or hostile file is refused with a reason, never a panic.

pub const MAX_SIDE: u32 = 1024;
pub const MAX_FILE: u64 = 4 * 1024 * 1024;
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Straight (not premultiplied) RGBA, 8 bits per channel, rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

/// An icon at its drawn size: premultiplied ARGB words (0xAARRGGBB), rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argb {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, e) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

struct Header {
    w: u32,
    h: u32,
    depth: u8,
    colour: u8,
}

impl Header {
    fn channels(&self) -> usize {
        match self.colour {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        }
    }
    fn bits_per_pixel(&self) -> usize {
        self.channels() * self.depth as usize
    }
}

pub fn decode(file: &[u8]) -> Result<Rgba, String> {
    if file.len() < 8 || file[..8] != SIGNATURE {
        return Err("not a PNG (signature)".into());
    }
    let mut at = 8;
    let mut head: Option<Header> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    let mut ended = false;
    while at < file.len() {
        let len = file.get(at..at + 4).ok_or("cut in a chunk length")?;
        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
        if len > 0x7fff_ffff {
            return Err("a chunk longer than 2^31".into());
        }
        let end = at.checked_add(12 + len).ok_or("chunk length overflow")?;
        let chunk = file.get(at + 4..end).ok_or("cut in a chunk")?;
        let (kind, rest) = chunk.split_at(4);
        let (data, crc) = rest.split_at(len);
        if crc32(&chunk[..4 + len]) != u32::from_be_bytes([crc[0], crc[1], crc[2], crc[3]]) {
            return Err(format!("CRC wrong in {}", String::from_utf8_lossy(kind)));
        }
        at = end;
        if head.is_none() && kind != b"IHDR" {
            return Err("the first chunk is not IHDR".into());
        }
        match kind {
            b"IHDR" => {
                if head.is_some() || data.len() != 13 {
                    return Err("a bad IHDR".into());
                }
                let w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let h = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                let (depth, colour) = (data[8], data[9]);
                if w == 0 || h == 0 {
                    return Err("zero width or height".into());
                }
                if w > MAX_SIDE || h > MAX_SIDE {
                    return Err(format!("{w}x{h} is larger than {MAX_SIDE}x{MAX_SIDE}"));
                }
                let ok = match colour {
                    0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
                    3 => matches!(depth, 1 | 2 | 4 | 8),
                    2 | 4 | 6 => matches!(depth, 8 | 16),
                    _ => false,
                };
                if !ok {
                    return Err(format!("colour type {colour} with bit depth {depth} is not PNG"));
                }
                if data[10] != 0 || data[11] != 0 {
                    return Err("an unknown compression or filter method".into());
                }
                match data[12] {
                    0 => {}
                    1 => return Err("Adam7 interlaced (not read; fallback icon)".into()),
                    _ => return Err("an unknown interlace method".into()),
                }
                head = Some(Header { w, h, depth, colour });
            }
            b"PLTE" => {
                if data.len() % 3 != 0 || data.is_empty() || data.len() > 768 {
                    return Err("a bad PLTE".into());
                }
                palette = data.chunks(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => trns = data.to_vec(),
            b"IDAT" => {
                if idat.len() + data.len() > 64 * 1024 * 1024 {
                    return Err("too much image data".into());
                }
                idat.extend_from_slice(data);
            }
            b"IEND" => {
                ended = true;
                break;
            }
            // An unknown critical chunk (upper-case first letter) may change the meaning: refused.
            k if k[0].is_ascii_uppercase() => return Err(format!("an unknown critical chunk {}", String::from_utf8_lossy(k))),
            _ => {}
        }
    }
    if !ended {
        return Err("no IEND (cut)".into());
    }
    let h = head.ok_or("no IHDR")?;
    if h.colour == 3 && palette.is_empty() {
        return Err("a palette image without PLTE".into());
    }
    let bpp = h.bits_per_pixel();
    let stride = (h.w as usize * bpp).div_ceil(8);
    let want = (stride + 1) * h.h as usize;
    let mut raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&idat, want).map_err(|e| format!("inflate: {:?}", e.status))?;
    if raw.len() != want {
        return Err(format!("image data has {} bytes, want {want}", raw.len()));
    }
    unfilter(&mut raw, stride, bpp.div_ceil(8))?;
    Ok(expand(&h, &raw, stride, &palette, &trns))
}

/// Undoes the five filters in place (row = filter byte + `stride` bytes; `step` = bytes per pixel, at least 1).
fn unfilter(raw: &mut [u8], stride: usize, step: usize) -> Result<(), String> {
    let rows = raw.len() / (stride + 1);
    for y in 0..rows {
        let (done, now) = raw.split_at_mut(y * (stride + 1));
        let prev = if y == 0 { None } else { Some(&done[(y - 1) * (stride + 1) + 1..]) };
        let (f, row) = now.split_at_mut(1);
        let row = &mut row[..stride];
        let up = |i: usize| prev.map_or(0, |p| p[i]);
        match f[0] {
            0 => {}
            1 => {
                for i in step..stride {
                    row[i] = row[i].wrapping_add(row[i - step]);
                }
            }
            2 => {
                for i in 0..stride {
                    row[i] = row[i].wrapping_add(up(i));
                }
            }
            3 => {
                for i in 0..stride {
                    let left = if i >= step { row[i - step] } else { 0 };
                    row[i] = row[i].wrapping_add(((left as u16 + up(i) as u16) / 2) as u8);
                }
            }
            4 => {
                for i in 0..stride {
                    let a = if i >= step { row[i - step] } else { 0 };
                    let c = if i >= step { up(i - step) } else { 0 };
                    row[i] = row[i].wrapping_add(paeth(a, up(i), c));
                }
            }
            n => return Err(format!("row {y}: an unknown filter {n}")),
        }
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
}

/// Unfiltered rows to straight RGBA8: 16-bit samples keep their high byte; grey below 8 bits is scaled to 0-255;
/// `tRNS` makes a palette entry or the one matching grey/RGB colour (at the file's own depth) transparent.
fn expand(h: &Header, raw: &[u8], stride: usize, palette: &[[u8; 3]], trns: &[u8]) -> Rgba {
    let (w, d) = (h.w as usize, h.depth as usize);
    let mut px = Vec::with_capacity(w * h.h as usize * 4);
    let key = |i: usize| trns.get(2 * i..2 * i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    for y in 0..h.h as usize {
        let row = &raw[y * (stride + 1) + 1..][..stride];
        // The sample `n` of the row at the file's depth (`d` bits).
        let sample = |n: usize| -> u16 {
            match d {
                16 => u16::from_be_bytes([row[2 * n], row[2 * n + 1]]),
                8 => row[n] as u16,
                _ => {
                    let bit = n * d;
                    ((row[bit / 8] >> (8 - d - bit % 8)) & ((1u8 << d) - 1)) as u16
                }
            }
        };
        let to8 = |v: u16| -> u8 {
            match d {
                16 => (v >> 8) as u8,
                8 => v as u8,
                _ => (v as u32 * 255 / ((1u32 << d) - 1)) as u8,
            }
        };
        for x in 0..w {
            let out = match h.colour {
                0 => {
                    let g = sample(x);
                    let a = if key(0) == Some(g) { 0 } else { 255 };
                    [to8(g), to8(g), to8(g), a]
                }
                2 => {
                    let (r, g, b) = (sample(3 * x), sample(3 * x + 1), sample(3 * x + 2));
                    let a = if (key(0), key(1), key(2)) == (Some(r), Some(g), Some(b)) { 0 } else { 255 };
                    [to8(r), to8(g), to8(b), a]
                }
                3 => {
                    let i = sample(x) as usize;
                    let c = palette.get(i).copied().unwrap_or([0, 0, 0]);
                    [c[0], c[1], c[2], trns.get(i).copied().unwrap_or(255)]
                }
                4 => {
                    let g = to8(sample(2 * x));
                    [g, g, g, to8(sample(2 * x + 1))]
                }
                _ => [to8(sample(4 * x)), to8(sample(4 * x + 1)), to8(sample(4 * x + 2)), to8(sample(4 * x + 3))],
            };
            px.extend_from_slice(&out);
        }
    }
    Rgba { w: h.w, h: h.h, px }
}

/// `img` at `side`x`side` (aspect kept, centred, transparent around it), premultiplied: each target pixel is the
/// area-weighted mean of the source pixels it covers (a box filter; upscaling repeats pixels).
pub fn to_tile(img: &Rgba, side: u32) -> Argb {
    let side = side.max(1);
    let scale = (side as f32 / img.w as f32).min(side as f32 / img.h as f32);
    let (tw, th) = (((img.w as f32 * scale).round() as u32).clamp(1, side), ((img.h as f32 * scale).round() as u32).clamp(1, side));
    let (ox, oy) = ((side - tw) / 2, (side - th) / 2);
    let mut px = vec![0u32; (side * side) as usize];
    let pre = |i: usize| {
        let p = &img.px[i * 4..i * 4 + 4];
        let a = p[3] as f32;
        [p[0] as f32 * a / 255.0, p[1] as f32 * a / 255.0, p[2] as f32 * a / 255.0, a]
    };
    let (fx, fy) = (img.w as f32 / tw as f32, img.h as f32 / th as f32);
    for ty in 0..th {
        let (y0, y1) = (ty as f32 * fy, (ty + 1) as f32 * fy);
        for tx in 0..tw {
            let (x0, x1) = (tx as f32 * fx, (tx + 1) as f32 * fx);
            let mut acc = [0f32; 4];
            let mut area = 0f32;
            let mut sy = y0.floor() as u32;
            while (sy as f32) < y1 && sy < img.h {
                let cy = (sy as f32 + 1.0).min(y1) - (sy as f32).max(y0);
                let mut sx = x0.floor() as u32;
                while (sx as f32) < x1 && sx < img.w {
                    let cx = (sx as f32 + 1.0).min(x1) - (sx as f32).max(x0);
                    let wgt = cx * cy;
                    let p = pre((sy * img.w + sx) as usize);
                    for k in 0..4 {
                        acc[k] += p[k] * wgt;
                    }
                    area += wgt;
                    sx += 1;
                }
                sy += 1;
            }
            let c = |v: f32| (v / area.max(f32::MIN_POSITIVE)).round().clamp(0.0, 255.0) as u32;
            px[((oy + ty) * side + ox + tx) as usize] = c(acc[3]) << 24 | c(acc[0]) << 16 | c(acc[1]) << 8 | c(acc[2]);
        }
    }
    Argb { w: side, h: side, px }
}
