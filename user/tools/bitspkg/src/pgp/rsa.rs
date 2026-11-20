//! RSA PKCS#1 v1.5 verification of a digest (RFC 8017 sections 8.2.2 and 9.2). ring 0.17.14 verifies RSA only
//! over the whole message (rsa/verification.rs:198 hashes it itself), so a 300 MB package, which is streamed,
//! needs RSAVP1 here: s^e mod n with public values only (no secret, so no constant-time need), then a byte-exact
//! compare against the expected encoding. Every in-memory message (certifications, bindings) goes through ring;
//! a test checks both agree on every RSA signature of the Arch keyring.

/// DigestInfo prefixes of RFC 8017 section 9.2 note 1.
fn digest_info(hash: u8) -> Option<&'static [u8]> {
    Some(match hash {
        8 => &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20],
        9 => &[0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30],
        10 => &[0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40],
        _ => return None,
    })
}

/// Little-endian 64-bit limbs of a big-endian number.
fn limbs(be: &[u8], k: usize) -> Vec<u64> {
    let mut v = vec![0u64; k];
    for (i, &b) in be.iter().rev().enumerate() {
        v[i / 8] |= (b as u64) << (8 * (i % 8));
    }
    v
}

fn geq(a: &[u64], n: &[u64]) -> bool {
    for i in (0..n.len()).rev() {
        if a[i] != n[i] {
            return a[i] > n[i];
        }
    }
    true
}

fn sub_in(a: &mut [u64], n: &[u64]) {
    let mut borrow = 0u64;
    for i in 0..n.len() {
        let (d, b1) = a[i].overflowing_sub(n[i]);
        let (d, b2) = d.overflowing_sub(borrow);
        a[i] = d;
        borrow = (b1 | b2) as u64;
    }
}

/// Montgomery product a*b*R^-1 mod n (CIOS), R = 2^(64k); a, b < n.
fn mont(a: &[u64], b: &[u64], n: &[u64], n0: u64) -> Vec<u64> {
    let k = n.len();
    let mut t = vec![0u64; k + 2];
    for &bi in b.iter().take(k) {
        let mut c = 0u128;
        for j in 0..k {
            let s = t[j] as u128 + a[j] as u128 * bi as u128 + c;
            t[j] = s as u64;
            c = s >> 64;
        }
        let s = t[k] as u128 + c;
        t[k] = s as u64;
        t[k + 1] = (s >> 64) as u64;
        let m = t[0].wrapping_mul(n0);
        let mut c = (t[0] as u128 + m as u128 * n[0] as u128) >> 64;
        for j in 1..k {
            let s = t[j] as u128 + m as u128 * n[j] as u128 + c;
            t[j - 1] = s as u64;
            c = s >> 64;
        }
        let s = t[k] as u128 + c;
        t[k - 1] = s as u64;
        t[k] = t[k + 1] + (s >> 64) as u64;
        t[k + 1] = 0;
    }
    let carry = t[k] != 0;
    let mut r = t[..k].to_vec();
    if carry || geq(&r, n) {
        sub_in(&mut r, n);
    }
    r
}

/// s^e mod n as `n.len()` big-endian octets. n odd, s < n, e >= 3 (checked by the caller).
fn power(n_be: &[u8], e_be: &[u8], s_be: &[u8]) -> Vec<u8> {
    let k = n_be.len().div_ceil(8);
    let n = limbs(n_be, k);
    let s = limbs(s_be, k);
    // -n^-1 mod 2^64 by Newton's iteration (each step doubles the correct low bits).
    let mut inv = 1u64;
    for _ in 0..7 {
        inv = inv.wrapping_mul(2u64.wrapping_sub(n[0].wrapping_mul(inv)));
    }
    let n0 = inv.wrapping_neg();
    // R^2 mod n by doubling 1 (2 * 64k) times.
    let mut r2 = vec![0u64; k];
    r2[0] = 1;
    for _ in 0..128 * k {
        let mut carry = 0u64;
        for x in r2.iter_mut() {
            let nc = *x >> 63;
            *x = (*x << 1) | carry;
            carry = nc;
        }
        if carry != 0 || geq(&r2, &n) {
            sub_in(&mut r2, &n);
        }
    }
    let sm = mont(&s, &r2, &n, n0);
    let mut acc = sm.clone();
    let bits: Vec<bool> = e_be.iter().flat_map(|b| (0..8).rev().map(move |i| b >> i & 1 == 1)).skip_while(|b| !b).collect();
    for &bit in bits.iter().skip(1) {
        acc = mont(&acc, &acc, &n, n0);
        if bit {
            acc = mont(&acc, &sm, &n, n0);
        }
    }
    let mut one = vec![0u64; k];
    one[0] = 1;
    let r = mont(&acc, &one, &n, n0);
    let mut out = vec![0u8; n_be.len()];
    for (i, o) in out.iter_mut().rev().enumerate() {
        *o = (r[i / 8] >> (8 * (i % 8))) as u8;
    }
    out
}

/// Checks the sizes ring would check (rsa/public_key.rs: 2048..8192 bits, e odd, 3 <= e < 2^33) and returns the
/// modulus' bit length.
pub fn check_key(n: &[u8], e: &[u8]) -> Result<usize, String> {
    let bits = match n.first() {
        Some(&b) => n.len() * 8 - b.leading_zeros() as usize,
        None => 0,
    };
    if bits < 2048 {
        return Err(format!("RSA-Schlüssel mit {bits} Bit ist zu kurz (mindestens 2048)"));
    }
    if bits > 8192 {
        return Err(format!("RSA-Schlüssel mit {bits} Bit ist zu lang (höchstens 8192)"));
    }
    let ev = e.iter().try_fold(0u64, |a, &b| if a >> 40 == 0 { Some(a << 8 | b as u64) } else { None });
    match ev {
        Some(v) if v >= 3 && v % 2 == 1 && v < 1 << 33 && n.last().is_some_and(|l| l & 1 == 1) => Ok(bits),
        _ => Err("RSA-Schlüssel mit ungültigem Exponenten oder Modul".into()),
    }
}

/// RSASSA-PKCS1-v1_5 verification of `digest` (hash algorithm `hash`, RFC 9580 9.5 ids 8/9/10).
pub fn verify_digest(n: &[u8], e: &[u8], hash: u8, digest: &[u8], sig: &[u8]) -> Result<(), String> {
    check_key(n, e)?;
    let info = digest_info(hash).ok_or("Hash-Verfahren für RSA nicht erlaubt")?;
    let bad = || "Signatur passt nicht (RSA)".to_string();
    if sig.len() > n.len() {
        return Err(bad());
    }
    let mut s = vec![0u8; n.len() - sig.len()];
    s.extend_from_slice(sig);
    if s >= n.to_vec() {
        return Err(bad());
    }
    let em = power(n, e, &s);
    let t = info.len() + digest.len();
    if n.len() < t + 11 {
        return Err(bad());
    }
    let mut want = vec![0xffu8; n.len()];
    want[0] = 0;
    want[1] = 1;
    want[n.len() - t - 1] = 0;
    want[n.len() - t..n.len() - digest.len()].copy_from_slice(info);
    want[n.len() - digest.len()..].copy_from_slice(digest);
    if em == want { Ok(()) } else { Err(bad()) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn power_small_numbers() {
        // 2^65537 mod (2^127 - 1): 65537 = 127*516 + 5, so the result is 2^5.
        let n: Vec<u8> = {
            let mut v = vec![0xffu8; 16];
            v[0] = 0x7f;
            v
        };
        let r = super::power(&n, &[1, 0, 1], &[2]);
        assert_eq!(r[15], 32);
        assert!(r[..15].iter().all(|&b| b == 0));
    }
}
