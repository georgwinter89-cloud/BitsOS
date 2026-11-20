//! PK-6b: the keyring trust and the data signature verdict with keys made by tests/pgpgen (Ed25519 v4 legacy,
//! v4 and v6 algorithm 27). Host only.
mod pgpgen;

use bitspkg::pgp::trust::clock_floor;
use bitspkg::pgp::{Keyring, Trust, Verdict};
use pgpgen::*;

fn times() -> (u32, u64) {
    let f = clock_floor() as u32;
    (f - 300 * DAY, f as u64 + DAY as u64)
}

const DATA: &[u8] = b"ein Paket, gestreamt";

fn ring(keys: Vec<u8>, trusted: &[&TKey], revoked: &[&TKey], now: u64) -> Keyring {
    Keyring::from_sources(vec![source("test", keys, trusted, revoked)], now).unwrap()
}

fn check(kr: &Keyring, sig: &[u8]) -> Verdict {
    kr.verify_detached(DATA, sig)
}

fn bad_with(v: Verdict, words: &str) {
    assert!(!v.good && v.reason.contains(words), "want refusal with \"{words}\", got {v:?}");
}

/// Three main keys (m1 legacy 22, m2 v4 27, m3 v6 27) and packagers certified by some of them.
fn mains(t0: u32) -> (Vec<TKey>, Vec<u8>) {
    let m = vec![tkey(1, 4, 22, t0), tkey(2, 4, 27, t0), tkey(3, 6, 27, t0)];
    let mut keys = Vec::new();
    for (i, k) in m.iter().enumerate() {
        keys.extend(tpk(k, &format!("Haupt {i} <m{i}@test>"), t0, None));
    }
    (m, keys)
}

#[test]
fn packager_needs_three_main_keys() {
    let (t0, now) = times();
    let (m, mut keys) = mains(t0);
    let p = tkey(10, 4, 22, t0);
    let q = tkey(11, 4, 27, t0);
    let r = tkey(12, 4, 22, t0);
    keys.extend(tpk(&p, "P <p@test>", t0, None));
    for k in &m {
        keys.extend(certify(k, &p, "P <p@test>", t0 + DAY, 0x10));
    }
    keys.extend(tpk(&q, "Q <q@test>", t0, None));
    for k in &m[..2] {
        keys.extend(certify(k, &q, "Q <q@test>", t0 + DAY, 0x12));
    }
    keys.extend(tpk(&r, "R <r@test>", t0, None));
    for k in &m {
        keys.extend(certify(k, &r, "R <r@test>", t0 + DAY, 0x13));
    }
    keys.extend(certify(&m[2], &r, "R <r@test>", t0 + 2 * DAY, 0x30));
    let kr = ring(keys, &[&m[0], &m[1], &m[2]], &[], now);
    assert_eq!(kr.counts(), (3, 4), "{:#?}", kr.describe());
    let v = check(&kr, &data_sig(&p, DATA, t0 + 5 * DAY, &[], 10));
    assert!(v.good && v.trust == Trust::Full, "{v:?}");
    assert_eq!((v.key_fpr.as_str(), v.owner_uid.as_str()), (p.key.fpr_hex().as_str(), "P <p@test>"));
    let vq = check(&kr, &data_sig(&q, DATA, t0 + 5 * DAY, &[], 10));
    assert_eq!(vq.trust, Trust::Marginal, "{vq:?}");
    bad_with(vq, "2 von 3");
    bad_with(check(&kr, &data_sig(&r, DATA, t0 + 5 * DAY, &[], 10)), "2 von 3");
    // A marginal key whose signature does not verify is invalid, not marginal.
    let mut d = DATA.to_vec();
    d[0] ^= 1;
    assert_eq!(kr.verify_detached(&d[..], &data_sig(&q, DATA, t0 + 5 * DAY, &[], 10)).trust, Trust::Invalid);
    for k in &m {
        assert!(check(&kr, &data_sig(k, DATA, t0 + DAY, &[], 8)).good);
    }
}

#[test]
fn revoked_keys_are_never_valid() {
    let (t0, now) = times();
    let (m, mut keys) = mains(t0);
    let k = tkey(20, 4, 27, t0);
    keys.extend(tpk(&k, "K <k@test>", t0, None));
    keys.extend(revoke(&k, None, t0 + 10 * DAY));
    let kr = ring(keys, &[&m[0], &m[1], &k], &[&m[1]], now);
    assert!(check(&kr, &data_sig(&m[0], DATA, t0 + DAY, &[], 10)).good);
    bad_with(check(&kr, &data_sig(&m[1], DATA, t0 + DAY, &[], 10)), "gesperrt");
    bad_with(check(&kr, &data_sig(&k, DATA, t0 + DAY, &[], 10)), "widerrufen");
}

#[test]
fn signing_subkeys_need_flag_back_signature_and_lifetime() {
    let (t0, now) = times();
    let p = tkey(30, 4, 27, t0);
    let subs: Vec<TKey> = (31..36).map(|s| tkey(s, 4, 22, t0)).collect();
    let mut keys = tpk(&p, "P <p@test>", t0, None);
    keys.extend(subkey(&p, &subs[0], t0, 0x02, None, true));
    keys.extend(subkey(&p, &subs[1], t0, 0x02, None, false));
    keys.extend(subkey(&p, &subs[2], t0, 0x02, Some(10 * DAY), true));
    keys.extend(subkey(&p, &subs[3], t0, 0x02, None, true));
    keys.extend(revoke(&p, Some(&subs[3]), t0 + 20 * DAY));
    keys.extend(subkey(&p, &subs[4], t0, 0x0c, None, true));
    let kr = ring(keys, &[&p], &[], now);
    let v = check(&kr, &data_sig(&subs[0], DATA, t0 + DAY, &[], 10));
    assert!(v.good && v.key_fpr == p.key.fpr_hex(), "{v:?}");
    bad_with(check(&kr, &data_sig(&subs[1], DATA, t0 + DAY, &[], 10)), "Rücksignatur");
    bad_with(check(&kr, &data_sig(&subs[2], DATA, t0 + DAY, &[], 10)), "abgelaufen");
    bad_with(check(&kr, &data_sig(&subs[3], DATA, t0 + DAY, &[], 10)), "widerrufen");
    bad_with(check(&kr, &data_sig(&subs[4], DATA, t0 + DAY, &[], 10)), "darf nicht signieren");
}

#[test]
fn critical_subpackets_and_hashes() {
    let (t0, now) = times();
    let (m, keys) = mains(t0);
    let kr = ring(keys, &[&m[0], &m[1], &m[2]], &[], now);
    let k = &m[0];
    bad_with(check(&kr, &data_sig(k, DATA, t0 + DAY, &sp(0x80 | 99, b"x"), 10)), "kritischem Unterpaket 99");
    bad_with(check(&kr, &data_sig(k, DATA, t0 + DAY, &sp(0x80 | 20, b"\x80\0\0\0\0\x01\0\x01ab"), 10)), "Unterpaket 20");
    assert!(check(&kr, &data_sig(k, DATA, t0 + DAY, &sp(99, b"x"), 10)).good);
    assert!(check(&kr, &data_sig(k, DATA, t0 + DAY, &sp(0x80 | 27, &[3]), 10)).good);
    for h in [8, 9, 10] {
        assert!(check(&kr, &data_sig(k, DATA, t0 + DAY, &[], h)).good, "hash {h}");
        assert!(check(&kr, &data_sig(&m[2], DATA, t0 + DAY, &[], h)).good, "v6 hash {h}");
    }
    bad_with(check(&kr, &data_sig(k, DATA, t0 + DAY, &[], 2)), "SHA-1");
}

#[test]
fn v6_keys_sign_only_v6() {
    let (t0, now) = times();
    let k6 = tkey(40, 6, 27, t0);
    let keys = tpk(&k6, "Sechs <v6@test>", t0, None);
    let kr = ring(keys, &[&k6], &[], now);
    let v = check(&kr, &data_sig(&k6, DATA, t0 + DAY, &[], 10));
    assert!(v.good && v.key_fpr.len() == 64, "{v:?}");
    let mut h = created(t0 + DAY);
    h.extend(issuer(&k6));
    bad_with(check(&kr, &pkt(2, &sign(&k6, 4, 0, 10, &h, &[], DATA))), "v4-Signatur von einem v6-Schlüssel");
}

#[test]
fn clock_times_and_tampering() {
    let (t0, now) = times();
    let k = tkey(50, 4, 22, t0);
    let e = tkey(51, 4, 27, t0);
    let mut keys = tpk(&k, "K <k@test>", t0, None);
    keys.extend(tpk(&e, "E <e@test>", t0, Some(10 * DAY)));
    let kr = ring(keys.clone(), &[&k, &e], &[], now);
    let good = data_sig(&k, DATA, t0 + DAY, &[], 10);
    assert!(check(&kr, &good).good);
    bad_with(check(&kr, &data_sig(&k, DATA, t0 - DAY, &[], 10)), "außerhalb");
    bad_with(check(&kr, &data_sig(&k, DATA, t0 + DAY, &sp(3, &DAY.to_be_bytes()), 10)), "Signatur abgelaufen");
    bad_with(check(&kr, &data_sig(&e, DATA, t0 + DAY, &[], 10)), "abgelaufen");
    let early = ring(keys.clone(), &[&k], &[], clock_floor() - 1);
    bad_with(early.verify_detached(DATA, &good), "Uhr falsch");
    for i in 0..DATA.len() {
        let mut d = DATA.to_vec();
        d[i] ^= 1 << (i % 8);
        bad_with(kr.verify_detached(&d[..], &good), "passt nicht");
    }
    let mut longer = DATA.to_vec();
    longer.push(0);
    bad_with(kr.verify_detached(&longer[..], &good), "passt nicht");
    let stranger = tkey(52, 4, 22, t0);
    bad_with(check(&kr, &data_sig(&stranger, DATA, t0 + DAY, &[], 10)), "nicht im Schlüsselbund");
    let k2 = tkey(53, 4, 27, t0);
    let mut keys2 = keys.clone();
    keys2.extend(tpk(&k2, "K2 <k2@test>", t0, None));
    let kr2 = ring(keys2, &[&k, &k2], &[], now);
    let mut two = good.clone();
    two.extend(data_sig(&k2, DATA, t0 + 2 * DAY, &[], 8));
    assert!(check(&kr2, &two).good);
    let mut twice = good.clone();
    twice.extend(data_sig(&k, DATA, t0 + 2 * DAY, &[], 8));
    bad_with(check(&kr2, &twice), "desselben Schlüssels");
    two.extend(data_sig(&stranger, DATA, t0 + DAY, &[], 10));
    assert!(!check(&kr2, &two).good);
    bad_with(check(&kr, &pkt(2, &sign(&k, 4, 0x01, 10, &created(t0 + DAY), &[], DATA))), "Binärdaten");
}

#[test]
fn legacy_eddsa_short_r_and_s() {
    let (t0, now) = times();
    let k = tkey(60, 4, 22, t0);
    let kr = ring(tpk(&k, "K <k@test>", t0, None), &[&k], &[], now);
    let (mut short_r, mut short_s) = (false, false);
    for i in 0u32..20000 {
        let data = i.to_be_bytes();
        let s = data_sig(&k, &data, t0 + DAY, &[], 10);
        // pkt() writes a six-octet header; the parsed values are R || S, zero-padded back to 32 octets each.
        let sig = bitspkg::pgp::sig::Sig::parse(&s[6..]).unwrap();
        let v = kr.verify_detached(&data[..], &s);
        assert!(v.good, "{v:?}");
        if let bitspkg::pgp::key::SigMat::Ed(rs) = sig.mat {
            short_r |= rs[0] == 0;
            short_s |= rs[32] == 0;
        }
        if short_r && short_s {
            return;
        }
    }
    panic!("no signature with a short R and a short S found (r {short_r}, s {short_s})");
}

#[test]
fn sha1_and_critical_revocations_still_revoke() {
    let (t0, now) = times();
    let (m, mut keys) = mains(t0);
    let k = tkey(70, 4, 27, t0);
    keys.extend(tpk(&k, "K <k@test>", t0, None));
    keys.extend(revoke_x(&k, None, t0 + DAY, 2, &[]));
    let c = tkey(71, 4, 27, t0);
    keys.extend(tpk(&c, "C <c@test>", t0, None));
    keys.extend(revoke_x(&c, None, t0 + DAY, 10, &sp(0x80 | 99, b"x")));
    let p = tkey(72, 4, 27, t0);
    let s = tkey(73, 4, 22, t0);
    keys.extend(tpk(&p, "P <p@test>", t0, None));
    keys.extend(subkey(&p, &s, t0, 0x02, None, true));
    keys.extend(revoke_x(&p, Some(&s), t0 + DAY, 2, &[]));
    let q = tkey(74, 4, 27, t0);
    keys.extend(tpk(&q, "Q <q@test>", t0, None));
    for mk in &m {
        keys.extend(certify(mk, &q, "Q <q@test>", t0 + DAY, 0x10));
    }
    keys.extend(certify_x(&m[0], &q, "Q <q@test>", t0 + 2 * DAY, 0x30, 2, &[]));
    let kr = ring(keys, &[&m[0], &m[1], &m[2], &k, &c, &p], &[], now);
    bad_with(check(&kr, &data_sig(&k, DATA, t0 + 2 * DAY, &[], 10)), "widerrufen");
    bad_with(check(&kr, &data_sig(&c, DATA, t0 + 2 * DAY, &[], 10)), "widerrufen");
    assert!(check(&kr, &data_sig(&p, DATA, t0 + 2 * DAY, &[], 10)).good);
    bad_with(check(&kr, &data_sig(&s, DATA, t0 + 2 * DAY, &[], 10)), "widerrufen");
    bad_with(check(&kr, &data_sig(&q, DATA, t0 + 3 * DAY, &[], 10)), "2 von 3");
}

#[test]
fn non_revocable_certification_survives_its_revocation() {
    let (t0, now) = times();
    let (m, mut keys) = mains(t0);
    let q = tkey(80, 4, 27, t0);
    keys.extend(tpk(&q, "Q <q@test>", t0, None));
    keys.extend(certify_x(&m[0], &q, "Q <q@test>", t0 + DAY, 0x10, 10, &sp(7, &[0])));
    for mk in &m[1..] {
        keys.extend(certify(mk, &q, "Q <q@test>", t0 + DAY, 0x10));
    }
    keys.extend(certify(&m[0], &q, "Q <q@test>", t0 + 2 * DAY, 0x30));
    let kr = ring(keys, &[&m[0], &m[1], &m[2]], &[], now);
    assert!(check(&kr, &data_sig(&q, DATA, t0 + 3 * DAY, &[], 10)).good);
}

#[test]
fn future_keys_and_signatures_are_refused() {
    let (t0, now) = times();
    let k = tkey(90, 4, 27, t0);
    let f = tkey(91, 4, 27, now as u32 + DAY);
    let mut keys = tpk(&k, "K <k@test>", t0, None);
    keys.extend(tpk(&f, "F <f@test>", now as u32 + DAY, None));
    let kr = ring(keys, &[&k, &f], &[], now);
    bad_with(check(&kr, &data_sig(&k, DATA, now as u32 + 3600, &[], 10)), "Zukunft");
    assert!(check(&kr, &data_sig(&k, DATA, now as u32 + 60, &[], 10)).good, "inside the five minutes of drift");
    bad_with(check(&kr, &data_sig(&f, DATA, now as u32 + DAY, &[], 10)), "Zukunft");
}

#[test]
fn trust_levels_and_missing_signature() {
    let (t0, now) = times();
    let (m, mut keys) = mains(t0);
    let u = tkey(100, 4, 27, t0);
    keys.extend(tpk(&u, "U <u@test>", t0, None));
    let kr = ring(keys, &[&m[0], &m[1], &m[2]], &[], now);
    let v = check(&kr, &data_sig(&u, DATA, t0 + DAY, &[], 10));
    assert!(!v.good && v.trust == Trust::Unknown, "{v:?}");
    assert_eq!(check(&kr, &[]).trust, Trust::Missing);
    let stranger = tkey(101, 4, 27, t0);
    assert_eq!(check(&kr, &data_sig(&stranger, DATA, t0 + DAY, &[], 10)).trust, Trust::Invalid);
}

/// A hostile .sig file must not make the data be hashed per signature: refusals that need no digest come
/// before one byte is read.
#[test]
fn hostile_signature_files_read_no_data() {
    let (t0, now) = times();
    let k = tkey(110, 4, 27, t0);
    let kr = ring(tpk(&k, "K <k@test>", t0, None), &[&k], &[], now);
    // The smallest v4 signature that parses: type 0, RSA, SHA-256, creation time, no issuer, empty MPI.
    let minimal = [0xc2, 0x12, 4, 0, 1, 8, 0, 6, 5, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    let many: Vec<u8> = minimal.iter().copied().cycle().take(minimal.len() * 50_000).collect();
    let mut g = Gen::new(1 << 40);
    bad_with(kr.verify_detached(&mut g, &many), "nicht im Schlüsselbund");
    assert_eq!(g.reads, 0);
    let one = data_sig(&k, DATA, t0 + DAY, &[], 10);
    let copies = (1 << 20) / one.len();
    let rep: Vec<u8> = one.iter().copied().cycle().take(one.len() * copies).collect();
    let mut g = Gen::new(1 << 40);
    bad_with(kr.verify_detached(&mut g, &rep), "desselben Schlüssels");
    assert_eq!(g.reads, 0, "{copies} copies of one signature");
}

/// The data is streamed: reads of at most 256 KiB, nothing held. Georg's 6.11. rule: 32 MiB here instead of the
/// brief's 512 MiB (the chunk bound is what shows the streaming; the RSA path is checked on a real package in
/// pgp_vectors).
#[test]
fn data_is_streamed_in_chunks() {
    let (t0, now) = times();
    let k = tkey(120, 4, 22, t0);
    let k6 = tkey(121, 6, 27, t0);
    let mut keys = tpk(&k, "K <k@test>", t0, None);
    keys.extend(tpk(&k6, "K6 <k6@test>", t0, None));
    let kr = ring(keys, &[&k, &k6], &[], now);
    let len = 32u64 << 20;
    let data: Vec<u8> = (0..len).map(Gen::byte).collect();
    let mut sigs = data_sig(&k, &data, t0 + DAY, &[], 10);
    sigs.extend(data_sig(&k6, &data, t0 + DAY, &[], 8));
    drop(data);
    let mut g = Gen::new(len);
    let v = kr.verify_detached(&mut g, &sigs);
    assert!(v.good, "{v:?}");
    assert_eq!(g.pos, len);
    assert!(g.max_req <= 256 << 10 && g.reads > 100, "max read {} in {} reads", g.max_req, g.reads);
}

/// A key revocation (0x20) of `target` made by `by`.
fn revoke_by(by: &TKey, target: &TKey, t: u32) -> Vec<u8> {
    let mut h = created(t);
    h.extend(sp(29, b"\x02compromised"));
    h.extend(issuer(by));
    pkt(2, &sign(by, by.version, 0x20, 10, &h, &[], &target.key.hashed()))
}

/// Key `x` with a direct-key self-signature naming `r` as revoker in the hashed area, the unhashed area or
/// nowhere, and `r`'s revocation of `x`.
fn designated(x: &TKey, uid: &str, r: &TKey, place: u8, t0: u32) -> Vec<u8> {
    let mut rk = vec![0x80, r.algo];
    rk.extend_from_slice(&r.key.fpr);
    let rk = sp(12, &rk);
    let mut h = created(t0 + DAY);
    h.extend(issuer(x));
    if place == b'h' {
        h.extend_from_slice(&rk);
    }
    let u = if place == b'u' { rk } else { vec![] };
    let mut v = pkt(6, &x.body);
    v.extend(pkt(2, &sign(x, x.version, 0x1F, 10, &h, &u, &x.key.hashed())));
    v.extend(revoke_by(r, x, t0 + 2 * DAY));
    let tp = tpk(x, uid, t0, None);
    v.extend_from_slice(&tp[pkt(6, &x.body).len()..]);
    v
}

/// Designated revokers (verifier CR after PK-6b): a 0x20 by a key named in subpacket 12 of a hashed
/// direct-key self-signature revokes, even when that revoker is not valid itself; named only in the unhashed
/// area or not named, the third-party revocation means nothing. gpg on a scratch GNUPGHOME agrees.
#[test]
fn designated_revokers_revoke_as_gpg() {
    let (t0, now) = times();
    let (m, mut keys) = mains(t0);
    let r = tkey(41, 4, 22, t0);
    let set = [(tkey(42, 4, 22, t0), "K <k@test>", b'h'), (tkey(43, 4, 22, t0), "U <u@test>", b'u'), (tkey(44, 4, 22, t0), "N <n@test>", b'n')];
    let mut theirs = tpk(&r, "R <r@test>", t0, None);
    keys.extend(theirs.clone());
    for (x, uid, place) in &set {
        let b = designated(x, uid, &r, *place, t0);
        theirs.extend_from_slice(&b);
        keys.extend(b);
        for k in &m {
            keys.extend(certify(k, x, uid, t0 + DAY, 0x10));
        }
    }
    let kr = ring(keys, &[&m[0], &m[1], &m[2]], &[], now);
    bad_with(check(&kr, &data_sig(&set[0].0, DATA, t0 + 5 * DAY, &[], 10)), "benannten Widerrufer");
    assert!(check(&kr, &data_sig(&set[1].0, DATA, t0 + 5 * DAY, &[], 10)).good);
    assert!(check(&kr, &data_sig(&set[2].0, DATA, t0 + 5 * DAY, &[], 10)).good);

    // gpg's view of the same packets: field 2 of the pub line is "r" for a revoked key.
    use std::process::Command;
    let home = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("gpg-designated");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let file = home.join("keys.gpg");
    std::fs::write(&file, &theirs).unwrap();
    let gpg = |args: &[&str]| {
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args(["--batch", "--no-tty", "--no-auto-check-trustdb"])
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("RED: gpg missing on the build host ({e}); this test needs it"))
    };
    let imp = gpg(&["--import", file.to_str().unwrap()]);
    let list = gpg(&["--with-colons", "--list-keys"]);
    let _ = Command::new("gpgconf").arg("--homedir").arg(&home).args(["--kill", "all"]).status();
    let out = String::from_utf8_lossy(&list.stdout).to_string();
    let validity = |k: &TKey| {
        let lines: Vec<&str> = out.lines().collect();
        let at = lines.iter().position(|l| l.starts_with("fpr:") && l.contains(&k.key.fpr_hex().to_uppercase()));
        at.and_then(|a| lines[..a].iter().rev().find(|l| l.starts_with("pub:"))).map(|l| l.split(':').nth(1).unwrap_or("").to_string())
    };
    let why = format!("gpg import: {}\nlist: {out}", String::from_utf8_lossy(&imp.stderr));
    assert_eq!(validity(&set[0].0).as_deref(), Some("r"), "{why}");
    assert_ne!(validity(&set[1].0).as_deref(), Some("r"), "{why}");
    assert_ne!(validity(&set[2].0).as_deref(), Some("r"), "{why}");
    assert!(validity(&set[1].0).is_some() && validity(&set[2].0).is_some(), "{why}");
}
