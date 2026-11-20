//! The checks before anything is written, one test per check, each case refused by the check it names: the
//! signature (a tampered manifest, a wrong key, no key, a damaged signature), the release's name, not newer
//! (an older validly signed manifest replayed), too old a BitsOS (`min_previous`), and each file's size and hash.

use crate::manifest::{Entry, Manifest, NOTES};
use crate::sig;
use crate::verify::{self, Why};

const KERNEL: [u8; 5000] = [7u8; 5000];
const INITRD: [u8; 3000] = [9u8; 3000];

fn release(version: u32, min: Option<u32>) -> Manifest {
    Manifest {
        version,
        date: "2026-11-16".into(),
        min_previous: min,
        files: vec![
            Entry { name: "KERNEL".into(), size: KERNEL.len() as u64, sha256: verify::sha256(&KERNEL) },
            Entry { name: "INITRD".into(), size: INITRD.len() as u64, sha256: verify::sha256(&INITRD) },
            Entry { name: NOTES.into(), size: 5, sha256: verify::sha256(b"neu!\n") },
        ],
        bins: vec![],
        trees: vec![],
    }
}

/// `m` rendered and signed with a key; the key's public half beside it.
struct Signed {
    key: Vec<u8>,
    public: [u8; 32],
    text: Vec<u8>,
    sig: Vec<u8>,
}

fn signed(m: &Manifest) -> Signed {
    let (key, public) = sig::keygen().unwrap();
    sign_with(key, public, m)
}

fn sign_with(key: Vec<u8>, public: [u8; 32], m: &Manifest) -> Signed {
    let text = m.render().into_bytes();
    let sig = sig::sign(&key, &text).unwrap().into_bytes();
    Signed { key, public, text, sig }
}

#[test]
fn a_signed_release_passes_every_check() {
    let m = release(166, Some(164));
    let s = signed(&m);
    // Medium newest 165, running 165.
    let ok = verify::manifest(Some(&s.public), &s.text, &s.sig, 166, 165, 165).unwrap();
    assert_eq!(ok, m);
    verify::file(&ok, "KERNEL", &KERNEL).unwrap();
    verify::file(&ok, "INITRD", &INITRD).unwrap();
    verify::file(&ok, NOTES, b"neu!\n").unwrap();
}

#[test]
fn the_signature_check_refuses_a_changed_manifest() {
    let s = signed(&release(166, Some(164)));
    let t = String::from_utf8(s.text.clone()).unwrap();
    for changed in [
        t.replace("KERNEL 5000", "KERNEL 5001"),
        t.replace("version 166", "version 167"),
        t.replace("min_previous 164\n", ""),
        format!("{t}file BOOTX64.EFI 1 {}\n", "0".repeat(64)),
    ] {
        assert_ne!(changed, t);
        assert_eq!(verify::manifest(Some(&s.public), changed.as_bytes(), &s.sig, 166, 165, 165), Err(Why::Signature), "{changed}");
    }
}

#[test]
fn the_signature_check_refuses_a_wrong_key_and_a_damaged_signature() {
    let s = signed(&release(166, None));
    let (_, other) = sig::keygen().unwrap();
    assert_eq!(verify::manifest(Some(&other), &s.text, &s.sig, 166, 165, 165), Err(Why::Signature));
    assert_eq!(verify::manifest(None, &s.text, &s.sig, 166, 165, 165), Err(Why::NoKey));
    assert_eq!(verify::manifest(Some(&s.public), &s.text, b"", 166, 165, 165), Err(Why::Signature));
    assert_eq!(verify::manifest(Some(&s.public), &s.text, &s.sig[..64], 166, 165, 165), Err(Why::Signature));
}

#[test]
fn the_name_check_refuses_a_release_named_for_another_version() {
    let s = signed(&release(165, None));
    // Under another release's name, above and below the manifest's own number.
    assert_eq!(verify::manifest(Some(&s.public), &s.text, &s.sig, 166, 160, 160), Err(Why::Tag { tag: 166, manifest: 165 }));
    assert_eq!(verify::manifest(Some(&s.public), &s.text, &s.sig, 164, 160, 160), Err(Why::Tag { tag: 164, manifest: 165 }));
    assert!(verify::manifest(Some(&s.public), &s.text, &s.sig, 165, 160, 160).is_ok());
}

#[test]
fn the_newer_check_refuses_a_replayed_older_manifest() {
    let s = signed(&release(165, None));
    // Equal to the medium's newest, and below it.
    assert_eq!(verify::manifest(Some(&s.public), &s.text, &s.sig, 165, 165, 165), Err(Why::NotNewer { version: 165, newest: 165 }));
    assert_eq!(verify::manifest(Some(&s.public), &s.text, &s.sig, 165, 170, 168), Err(Why::NotNewer { version: 165, newest: 170 }));
    assert!(verify::manifest(Some(&s.public), &s.text, &s.sig, 165, 164, 164).is_ok());
}

#[test]
fn the_min_previous_check_refuses_too_old_a_bitsos() {
    let (key, public) = sig::keygen().unwrap();
    let with = sign_with(key, public, &release(166, Some(164)));
    assert_eq!(verify::manifest(Some(&public), &with.text, &with.sig, 166, 165, 163), Err(Why::TooOld { need: 164, have: 163 }));
    assert_eq!(verify::manifest(Some(&public), &with.text, &with.sig, 166, 165, 0), Err(Why::TooOld { need: 164, have: 0 }), "a development build");
    assert!(verify::manifest(Some(&public), &with.text, &with.sig, 166, 165, 164).is_ok());
    // The same release signed without min_previous: the same BitsOS passes.
    let without = sign_with(with.key, public, &release(166, None));
    assert!(verify::manifest(Some(&public), &without.text, &without.sig, 166, 165, 163).is_ok());
}

#[test]
fn the_manifest_check_refuses_signed_junk() {
    let (key, public) = sig::keygen().unwrap();
    let junk = b"bitsos-update 1\nversion 166\n".to_vec();
    let junk_sig = sig::sign(&key, &junk).unwrap().into_bytes();
    assert!(matches!(verify::manifest(Some(&public), &junk, &junk_sig, 166, 165, 165), Err(Why::Manifest(_))));
}

#[test]
fn the_size_check_refuses_a_cut_short_or_longer_file() {
    let m = release(166, None);
    assert!(matches!(verify::file(&m, "INITRD", &INITRD[..2999]), Err(Why::File { ref what, .. }) if what.contains("2999 bytes")));
    let mut long = INITRD.to_vec();
    long.push(0);
    assert!(matches!(verify::file(&m, "INITRD", &long), Err(Why::File { ref what, .. }) if what.contains("3001 bytes")));
    assert!(matches!(verify::file(&m, "BOOTX64.EFI", b"x"), Err(Why::File { ref what, .. }) if what.contains("not in the manifest")));
}

#[test]
fn the_hash_check_refuses_a_changed_file_of_the_same_length() {
    let m = release(166, None);
    let mut bad = KERNEL.to_vec();
    bad[100] ^= 1;
    assert!(matches!(verify::file(&m, "KERNEL", &bad), Err(Why::File { ref what, .. }) if what.contains("SHA-256")));
}
