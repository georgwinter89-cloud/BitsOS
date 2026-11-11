//! SHA-512 (FIPS 180-4), for the one download whose maker publishes only SHA-512 sums: QEMU for Windows.

use std::io::{self, Read};

const K: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc, 0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2, 0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65, 0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4, 0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df, 0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30, 0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8, 0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec, 0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178, 0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c, 0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

const H0: [u64; 8] = [0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1, 0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179];

fn block(h: &mut [u64; 8], b: &[u8; 128]) {
    let mut w = [0u64; 80];
    for (i, c) in b.as_chunks::<8>().0.iter().enumerate() {
        w[i] = u64::from_be_bytes(*c);
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b_, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b_) ^ (a & c) ^ (b_ & c);
        let t2 = s0.wrapping_add(maj);
        (hh, g, f, e, d, c, b_, a) = (g, f, e, d.wrapping_add(t1), c, b_, a, t1.wrapping_add(t2));
    }
    for (x, y) in h.iter_mut().zip([a, b_, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(y);
    }
}

/// Hash (128 hex digits) and length of everything `r` yields.
pub fn of_reader(mut r: impl Read) -> io::Result<(String, u64)> {
    let mut h = H0;
    let mut buf = [0u8; 128];
    let (mut fill, mut len) = (0usize, 0u128);
    loop {
        let k = r.read(&mut buf[fill..])?;
        if k == 0 {
            break;
        }
        fill += k;
        len += k as u128;
        if fill == 128 {
            block(&mut h, &buf);
            fill = 0;
        }
    }
    buf[fill] = 0x80;
    buf[fill + 1..].fill(0);
    if fill >= 112 {
        block(&mut h, &buf);
        buf = [0; 128];
    }
    buf[112..].copy_from_slice(&(len * 8).to_be_bytes());
    block(&mut h, &buf);
    Ok((h.iter().map(|x| format!("{x:016x}")).collect(), len as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors() {
        let h = |d: &[u8]| of_reader(d).unwrap().0;
        assert_eq!(h(b"abc"), "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
        assert_eq!(h(b""), "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e");
        let long = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        assert_eq!(h(long), "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909");
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + 3) as u8).collect();
        let want = "a7875d841eb26ba90ecce80cc9b5add3a7c3ea2574fb6a948eec3983862c0b653682fa1cd70220a4cee488f40e8d9ee124715075eae3426c7638efe40bed45a4";
        assert_eq!(of_reader(&data[..]).unwrap(), (want.to_string(), 5000));
        // Short reads fill the block piece by piece.
        struct Drip<'a>(&'a [u8]);
        impl Read for Drip<'_> {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                let n = self.0.len().min(b.len()).min(7);
                b[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        assert_eq!(of_reader(Drip(&data)).unwrap().0, want);
    }
}
