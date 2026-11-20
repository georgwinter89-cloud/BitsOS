//! PK-6b: hostile input. Every truncation and 10 000 seeded random mutations of keys and signatures (generated
//! ones, the RFC 9580 v6 certificate, and on the build host the real CachyOS keyring and five cached package
//! signatures): nothing may panic; each input is refused or verified, and a mutant that still verifies changed
//! nothing the signature covers. The size bounds (packet longer than the input, 64 KiB packets, 1 MiB .sig files,
//! 256 MiB keyrings) are checked one by one. No allocation counter: a global allocator needs unsafe code, which
//! bitspkg forbids.
mod pgpgen;

use bitspkg::pgp::Keyring;
use bitspkg::pgp::keyring::Source;
use bitspkg::pgp::packet::{dearmor, packets};
use bitspkg::pgp::sig::Sig;
use bitspkg::pgp::trust::clock_floor;
use pgpgen::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

fn mutate(r: &mut Rng, mut v: Vec<u8>) -> Vec<u8> {
    for _ in 0..1 + r.below(4) {
        let i = r.below(v.len());
        match r.below(7) {
            0 if !v.is_empty() => v[i] ^= 1 << r.below(8),
            1 if !v.is_empty() => v[i] = r.next() as u8,
            2 if !v.is_empty() => v[i] = [0, 0xff, 0x7f, 0x80, 0xc0][r.below(5)],
            3 => v.truncate(i),
            4 => {
                let b: Vec<u8> = (0..1 + r.below(8)).map(|_| r.next() as u8).collect();
                v.splice(i..i, b);
            }
            5 if i + 4 <= v.len() => v[i..i + 4].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]),
            _ => {
                let j = i + r.below(64).min(v.len() - i);
                let s = v[i..j].to_vec();
                v.splice(i..i, s);
            }
        }
    }
    v
}

/// What a signature file's signatures cover (the hashed part, the values, the digest's first octets, the salt).
fn covered(b: &[u8]) -> Vec<String> {
    let Ok(bin) = dearmor(b) else { return vec![] };
    let Ok(ps) = packets(&bin) else { return vec![] };
    ps.iter().filter_map(|p| Sig::parse(p.body).ok()).map(|s| format!("{:?}{:?}{:?}{:?}", s.hashed, s.mat, s.left16, s.salt)).collect()
}

fn host_inputs() -> (Vec<u8>, Vec<Vec<u8>>) {
    let p = "/usr/share/pacman/keyrings/cachyos.gpg";
    let ring = std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e} (this test runs on the CachyOS build host)"));
    let mut sigs: Vec<_> = std::fs::read_dir("/var/cache/pacman/pkg")
        .expect("/var/cache/pacman/pkg (build host)")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sig"))
        .collect();
    sigs.sort();
    let sigs: Vec<Vec<u8>> = sigs.iter().take(5).map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(sigs.len(), 5, "fewer than 5 cached package signatures on the build host");
    (ring, sigs)
}

#[test]
fn hostile_keys_and_signatures_never_panic() {
    let f = clock_floor() as u32;
    let (t0, now) = (f - 300 * DAY, f as u64 + 86400);
    let m = tkey(1, 4, 22, t0);
    let s6 = tkey(2, 6, 27, t0);
    let sub = tkey(3, 4, 27, t0);
    let mut keys = tpk(&m, "M <m@test>", t0, None);
    keys.extend(tpk(&s6, "S <s@test>", t0, None));
    keys.extend(certify(&m, &s6, "S <s@test>", t0, 0x10));
    keys.extend(subkey(&m, &sub, t0, 0x02, Some(400 * DAY), true));
    keys.extend(revoke(&m, Some(&sub), t0 + DAY));
    let data = b"fuzz".to_vec();
    let mut sigs = vec![data_sig(&m, &data, t0 + DAY, &[], 10), data_sig(&s6, &data, t0 + DAY, &sp(3, &[0x10, 0, 0, 0]), 9)];
    sigs.push(data_sig(&sub, &data, t0 + DAY, &[], 8));
    let (cachy, real_sigs) = host_inputs();
    sigs.extend(real_sigs);
    let src = |k: Vec<u8>| vec![Source { name: "f".into(), keys: k, trusted: vec![m.key.fpr.clone(), s6.key.fpr.clone()], revoked: vec![] }];
    let kr = Keyring::from_sources(src(keys.clone()), now).unwrap();
    assert!(kr.verify_detached(&data[..], &sigs[0]).good);
    assert!(kr.verify_detached(&data[..], &sigs[1]).good);
    let rings = [keys.clone(), cachy.clone(), dearmor(&cachy).unwrap()];
    let (mut refused, mut good, mut rings_ok) = (0usize, 0usize, 0usize);
    // Every truncation of every signature and of the generated keyring.
    for s in &sigs {
        for n in 0..s.len() {
            let v = kr.verify_detached(&data[..], &s[..n]);
            refused += !v.good as usize;
        }
    }
    for n in 0..keys.len() {
        rings_ok += Keyring::from_sources(src(keys[..n].to_vec()), now).is_ok() as usize;
    }
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..10_000 {
        if i % 4 == 0 {
            let pick = rings[r.below(rings.len())].clone();
            let k = mutate(&mut r, pick);
            if let Ok(kr2) = Keyring::from_sources(src(k), now) {
                rings_ok += 1;
                let v = kr2.verify_detached(&data[..], &sigs[r.below(sigs.len())]);
                good += v.good as usize;
            }
        } else {
            let pick = sigs[r.below(sigs.len())].clone();
            let orig = covered(&pick);
            let s = mutate(&mut r, pick);
            let v = kr.verify_detached(&data[..], &s);
            if v.good {
                good += 1;
                let changed: Vec<String> = covered(&s).into_iter().filter(|c| !orig.contains(c)).collect();
                assert!(changed.is_empty(), "mutation {i} verified with changed signed content: {changed:?}");
            } else {
                refused += 1
            }
        }
    }
    eprintln!("fuzz: {refused} refused, {good} verified, {rings_ok} keyrings read");
    assert!(refused > 7000);
}

#[test]
fn nested_embedded_signatures_are_refused_not_recursed() {
    let k = tkey(7, 4, 27, 1_700_000_000);
    let mut inner = sign(&k, 4, 0x19, 10, &created(1_700_000_000), &[], b"x");
    for _ in 0..3 {
        inner = sign(&k, 4, 0x19, 10, &[created(1_700_000_000), sp(32, &inner)].concat(), &[], b"x");
    }
    let e = bitspkg::pgp::sig::Sig::parse(&inner).unwrap_err();
    assert!(e.contains("eingebettete"), "{e}");
}

#[test]
fn size_bounds_are_refused_without_huge_allocations() {
    let f = clock_floor() as u32;
    let (t0, now) = (f - 300 * DAY, f as u64 + 86400);
    let m = tkey(1, 4, 27, t0);
    let keys = tpk(&m, "M <m@test>", t0, None);
    let src = |k: Vec<u8>| vec![Source { name: "f".into(), keys: k, trusted: vec![m.key.fpr.clone()], revoked: vec![] }];
    let kr = Keyring::from_sources(src(keys.clone()), now).unwrap();
    let data = b"x";
    // Headers that claim 4 GiB on a short input, new and old format, as signature and as key packets.
    for h in [&[0xc2u8, 0xff, 0xff, 0xff, 0xff, 0xff, 4, 0][..], &[0x8a, 0xff, 0xff, 0xff, 0xff, 4, 0]] {
        let v = kr.verify_detached(&data[..], h);
        assert!(!v.good && v.reason.contains("länger als die Eingabe"), "{v:?}");
    }
    for h in [&[0xc6u8, 0xff, 0xff, 0xff, 0xff, 0xff, 4, 0][..], &[0x9a, 0xff, 0xff, 0xff, 0xff, 4, 0]] {
        let e = Keyring::from_sources(src(h.to_vec()), now).err().expect("4 GiB key packet");
        assert!(e.contains("länger als die Eingabe"), "{e}");
    }
    // One packet over 64 KiB: a signature and a user ID.
    let big = pkt(2, &[4u8; (64 << 10) + 1]);
    let v = kr.verify_detached(&data[..], &big);
    assert!(!v.good && v.reason.contains("höchstens 64 KiB"), "{v:?}");
    let mut k = keys.clone();
    k.extend(pkt(13, &[b'a'; (64 << 10) + 1]));
    let e = Keyring::from_sources(src(k), now).err().expect("65 KiB user ID");
    assert!(e.contains("länger als 64 KiB"), "{e}");
    // A .sig over 1 MiB and a keyring file over 256 MiB (sparse).
    let v = kr.verify_detached(&data[..], &vec![0xc2; (1 << 20) + 1]);
    assert!(!v.good && v.reason.contains("größer als 1 MiB"), "{v:?}");
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("pgp-sparse-keyring");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fh = std::fs::File::create(dir.join("big.gpg")).unwrap();
    fh.set_len(bitspkg::pgp::keyring::MAX_KEYRING + 1).unwrap();
    drop(fh);
    let e = Keyring::load_at(&dir, now).err().expect("keyring over 256 MiB");
    assert!(e.contains("größer als 256 MiB"), "{e}");
    std::fs::remove_dir_all(&dir).unwrap();
}
