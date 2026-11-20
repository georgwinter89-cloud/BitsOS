//! A tiny OpenPGP signer for the verifier's tests: Ed25519 keys (v4 legacy 22, v4 and v6 algorithm 27) from fixed
//! seeds, user IDs, certifications, subkey bindings with back signatures, revocations and data signatures. It
//! writes the packets the way RFC 9580 describes them, independently of the verifier's own writer-free code.
#![allow(dead_code)]

use bitspkg::pgp::key::Key;
use bitspkg::pgp::keyring::{Source, uid_hashed};
use ring::signature::{Ed25519KeyPair, KeyPair};
use sha2::{Digest, Sha256, Sha384, Sha512};

pub const DAY: u32 = 86400;

pub struct TKey {
    pub kp: Ed25519KeyPair,
    pub version: u8,
    pub algo: u8,
    pub body: Vec<u8>,
    pub key: Key,
}

pub fn tkey(seed: u8, version: u8, algo: u8, created: u32) -> TKey {
    let kp = Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap();
    let pk = kp.public_key().as_ref().to_vec();
    let mut body = vec![version];
    body.extend_from_slice(&created.to_be_bytes());
    body.push(algo);
    let mut mat = Vec::new();
    if algo == 22 {
        mat.push(9);
        mat.extend_from_slice(&[0x2b, 0x06, 0x01, 0x04, 0x01, 0xda, 0x47, 0x0f, 0x01]);
        mat.extend_from_slice(&263u16.to_be_bytes());
        mat.push(0x40);
        mat.extend_from_slice(&pk);
    } else {
        mat.extend_from_slice(&pk);
    }
    if version == 6 {
        body.extend_from_slice(&(mat.len() as u32).to_be_bytes());
    }
    body.extend_from_slice(&mat);
    let key = Key::parse(&body).unwrap();
    TKey { kp, version, algo, body, key }
}

pub fn pkt(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0xC0 | tag, 0xFF];
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    v
}

/// A subpacket; `t | 0x80` marks it critical.
pub fn sp(t: u8, d: &[u8]) -> Vec<u8> {
    let n = d.len() + 1;
    let mut v = if n < 192 {
        vec![n as u8]
    } else if n < 8384 {
        vec![(((n - 192) >> 8) + 192) as u8, ((n - 192) & 0xff) as u8]
    } else {
        let mut v = vec![255];
        v.extend_from_slice(&(n as u32).to_be_bytes());
        v
    };
    v.push(t);
    v.extend_from_slice(d);
    v
}

pub fn created(t: u32) -> Vec<u8> {
    sp(2, &t.to_be_bytes())
}

pub fn issuer(k: &TKey) -> Vec<u8> {
    let mut d = vec![k.version];
    d.extend_from_slice(&k.key.fpr);
    sp(33, &d)
}

fn mpi(b: &[u8]) -> Vec<u8> {
    let mut b = b;
    while let [0, r @ ..] = b {
        b = r;
    }
    let bits = match b.first() {
        Some(&x) => b.len() * 8 - x.leading_zeros() as usize,
        None => 0,
    };
    let mut v = (bits as u16).to_be_bytes().to_vec();
    v.extend_from_slice(b);
    v
}

fn digest(hash: u8, d: &[u8]) -> Vec<u8> {
    match hash {
        8 => Sha256::digest(d).to_vec(),
        9 => Sha384::digest(d).to_vec(),
        10 => Sha512::digest(d).to_vec(),
        2 => ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, d).as_ref().to_vec(),
        _ => panic!("hash {hash}"),
    }
}

/// A signature packet body by `by` over `data` (the already prefixed key/user ID octets, or the document).
pub fn sign(by: &TKey, version: u8, typ: u8, hash: u8, hashed: &[u8], unhashed: &[u8], data: &[u8]) -> Vec<u8> {
    let mut head = vec![version, typ, by.algo, hash];
    if version == 4 {
        head.extend_from_slice(&(hashed.len() as u16).to_be_bytes());
    } else {
        head.extend_from_slice(&(hashed.len() as u32).to_be_bytes());
    }
    head.extend_from_slice(hashed);
    let salt: Vec<u8> = if version == 6 { (0..[16, 24, 32][(hash - 8) as usize]).map(|i| i as u8 ^ 0x5a).collect() } else { vec![] };
    let mut m = salt.clone();
    m.extend_from_slice(data);
    m.extend_from_slice(&head);
    m.extend_from_slice(&[version, 0xff]);
    m.extend_from_slice(&(head.len() as u32).to_be_bytes());
    let d = digest(hash, &m);
    let s = by.kp.sign(&d);
    let s = s.as_ref();
    let mut v = head;
    if version == 4 {
        v.extend_from_slice(&(unhashed.len() as u16).to_be_bytes());
    } else {
        v.extend_from_slice(&(unhashed.len() as u32).to_be_bytes());
    }
    v.extend_from_slice(unhashed);
    v.extend_from_slice(&d[..2]);
    if version == 6 {
        v.push(salt.len() as u8);
        v.extend_from_slice(&salt);
    }
    if by.algo == 22 {
        v.extend_from_slice(&mpi(&s[..32]));
        v.extend_from_slice(&mpi(&s[32..]));
    } else {
        v.extend_from_slice(s);
    }
    v
}

pub fn key_uid(k: &TKey, uid: &str) -> Vec<u8> {
    let mut m = k.key.hashed();
    m.extend_from_slice(&uid_hashed(uid.as_bytes()));
    m
}

pub fn key_sub(p: &TKey, s: &TKey) -> Vec<u8> {
    let mut m = p.key.hashed();
    m.extend_from_slice(&s.key.hashed());
    m
}

/// Key packet, user ID and its positive self-certification (flags certify + sign, optional key expiry).
pub fn tpk(k: &TKey, uid: &str, t: u32, expiry: Option<u32>) -> Vec<u8> {
    let mut h = created(t);
    h.extend(sp(27, &[0x03]));
    if let Some(e) = expiry {
        h.extend(sp(9, &e.to_be_bytes()));
    }
    h.extend(issuer(k));
    let mut v = pkt(6, &k.body);
    v.extend(pkt(13, uid.as_bytes()));
    v.extend(pkt(2, &sign(k, k.version, 0x13, 10, &h, &[], &key_uid(k, uid))));
    v
}

/// A certification (or with `typ` 0x30 its revocation) by `by` on `target`'s user ID, as a signature packet.
pub fn certify(by: &TKey, target: &TKey, uid: &str, t: u32, typ: u8) -> Vec<u8> {
    certify_x(by, target, uid, t, typ, 10, &[])
}

/// `certify` with a hash algorithm and extra hashed subpackets.
pub fn certify_x(by: &TKey, target: &TKey, uid: &str, t: u32, typ: u8, hash: u8, extra: &[u8]) -> Vec<u8> {
    let mut h = created(t);
    h.extend(issuer(by));
    h.extend_from_slice(extra);
    pkt(2, &sign(by, by.version, typ, hash, &h, &[], &key_uid(target, uid)))
}

/// A subkey packet with its binding (flags, expiry) and, when `backsig`, the embedded primary key binding.
pub fn subkey(p: &TKey, s: &TKey, t: u32, flags: u8, expiry: Option<u32>, backsig: bool) -> Vec<u8> {
    let m = key_sub(p, s);
    let mut h = created(t);
    h.extend(sp(27, &[flags]));
    if let Some(e) = expiry {
        h.extend(sp(9, &e.to_be_bytes()));
    }
    h.extend(issuer(p));
    if backsig {
        let mut bh = created(t);
        bh.extend(issuer(s));
        h.extend(sp(32, &sign(s, s.version, 0x19, 10, &bh, &[], &m)));
    }
    let mut v = pkt(14, &s.body);
    v.extend(pkt(2, &sign(p, p.version, 0x18, 10, &h, &[], &m)));
    v
}

/// A key (0x20) or subkey (0x28) revocation by `p`, as a signature packet.
pub fn revoke(p: &TKey, sub: Option<&TKey>, t: u32) -> Vec<u8> {
    revoke_x(p, sub, t, 10, &[])
}

/// `revoke` with a hash algorithm and extra hashed subpackets.
pub fn revoke_x(p: &TKey, sub: Option<&TKey>, t: u32, hash: u8, extra: &[u8]) -> Vec<u8> {
    let mut h = created(t);
    h.extend(sp(29, b"\x02compromised"));
    h.extend(issuer(p));
    h.extend_from_slice(extra);
    match sub {
        None => pkt(2, &sign(p, p.version, 0x20, hash, &h, &[], &p.key.hashed())),
        Some(s) => pkt(2, &sign(p, p.version, 0x28, hash, &h, &[], &key_sub(p, s))),
    }
}

/// A reader that makes `len` bytes on the fly (no buffer) and records the largest read it was asked for.
pub struct Gen {
    pub pos: u64,
    pub len: u64,
    pub max_req: usize,
    pub reads: usize,
}

impl Gen {
    pub fn new(len: u64) -> Gen {
        Gen { pos: 0, len, max_req: 0, reads: 0 }
    }
    pub fn byte(i: u64) -> u8 {
        (i.wrapping_mul(0x9e37_79b9) >> 11) as u8 ^ i as u8
    }
}

impl std::io::Read for Gen {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.max_req = self.max_req.max(buf.len());
        self.reads += 1;
        let n = (buf.len() as u64).min(self.len - self.pos) as usize;
        for (k, b) in buf[..n].iter_mut().enumerate() {
            *b = Gen::byte(self.pos + k as u64);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

/// A reader over any other that records the largest read it was asked for and the bytes it gave.
pub struct Counting<R> {
    pub inner: R,
    pub max_req: usize,
    pub total: u64,
}

impl<R: std::io::Read> std::io::Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.max_req = self.max_req.max(buf.len());
        let n = self.inner.read(buf)?;
        self.total += n as u64;
        Ok(n)
    }
}

/// A detached binary signature by `by` over `data`, with extra hashed subpackets.
pub fn data_sig(by: &TKey, data: &[u8], t: u32, extra: &[u8], hash: u8) -> Vec<u8> {
    let mut h = created(t);
    h.extend(issuer(by));
    h.extend_from_slice(extra);
    pkt(2, &sign(by, by.version, 0x00, hash, &h, &[], data))
}

pub fn source(name: &str, keys: Vec<u8>, trusted: &[&TKey], revoked: &[&TKey]) -> Source {
    Source {
        name: name.into(),
        keys,
        trusted: trusted.iter().map(|k| k.key.fpr.clone()).collect(),
        revoked: revoked.iter().map(|k| k.key.fpr.clone()).collect(),
    }
}
