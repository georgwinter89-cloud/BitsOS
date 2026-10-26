//! A small streaming zlib/deflate encoder for `screen`'s PNG (RFC 1950/1951): LZ77 with one hash probe and an
//! explicit run check (PNG's Sub/Up filters turn a screen's flat areas into runs of zeros), fixed Huffman codes.
//!
//! Why not miniz_oxide's deflate (V2-LANES MC-3 names it): its `CompressorOxide` holds a 64 KiB array inline and
//! is always built as a value on the stack first (measured: a 65.6 KiB frame in `Box::default`, `Box::write` and
//! an out-of-line constructor, debug and release), which overflows a 64 KiB kernel thread stack onto its guard
//! page. miniz_oxide's inflate (no such array) stays in use for the tests that read the PNG back. Once kernel
//! threads can ask for a bigger stack (change request to SC in plan/notes/mc-MC-3.md) the deflate side can switch
//! back; the PNG code calls only `Deflater`.
//!
//! State is heap-only (hash table and a 32 KiB window, a few hundred KiB), the stack frame small. Scalar (R10).

use alloc::vec::Vec;

const WINDOW: usize = 32 * 1024;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
/// Positions are absolute (`u32`); this marks an empty hash slot.
const NONE: u32 = u32::MAX;

const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

const fn reverse(mut code: u32, len: u32) -> u32 {
    let mut r = 0;
    let mut i = 0;
    while i < len {
        r = (r << 1) | (code & 1);
        code >>= 1;
        i += 1;
    }
    r
}

/// Fixed Huffman code of a literal/length symbol (RFC 1951 3.2.6), bit-reversed for LSB-first output: (code, len).
const FIXED: [(u16, u8); 288] = {
    let mut t = [(0u16, 0u8); 288];
    let mut s = 0;
    while s < 288 {
        let (code, len) = match s {
            0..=143 => (0x30 + s as u32, 8),
            144..=255 => (0x190 + (s as u32 - 144), 9),
            256..=279 => (s as u32 - 256, 7),
            _ => (0xC0 + (s as u32 - 280), 8),
        };
        t[s] = (reverse(code, len) as u16, len as u8);
        s += 1;
    }
    t
};

/// Length 3..=258 -> its symbol index into LEN_BASE (table by length).
const LEN_SYM: [u8; 259] = {
    let mut t = [0u8; 259];
    let mut l = 3;
    while l <= 258 {
        let mut i = 28;
        while LEN_BASE[i] as usize > l {
            i -= 1;
        }
        t[l] = i as u8;
        l += 1;
    }
    t
};

fn dist_sym(d: usize) -> usize {
    // 30 entries: a short search is as fast as a table of 32 K entries and needs no memory.
    let mut i = 29;
    while DIST_BASE[i] as usize > d {
        i -= 1;
    }
    i
}

struct Bits {
    acc: u64,
    n: u32,
}

impl Bits {
    #[inline]
    fn put(&mut self, out: &mut Vec<u8>, bits: u32, len: u32) {
        self.acc |= (bits as u64) << self.n;
        self.n += len;
        while self.n >= 8 {
            out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn align(&mut self, out: &mut Vec<u8>) {
        if self.n > 0 {
            out.push(self.acc as u8);
        }
        self.acc = 0;
        self.n = 0;
    }
}

pub fn adler32(a: u32, data: &[u8]) -> u32 {
    let (mut s1, mut s2) = (a & 0xFFFF, a >> 16);
    for chunk in data.chunks(5552) {
        for &b in chunk {
            s1 += b as u32;
            s2 += s1;
        }
        s1 %= 65521;
        s2 %= 65521;
    }
    (s2 << 16) | s1
}

/// A zlib stream written piece by piece: `write` each piece of input, then `finish`. Output is appended to the
/// caller's `Vec`, so the caller sees its size grow (and can stop early).
pub struct Deflater {
    hash: Vec<u32>,
    /// The last `WINDOW` bytes of input followed by the current piece.
    buf: Vec<u8>,
    /// Absolute position of `buf[0]`.
    base: u32,
    bits: Bits,
    adler: u32,
    started: bool,
}

impl Deflater {
    /// A compressor for pieces of at most `piece` bytes; None when the heap cannot hold its state (the caller
    /// answers "no memory" instead of the kernel's allocation-failure path). Pieces up to `piece` never allocate.
    pub fn try_new(piece: usize) -> Option<Self> {
        let mut hash = Vec::new();
        hash.try_reserve_exact(1 << HASH_BITS).ok()?;
        hash.resize(1 << HASH_BITS, NONE);
        let mut buf = Vec::new();
        buf.try_reserve_exact(WINDOW.checked_add(piece)?).ok()?;
        Some(Deflater { hash, buf, base: 0, bits: Bits { acc: 0, n: 0 }, adler: 1, started: false })
    }

    /// Output bytes one `write` of `n` input bytes can add at most: 9 bits per literal (matches are shorter),
    /// plus the zlib header, block header and a flushed bit buffer. Callers reserve this much beforehand.
    pub const fn worst_case(n: usize) -> usize {
        n + n / 8 + 16
    }

    #[inline]
    fn hash3(b: &[u8]) -> usize {
        let v = (b[0] as u32) | (b[1] as u32) << 8 | (b[2] as u32) << 16;
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    fn literal(&mut self, out: &mut Vec<u8>, b: u8) {
        let (c, l) = FIXED[b as usize];
        self.bits.put(out, c as u32, l as u32);
    }

    fn matched(&mut self, out: &mut Vec<u8>, len: usize, dist: usize) {
        let s = LEN_SYM[len] as usize;
        let (c, l) = FIXED[257 + s];
        self.bits.put(out, c as u32, l as u32);
        if LEN_EXTRA[s] > 0 {
            self.bits.put(out, (len - LEN_BASE[s] as usize) as u32, LEN_EXTRA[s] as u32);
        }
        let d = dist_sym(dist);
        self.bits.put(out, reverse(d as u32, 5), 5);
        if DIST_EXTRA[d] > 0 {
            self.bits.put(out, (dist - DIST_BASE[d] as usize) as u32, DIST_EXTRA[d] as u32);
        }
    }

    /// Compresses `data` (the bytes after everything written before).
    pub fn write(&mut self, out: &mut Vec<u8>, data: &[u8]) {
        if !self.started {
            out.extend_from_slice(&[0x78, 0x01]);
            // One fixed-Huffman block (BFINAL 0, BTYPE 01) for all data; `finish` closes it.
            self.bits.put(out, 0b010, 3);
            self.started = true;
        }
        self.adler = adler32(self.adler, data);
        let start = self.buf.len();
        self.buf.extend_from_slice(data);
        let end = self.buf.len();
        let mut i = start;
        while i < end {
            let left = end - i;
            // A run of the byte before (distance 1): the common case after PNG filtering.
            if i > 0 && left >= MIN_MATCH {
                let prev = self.buf[i - 1];
                let max = left.min(MAX_MATCH);
                let mut n = 0;
                while n < max && self.buf[i + n] == prev {
                    n += 1;
                }
                if n >= MIN_MATCH {
                    self.matched(out, n, 1);
                    i += n;
                    continue;
                }
            }
            if left >= MIN_MATCH {
                let h = Self::hash3(&self.buf[i..]);
                let cand = self.hash[h];
                let here = self.base.wrapping_add(i as u32);
                self.hash[h] = here;
                if cand != NONE && cand >= self.base && here - cand <= WINDOW as u32 {
                    let c = (cand - self.base) as usize;
                    let max = left.min(MAX_MATCH);
                    let mut n = 0;
                    while n < max && self.buf[c + n] == self.buf[i + n] {
                        n += 1;
                    }
                    if n >= MIN_MATCH {
                        self.matched(out, n, i - c);
                        i += n;
                        continue;
                    }
                }
            }
            let b = self.buf[i];
            self.literal(out, b);
            i += 1;
        }
        // Keep the last WINDOW bytes as history for the next piece.
        if self.buf.len() > WINDOW {
            let drop = self.buf.len() - WINDOW;
            self.buf.drain(..drop);
            self.base = self.base.wrapping_add(drop as u32);
        }
        // Positions are u32: past 2^31 bytes start over with an empty table (a 4 GB picture is not a screen).
        if self.base > u32::MAX / 2 {
            self.hash.fill(NONE);
            self.base = 0;
        }
    }

    /// Ends the stream: end of block, an empty final block, the Adler-32 checksum.
    pub fn finish(mut self, out: &mut Vec<u8>) {
        if !self.started {
            self.write(out, &[]);
        }
        let (c, l) = FIXED[256];
        self.bits.put(out, c as u32, l as u32);
        self.bits.put(out, 0b011, 3);
        self.bits.put(out, c as u32, l as u32);
        self.bits.align(out);
        out.extend_from_slice(&self.adler.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;

    /// `worst_case` holds for noise (all literals) and runs, with the header in the first piece.
    #[test]
    fn worst_case_bounds_each_write() {
        let mut seed = 99u32;
        let noise: Vec<u8> = (0..70_000).map(|_| { seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); (seed >> 24) as u8 }).collect();
        let mut d = Deflater::try_new(noise.len()).unwrap();
        let mut out = Vec::new();
        for piece in [&noise[..], &noise[..1], &[0u8; 5000][..], &noise[3..9000]] {
            let before = out.len();
            d.write(&mut out, piece);
            assert!(out.len() - before <= Deflater::worst_case(piece.len()), "{} > {}", out.len() - before, Deflater::worst_case(piece.len()));
        }
        assert!(Deflater::try_new(usize::MAX).is_none());
    }

    fn zlib(pieces: &[&[u8]]) -> Vec<u8> {
        let mut d = Deflater::try_new(1 << 20).unwrap();
        let mut out = Vec::new();
        for p in pieces {
            d.write(&mut out, p);
        }
        d.finish(&mut out);
        out
    }

    fn check(pieces: &[&[u8]]) -> usize {
        let z = zlib(pieces);
        let want: Vec<u8> = pieces.concat();
        let got = miniz_oxide::inflate::decompress_to_vec_zlib(&z).expect("miniz reads the stream");
        assert_eq!(got, want);
        z.len()
    }

    #[test]
    fn adler_of_known_text() {
        assert_eq!(adler32(1, b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(1, &[]), 1);
    }

    #[test]
    fn round_trips_through_miniz_inflate() {
        check(&[]);
        check(&[b""]);
        check(&[b"a"]);
        check(&[b"abcabcabcabcabc", b"abcabc", b"x"]);
        let zeros = vec![0u8; 100_000];
        assert!(check(&[&zeros]) < 1000, "runs compress");
        let mut seed = 1u32;
        let noise: Vec<u8> = (0..200_000).map(|_| { seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); (seed >> 24) as u8 }).collect();
        check(&[&noise[..70_000], &noise[70_000..70_001], &noise[70_001..]]);
        // Repeats farther back than one piece and near the window's edge.
        let mut text = Vec::new();
        for i in 0..5000 {
            text.extend_from_slice(std::format!("line {} of the console, with words that repeat\n", i % 700).as_bytes());
        }
        let n = check(&[&text[..1000], &text[1000..40_000], &text[40_000..]]);
        assert!(n < text.len() / 3, "{n} of {}", text.len());
        // Every length and many distances.
        let mut v = Vec::new();
        for l in 1..300usize {
            v.extend(core::iter::repeat_n((l % 251) as u8, l));
            let from = v.len().saturating_sub(l * 7);
            v.extend_from_within(from..from + l.min(v.len() / 2));
        }
        check(&[&v]);
    }

    #[test]
    fn tables() {
        assert_eq!(LEN_SYM[3], 0);
        assert_eq!(LEN_SYM[10], 7);
        assert_eq!(LEN_SYM[11], 8);
        assert_eq!(LEN_SYM[257], 27);
        assert_eq!(LEN_SYM[258], 28);
        assert_eq!(dist_sym(1), 0);
        assert_eq!(dist_sym(5), 4);
        assert_eq!(dist_sym(32768), 29);
        assert_eq!(FIXED[0], (reverse(0x30, 8) as u16, 8));
        assert_eq!(FIXED[256], (0, 7));
    }
}
