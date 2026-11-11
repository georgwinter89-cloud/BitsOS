// The release manifest's hash is part of the installer: a build with a malformed one must fail here, not at a
// tester's desk.
fn main() {
    println!("cargo:rerun-if-env-changed=BITSOS_RELEASE_MANIFEST_SHA256");
    println!("cargo:rerun-if-env-changed=BITSOS_RELEASE_VERSION");
    if let Ok(h) = std::env::var("BITSOS_RELEASE_MANIFEST_SHA256") {
        assert!(h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "BITSOS_RELEASE_MANIFEST_SHA256 must be 64 lower-case hex digits");
    }
    if let Ok(v) = std::env::var("BITSOS_RELEASE_VERSION") {
        assert!(!v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b)), "BITSOS_RELEASE_VERSION: letters, digits and ._-+ only");
    }
}
