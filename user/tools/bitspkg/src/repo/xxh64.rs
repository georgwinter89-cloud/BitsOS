//! XXH64 (the zstd frame checksum, RFC 8878 3.1.1: the low 32 bits of XXH64 with seed 0 over the frame's content),
//! streaming. ruzstd computes it only with its `hash` feature, whose twox-hash crate is not in the cargo cache; the
//! algorithm is small and fixed (Y. Collet's specification, xxhash.h XXH64).

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

fn merge(acc: u64, v: u64) -> u64 {
    (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4)
}

fn u64_at(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

pub struct Xxh64 {
    v: [u64; 4],
    buf: [u8; 32],
    fill: usize,
    total: u64,
}

impl Default for Xxh64 {
    fn default() -> Self {
        Xxh64 { v: [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)], buf: [0; 32], fill: 0, total: 0 }
    }
}

impl Xxh64 {
    fn stripe(v: &mut [u64; 4], s: &[u8]) {
        for (i, x) in v.iter_mut().enumerate() {
            *x = round(*x, u64_at(&s[i * 8..]));
        }
    }

    pub fn update(&mut self, mut d: &[u8]) {
        self.total += d.len() as u64;
        if self.fill > 0 {
            let k = (32 - self.fill).min(d.len());
            self.buf[self.fill..self.fill + k].copy_from_slice(&d[..k]);
            self.fill += k;
            d = &d[k..];
            if self.fill < 32 {
                return;
            }
            let b = self.buf;
            Self::stripe(&mut self.v, &b);
            self.fill = 0;
        }
        while d.len() >= 32 {
            Self::stripe(&mut self.v, &d[..32]);
            d = &d[32..];
        }
        self.buf[..d.len()].copy_from_slice(d);
        self.fill = d.len();
    }

    pub fn finish(&self) -> u64 {
        let [v1, v2, v3, v4] = self.v;
        let mut h = if self.total >= 32 {
            let h = v1.rotate_left(1).wrapping_add(v2.rotate_left(7)).wrapping_add(v3.rotate_left(12)).wrapping_add(v4.rotate_left(18));
            merge(merge(merge(merge(h, v1), v2), v3), v4)
        } else {
            P5
        };
        h = h.wrapping_add(self.total);
        let mut t = &self.buf[..self.fill];
        while t.len() >= 8 {
            h = (h ^ round(0, u64_at(t))).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
            t = &t[8..];
        }
        if t.len() >= 4 {
            let w = u32::from_le_bytes([t[0], t[1], t[2], t[3]]) as u64;
            h = (h ^ w.wrapping_mul(P1)).rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
            t = &t[4..];
        }
        for &b in t {
            h = (h ^ (b as u64).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
        }
        h ^= h >> 33;
        h = h.wrapping_mul(P2);
        h ^= h >> 29;
        h = h.wrapping_mul(P3);
        h ^ (h >> 32)
    }
}
