//! The kernel's random numbers (FS-2): a ChaCha20 generator seeded from the CPU's hardware random source, behind
//! `/dev/urandom`, `/dev/random` and (by change request) `getrandom` and AT_RANDOM.
//!
//! Design as Linux 6.6's `drivers/char/random.c` crng, in small: one base key; every request first takes a
//! block from the base key, whose first half replaces the base key (fast key erasure: an attacker who later
//! reads the key cannot recompute earlier output) and whose second half keys the request's own stream, which
//! is then made without any lock. The base key is reseeded from RDRAND (Intel/AMD's DRBG, reseeded by the CPU's
//! own conditioner) at the first use and then every 60 s (Linux's `crng_reseed` interval). Bytes written to
//! `/dev/random` are mixed in. No RDRAND: a TSC jitter mix, logged once as a warning (both machines have it).
//!
//! No crate: `spec/rust-ecosystem.md` names no ChaCha crate, and the block function is 30 lines (RFC 8439
//! §2.3), checked against the RFC's vectors and against OpenSSL in the host tests.

use ksync::SpinLock;

const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The ChaCha20 block function (RFC 8439 §2.3): 64 bytes of key stream for (key, counter, nonce).
pub fn block(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&CONSTANTS);
    init[4..12].copy_from_slice(key);
    init[12] = counter;
    init[13..].copy_from_slice(nonce);
    let mut s = init;
    for _ in 0..10 {
        qr(&mut s, 0, 4, 8, 12);
        qr(&mut s, 1, 5, 9, 13);
        qr(&mut s, 2, 6, 10, 14);
        qr(&mut s, 3, 7, 11, 15);
        qr(&mut s, 0, 5, 10, 15);
        qr(&mut s, 1, 6, 11, 12);
        qr(&mut s, 2, 7, 8, 13);
        qr(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

pub fn key_from_bytes(b: &[u8; 32]) -> [u32; 8] {
    let mut k = [0u32; 8];
    for (i, w) in k.iter_mut().enumerate() {
        *w = u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    }
    k
}

/// Fills `out` with the key stream of `key` (nonce 0, counter from 0). A request key is used once, so the
/// fixed nonce never repeats a stream.
pub fn stream(key: &[u32; 8], out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(64).enumerate() {
        // 2^32 blocks = 256 GiB per request: more than MAX_RW_COUNT, so the counter never wraps.
        let b = block(key, i as u32, &[0, 0, 0]);
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
}

/// The generator's state without the hardware: pure, so the host tests drive it.
pub struct Drbg {
    key: [u32; 8],
    /// Blocks taken from the base key (the nonce of the next one).
    taken: u64,
}

impl Drbg {
    pub const fn new() -> Self {
        Drbg { key: [0; 8], taken: 0 }
    }
    /// Mixes 32 bytes of seed (or written data) into the base key: the new key is a block of (key XOR seed).
    pub fn mix(&mut self, seed: &[u8; 32]) {
        let s = key_from_bytes(seed);
        let mut k = self.key;
        for i in 0..8 {
            k[i] ^= s[i];
        }
        let b = block(&k, 0, &[0xffff_ffff, self.taken as u32, (self.taken >> 32) as u32]);
        self.taken += 1;
        self.key = key_from_bytes(b[..32].try_into().unwrap_or(&[0; 32]));
    }
    /// One request's key; the base key is replaced in the same step (fast key erasure).
    pub fn request_key(&mut self) -> [u32; 8] {
        let b = block(&self.key, 0, &[0, self.taken as u32, (self.taken >> 32) as u32]);
        self.taken += 1;
        self.key = key_from_bytes(b[..32].try_into().unwrap_or(&[0; 32]));
        key_from_bytes(b[32..].try_into().unwrap_or(&[0; 32]))
    }
}

impl Default for Drbg {
    fn default() -> Self {
        Self::new()
    }
}

struct Global {
    drbg: Drbg,
    /// When the base key was last seeded from the hardware (ns; None = never).
    seeded_ns: Option<u64>,
}

static STATE: SpinLock<Global> = SpinLock::named("vfs.random", Global { drbg: Drbg::new(), seeded_ns: None });

const RESEED_NS: u64 = 60_000_000_000;

klog::counter!(RANDOM_BYTES, "vfs.random_bytes");

/// 32 bytes from the hardware: RDRAND, else a TSC jitter mix (and whether RDRAND gave them).
fn hw_seed() -> ([u8; 32], bool) {
    let mut out = [0u8; 32];
    let mut hw = true;
    let mut x = hal::cpu::rdtsc() ^ 0x9E37_79B9_7F4A_7C15;
    for c in out.chunks_mut(8) {
        // AB-3 change request 8 / FS-2 2c (INT-A1): RDSEED (a true entropy source) first, RDRAND second.
        let v = match hal::entry::hw_seed().or_else(hal::entry::hw_random) {
            Some(v) => v,
            None => {
                hw = false;
                // Each TSC read differs by the jitter of the loop; mixed through xorshift, then ChaCha.
                x ^= hal::cpu::rdtsc().rotate_left(17);
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            }
        };
        c.copy_from_slice(&v.to_le_bytes());
    }
    (out, hw)
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Fills `out` with random bytes (never blocks, never fails).
pub fn fill(out: &mut [u8]) {
    let t = now();
    let due = {
        let g = STATE.lock();
        g.seeded_ns.is_none_or(|s| t.saturating_sub(s) >= RESEED_NS)
    };
    // The hardware is asked outside the lock (RDRAND may retry); two callers seeding at once both mix in.
    let seed = due.then(hw_seed);
    let (key, first) = {
        let mut g = STATE.lock();
        let mut first = None;
        if let Some((s, hw)) = &seed {
            g.drbg.mix(s);
            if g.seeded_ns.is_none() {
                first = Some(*hw);
            }
            g.seeded_ns = Some(t);
        }
        (g.drbg.request_key(), first)
    };
    match first {
        Some(true) => klog::event!(Info, "vfs", "random.seeded", source = "rdrand", reseed_s = RESEED_NS / 1_000_000_000),
        Some(false) => klog::event!(Warn, "vfs", "random.seeded", source = "tsc"; "no RDRAND: /dev/urandom is seeded from TSC jitter only"),
        None => {}
    }
    stream(&key, out);
    RANDOM_BYTES.add(out.len() as u64);
}

/// Mixes bytes a program wrote to `/dev/random` or `/dev/urandom` into the base key (as Linux does; it never
/// replaces what the hardware gave).
pub fn absorb(data: &[u8]) {
    for chunk in data.chunks(32) {
        let mut s = [0u8; 32];
        s[..chunk.len()].copy_from_slice(chunk);
        STATE.lock().drbg.mix(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> alloc::vec::Vec<u8> {
        let s: alloc::string::String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn rfc8439_block_vector() {
        // RFC 8439 §2.3.2: key 00..1f, nonce 00:00:00:09:00:00:00:4a:00:00:00:00, block count 1.
        let kb: [u8; 32] = core::array::from_fn(|i| i as u8);
        let key = key_from_bytes(&kb);
        let nonce = [u32::from_le_bytes([0, 0, 0, 9]), u32::from_le_bytes([0, 0, 0, 0x4a]), 0];
        let out = block(&key, 1, &nonce);
        let want = hex(
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e",
        );
        assert_eq!(&out[..], &want[..]);
    }

    #[test]
    fn rfc8439_appendix_a1_zero_key() {
        // RFC 8439 A.1 test vector #1: all-zero key and nonce, block counter 0.
        let out = block(&[0; 8], 0, &[0; 3]);
        let want = hex(
            "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7
             da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586",
        );
        assert_eq!(&out[..], &want[..]);
    }

    #[test]
    fn drbg_erases_and_differs() {
        let mut d = Drbg::new();
        d.mix(&[7; 32]);
        let k1 = d.request_key();
        let base_after = d.key;
        let k2 = d.request_key();
        assert_ne!(k1, k2);
        assert_ne!(base_after, d.key, "the base key changes with every request");
        let mut a = [0u8; 200];
        let mut b = [0u8; 200];
        stream(&k1, &mut a);
        stream(&k2, &mut b);
        assert_ne!(a, b);
        // Deterministic for the same seed (the hardware is the only randomness).
        let mut e = Drbg::new();
        e.mix(&[7; 32]);
        assert_eq!(e.request_key(), k1);
        // A different seed gives a different stream.
        let mut f = Drbg::new();
        f.mix(&[8; 32]);
        assert_ne!(f.request_key(), k1);
    }

    #[test]
    fn global_fill_is_not_constant() {
        let mut a = [0u8; 4096];
        let mut b = [0u8; 4096];
        fill(&mut a);
        fill(&mut b);
        assert_ne!(a, b);
        assert!(a.iter().any(|&x| x != 0));
        // Rough byte balance: every value appears in 64 KiB.
        let mut big = alloc::vec![0u8; 65536];
        fill(&mut big);
        let mut seen = [false; 256];
        for &x in &big {
            seen[x as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
        absorb(b"written by a program");
        fill(&mut a);
        assert_ne!(a, b);
    }
}
