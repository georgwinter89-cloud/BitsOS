//! Ed25519 over the manifest's bytes (ring). The desk signs with the private key that lives only on its host
//! (`cargo xtask update-release` runs `bitsupdate keygen` once and `bitsupdate sign` per version); BitsOS checks
//! against the public half compiled in (`DESK_KEY`, from keys/release.pub). The signature file is 128 hex digits
//! and a newline.

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/key.rs"));

/// A new key pair as PKCS#8 (what `sign` takes) and its public half.
pub fn keygen() -> Result<(Vec<u8>, [u8; 32]), String> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| String::from("no random numbers for a key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| String::from("the new key does not load"))?;
    let mut public = [0u8; 32];
    public.copy_from_slice(pair.public_key().as_ref());
    Ok((pkcs8.as_ref().to_vec(), public))
}

/// The public half of a PKCS#8 key.
pub fn public_of(pkcs8: &[u8]) -> Result<[u8; 32], String> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| String::from("the key file is no Ed25519 PKCS#8 key"))?;
    let mut public = [0u8; 32];
    public.copy_from_slice(pair.public_key().as_ref());
    Ok(public)
}

/// The signature file's text for `msg`.
pub fn sign(pkcs8: &[u8], msg: &[u8]) -> Result<String, String> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| String::from("the key file is no Ed25519 PKCS#8 key"))?;
    Ok(format!("{}\n", crate::manifest::hex(pair.sign(msg).as_ref())))
}

/// Whether `sig_text` (the signature file) is `key`'s signature of `msg`.
pub fn verify(key: &[u8; 32], msg: &[u8], sig_text: &[u8]) -> bool {
    let Ok(t) = std::str::from_utf8(sig_text) else { return false };
    let t = t.strip_suffix('\n').unwrap_or(t);
    if t.len() != 128 || !t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return false;
    }
    let sig: Vec<u8> = (0..64).filter_map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).ok()).collect();
    UnparsedPublicKey::new(&ED25519, key).verify(msg, &sig).is_ok()
}

/// What the desk and the page say about a key: "ab12 cd34 ef56 7890" (the first 8 bytes of its SHA-256).
pub fn fingerprint(key: &[u8; 32]) -> String {
    let h = Sha256::digest(key);
    h[..8].chunks(2).map(crate::manifest::hex).collect::<Vec<_>>().join(" ")
}

/// The public key file's text (keys/release.pub).
pub fn public_file(key: &[u8; 32]) -> String {
    format!(
        "# The desk's release key (Ed25519, public half; fingerprint {}), compiled into BitsOS by build.rs.\n# Made by cargo xtask update-release; the private half lives only in ~/.config/bitsos-release/ on the desk host.\n{}\n",
        fingerprint(key),
        crate::manifest::hex(key)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let (k, public) = keygen().unwrap();
        assert_eq!(public_of(&k).unwrap(), public);
        let s = sign(&k, b"hello").unwrap();
        assert!(s.ends_with('\n') && s.len() == 129);
        assert!(verify(&public, b"hello", s.as_bytes()));
        assert!(verify(&public, b"hello", s.trim_end().as_bytes()), "without the newline too");
        assert!(!verify(&public, b"hellO", s.as_bytes()));
        let (_, other) = keygen().unwrap();
        assert!(!verify(&other, b"hello", s.as_bytes()));
        assert!(!verify(&public, b"hello", s.to_uppercase().as_bytes()));
        assert!(!verify(&public, b"hello", &s.as_bytes()[..100]));
        assert_eq!(fingerprint(&public).len(), 19);
        assert!(public_file(&public).lines().last().unwrap().len() == 64);
    }
}
