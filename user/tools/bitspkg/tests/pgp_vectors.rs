//! PK-6b: RFC 9580 appendix A test vectors (typed from the RFC text: A.1/A.2 v4 Ed25519Legacy key and signature,
//! A.3 v6 certificate, A.7 v6 signature) and, on the build host, ring against our own RSAVP1 on every RSA
//! signature of the Arch keyring. The build host has no v6-capable signer (no sqop/sq/rnp, gpg 2.4.9), so the v6
//! vectors are the RFC's.
use bitspkg::pgp::key::Key;
use bitspkg::pgp::keyring::{parse_certs, uid_hashed};
use bitspkg::pgp::packet::{base64, dearmor, hex, packets};
use bitspkg::pgp::sig::{Sig, is_cert};
use std::collections::HashMap;

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn rfc9580_a1_a2_v4_ed25519_legacy() {
    let kp = unhex(
        "98 33 04 53 f3 5f 0b 16  09 2b 06 01 04 01 da 47 0f 01 01 07 40 3f 09 89  94 bd d9 16 ed 40 53 19
         79 34 e4 a8 7c 80 73 3a  12 80 d6 2f 80 10 99 2e 43 ee 3b 24 06",
    );
    let sp = unhex(
        "88 5e 04 00 16 08 00 06  05 02 55 f9 5f 95 00 0a 09 10 8c fd e1 21 97 96  5a 9a f6 22 00 ff 56 f9
         0c ca 98 e2 10 26 37 bd  98 3f db 16 c1 31 df d2 7e d8 2b f4 dd e5 60 6e  0d 75 6a ed 33 66 01 00
         d0 9c 4f a1 15 27 f0 38  e0 f5 7f 22 01 d8 2f 2e a2 c9 03 32 65 fa 6c eb  48 9e 85 4b ae 61 b4 04",
    );
    let key = Key::parse(packets(&kp).unwrap()[0].body).unwrap();
    assert_eq!(key.fpr_hex(), "C959BDBAFA32A2F89A153B678CFDE12197965A9A");
    let sig = Sig::parse(packets(&sp).unwrap()[0].body).unwrap();
    assert_eq!(sig.issuer_id, Some(key.key_id()));
    sig.verify_mem(&key, b"OpenPGP").unwrap();
    assert!(sig.verify_mem(&key, b"OpenPGp").is_err());
    let armored = "-----BEGIN PGP SIGNATURE-----\n\niF4EABYIAAYFAlX5X5UACgkQjP3hIZeWWpr2IgD/VvkMypjiECY3vZg/2xbBMd/S\n\
                   ftgr9N3lYG4NdWrtM2YBANCcT6EVJ/A44PV/IgHYLy6iyQMyZfps60iehUuuYbQE\n-----END PGP SIGNATURE-----\n";
    assert_eq!(dearmor(armored.as_bytes()).unwrap(), sp);
}

const A3: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----

xioGY4d/4xsAAAAg+U2nu0jWCmHlZ3BqZYfQMxmZu52JGggkLq2EVD34laPCsQYf
GwoAAABCBYJjh3/jAwsJBwUVCg4IDAIWAAKbAwIeCSIhBssYbE8GCaaX5NUt+mxy
KwwfHifBilZwj2Ul7Ce62azJBScJAgcCAAAAAK0oIBA+LX0ifsDm185Ecds2v8lw
gyU2kCcUmKfvBXbAf6rhRYWzuQOwEn7E/aLwIwRaLsdry0+VcallHhSu4RN6HWaE
QsiPlR4zxP/TP7mhfVEe7XWPxtnMUMtf15OyA51YBM4qBmOHf+MZAAAAIIaTJINn
+eUBXbki+PSAld2nhJh/LVmFsS+60WyvXkQ1wpsGGBsKAAAALAWCY4d/4wKbDCIh
BssYbE8GCaaX5NUt+mxyKwwfHifBilZwj2Ul7Ce62azJAAAAAAQBIKbpGG2dWTX8
j+VjFM21J0hqWlEg+bdiojWnKfA5AQpWUWtnNwDEM0g12vYxoWM8Y81W+bHBw805
I8kWVkXU6vFOi+HWvv/ira7ofJu16NnoUkhclkUrk0mXubZvyl4GBg==
-----END PGP PUBLIC KEY BLOCK-----
";

const A7: &str = "xEYGAQobIHZJX1AhiJD39eLuPBgiUU9wUA9VHYblySHkBONKU/usyxhsTwYJppfk
1S36bHIrDB8eJ8GKVnCPZSXsJ7rZrMkBy0p1AAAAAABXaGF0IHdlIG5lZWQgZnJv
bSB0aGUgZ3JvY2VyeSBzdG9yZToKCi0gdG9mdQotIHZlZ2V0YWJsZXMKLSBub29k
bGVzCsKYBgEbCgAAACkFgmOYo2MiIQbLGGxPBgmml+TVLfpscisMHx4nwYpWcI9l
JewnutmsyQAAAABpNiB2SV9QIYiQ9/Xi7jwYIlFPcFAPVR2G5ckh5ATjSlP7rCfQ
b7gKqPxbyxbhljGygHQPnqau1eBzrQD5QVplPEDnemrnfmkrpx0GmhCfokxYz9jj
FtCgazStmsuOXF9SFQE=";

fn a3() -> (Key, Vec<Sig>, Key) {
    let bin = dearmor(A3.as_bytes()).unwrap();
    let p = packets(&bin).unwrap();
    assert_eq!(p.iter().map(|p| p.tag).collect::<Vec<_>>(), [6, 2, 14, 2]);
    let pk = Key::parse(p[0].body).unwrap();
    let sk = Key::parse(p[2].body).unwrap();
    (pk, vec![Sig::parse(p[1].body).unwrap(), Sig::parse(p[3].body).unwrap()], sk)
}

#[test]
fn rfc9580_a3_v6_certificate() {
    let (pk, sigs, sk) = a3();
    assert_eq!(pk.fpr_hex(), "CB186C4F0609A697E4D52DFA6C722B0C1F1E27C18A56708F6525EC27BAD9ACC9");
    assert_eq!(sk.fpr_hex(), "12C83F1E706F6308FE151A417743A1F033790E93E9978488D1DB378DA9930885");
    assert_eq!((sigs[0].version, sigs[0].typ, sigs[1].typ), (6, 0x1f, 0x18));
    assert!(sigs[0].issued_by(&pk) && sigs[1].issued_by(&pk));
    sigs[0].verify_mem(&pk, &pk.hashed()).unwrap();
    let mut m = pk.hashed();
    m.extend_from_slice(&sk.hashed());
    sigs[1].verify_mem(&pk, &m).unwrap();
    m.push(0);
    assert!(sigs[1].verify_mem(&pk, &m).is_err());
    // The X25519 subkey is kept for its fingerprint but verifies nothing.
    assert!(sk.supported().is_err());
}

#[test]
fn rfc9580_a7_v6_text_signature() {
    let (pk, _, _) = a3();
    let bin = base64(A7).unwrap();
    let p = packets(&bin).unwrap();
    assert_eq!(p.iter().map(|p| p.tag).collect::<Vec<_>>(), [4, 11, 2]);
    // Literal data (5.9): format, name length, name, date, then the text.
    let lit = p[1].body;
    let text = &lit[2 + lit[1] as usize + 4..];
    let sig = Sig::parse(p[2].body).unwrap();
    assert_eq!((sig.version, sig.typ, sig.salt.len()), (6, 0x01, 32));
    // A text signature (type 0x01) hashes the text with CRLF line ends (5.2.1.2).
    let crlf: Vec<u8> = String::from_utf8_lossy(text).replace('\n', "\r\n").into_bytes();
    sig.verify_mem(&pk, &crlf).unwrap();
    assert!(sig.verify_mem(&pk, text).is_err());
}

/// Every RSA signature of the Arch keyring by a key of that keyring: ring (whole message) and our RSAVP1 (digest)
/// give the same answer, and both say good for the self-signatures. Host: /usr/share/pacman/keyrings.
#[test]
fn rsa_ours_equals_ring_on_the_arch_keyring() {
    let p = "/usr/share/pacman/keyrings/archlinux.gpg";
    let raw = std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e} (this test runs on the CachyOS build host)"));
    let mut notes = Vec::new();
    let certs = parse_certs(&raw, "archlinux", &mut notes).unwrap();
    let keys: HashMap<Vec<u8>, &Key> = certs.iter().map(|c| (c.primary.fpr.clone(), &c.primary)).collect();
    let ids: HashMap<[u8; 8], &Key> = certs.iter().map(|c| (c.primary.key_id(), &c.primary)).collect();
    let (mut n, mut ok) = (0, 0);
    for c in &certs {
        for u in &c.uids {
            let mut m = c.primary.hashed();
            m.extend_from_slice(&uid_hashed(&u.raw));
            for s in u.sigs.iter().filter(|s| is_cert(s.typ) && s.pub_algo == 1 && s.sane().is_ok()) {
                let by = s.issuer_fpr.as_ref().and_then(|f| keys.get(f)).or_else(|| s.issuer_id.and_then(|i| ids.get(&i).filter(|k| k.version == 4)));
                let Some(by) = by.filter(|k| k.supported().is_ok()) else { continue };
                let mut h = s.hasher().unwrap();
                h.update(&m);
                let d = s.finish(h);
                let ours = s.verify_digest(by, &d, None).is_ok();
                let theirs = s.verify_mem(by, &m).is_ok();
                assert_eq!(ours, theirs, "{} on {}", hex(&by.fpr), u.text);
                let mut d2 = d.clone();
                d2[5] ^= 1;
                assert!(s.verify_digest(by, &d2, None).is_err(), "a changed digest verified");
                n += 1;
                ok += ours as usize;
            }
        }
    }
    assert!(n >= 100 && ok >= 100, "only {n} RSA signatures ({ok} good) found");
    eprintln!("RSA: {n} signatures, {ok} good, ring and ours agree on all");
}

/// A reader that records the largest read it was asked for and the bytes it gave.
struct Counting<R> {
    inner: R,
    max_req: usize,
    total: u64,
}

impl<R: std::io::Read> std::io::Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.max_req = self.max_req.max(buf.len());
        let n = self.inner.read(buf)?;
        self.total += n as u64;
        Ok(n)
    }
}

/// The RSA path (rsa.rs, own digest) streams a real package: reads of at most 256 KiB, every byte read once.
/// Host: an RSA-signed cached package of 4 to 64 MiB with the real keyrings.
#[test]
fn rsa_package_is_streamed_in_chunks() {
    let kr = bitspkg::pgp::Keyring::load(std::path::Path::new("/usr/share/pacman/keyrings")).expect("keyrings (build host)");
    let mut sigs: Vec<_> = std::fs::read_dir("/var/cache/pacman/pkg")
        .expect("/var/cache/pacman/pkg (build host)")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sig"))
        .collect();
    sigs.sort();
    let mut tried = 0;
    for s in sigs {
        let pkg = s.with_extension("");
        let size = std::fs::metadata(&pkg).map_or(0, |m| m.len());
        let sig = std::fs::read(&s).unwrap();
        let rsa = dearmor(&sig).ok().and_then(|b| packets(&b).ok()?.first().and_then(|p| Sig::parse(p.body).ok())).is_some_and(|x| x.pub_algo == 1);
        if !rsa || !(4 << 20..=64 << 20).contains(&size) {
            continue;
        }
        let mut r = Counting { inner: std::fs::File::open(&pkg).unwrap(), max_req: 0, total: 0 };
        let v = kr.verify_detached(&mut r, &sig);
        if v.good {
            assert_eq!(r.total, size, "{}", pkg.display());
            assert!(r.max_req <= 256 << 10, "read of {} bytes", r.max_req);
            eprintln!("streamed {} ({size} bytes, reads of at most {} bytes)", pkg.display(), r.max_req);
            return;
        }
        tried += 1;
        assert!(tried < 10, "10 RSA-signed packages refused, the last: {v:?}");
    }
    panic!("no RSA-signed cached package of 4 to 64 MiB on the build host");
}
