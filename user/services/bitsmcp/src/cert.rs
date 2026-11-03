//! Self-signed X.509 certificates for the pinned keys of MC-8b (no CA anywhere: both sides compare the other's
//! whole certificate with the one they paired with). ECDSA P-256 with SHA-256, the key made by ring; the DER is
//! written by hand (RFC 5280 4.1), the smallest certificate rustls/webpki and OpenSSL take: version 3, a random
//! positive serial, issuer = subject = one common name, valid from 2000-01-01 to 9999-12-31 (RFC 5280 4.1.2.5:
//! "no well-defined expiration date"; the Mini-PC's clock is not trusted for a pinned key), no extensions.
//! host/mcp/mcp-keys.mjs builds the Gaming-PC's certificate the same way.

use ring::rand::SecureRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

/// id-ecPublicKey, prime256v1, ecdsa-with-SHA256, commonName (DER contents of the OIDs).
const OID_EC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const OID_CN: &[u8] = &[0x55, 0x04, 0x03];

/// One DER element: tag, definite length, contents.
pub fn tlv(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = vec![tag];
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

fn name(cn: &str) -> Vec<u8> {
    let atv = tlv(0x30, &[&tlv(0x06, &[OID_CN]), &tlv(0x0c, &[cn.as_bytes()])]);
    tlv(0x30, &[&tlv(0x31, &[&atv])])
}

/// The certificate of `key` signed by itself, named `cn`.
pub fn self_signed(key: &EcdsaKeyPair, rng: &dyn SecureRandom, cn: &str) -> Result<Vec<u8>, String> {
    let mut serial = [0u8; 16];
    rng.fill(&mut serial).map_err(|_| "no random bytes for the serial".to_string())?;
    // Positive and minimal: the top bit clear, the first byte not zero.
    serial[0] = (serial[0] & 0x7f) | 0x01;
    let alg = tlv(0x30, &[&tlv(0x06, &[OID_ECDSA_SHA256])]);
    let validity = tlv(0x30, &[&tlv(0x17, &[b"000101000000Z"]), &tlv(0x18, &[b"99991231235959Z"])]);
    let spki = tlv(0x30, &[&tlv(0x30, &[&tlv(0x06, &[OID_EC_KEY]), &tlv(0x06, &[OID_P256])]), &tlv(0x03, &[&[0], key.public_key().as_ref()])]);
    let tbs = tlv(0x30, &[&tlv(0xa0, &[&tlv(0x02, &[&[2]])]), &tlv(0x02, &[&serial]), &alg, &name(cn), &validity, &name(cn), &spki]);
    let sig = key.sign(rng, &tbs).map_err(|_| "signing the certificate failed".to_string())?;
    Ok(tlv(0x30, &[&tbs, &alg, &tlv(0x03, &[&[0], sig.as_ref()])]))
}

/// A new P-256 key (PKCS#8 DER) and its self-signed certificate (DER).
pub fn generate(rng: &dyn SecureRandom, cn: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, rng).map_err(|_| "generating a P-256 key failed".to_string())?;
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), rng).map_err(|e| format!("the new key: {e}"))?;
    let cert = self_signed(&key, rng, cn)?;
    Ok((pkcs8.as_ref().to_vec(), cert))
}

/// True when `key` (PKCS#8) is a P-256 key whose public half `cert` carries and `cert` parses as a certificate.
pub fn key_fits(key: &[u8], cert: &[u8], rng: &dyn SecureRandom) -> bool {
    let Ok(k) = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, key, rng) else { return false };
    let public = k.public_key().as_ref();
    is_certificate(cert) && cert.windows(public.len()).any(|w| w == public)
}

/// The SHA-256 of the whole DER certificate in lower-case hex: the name of a pinned key in logs and files.
pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// True when webpki parses `der` as an end-entity certificate (what rustls needs to check its signatures).
pub fn is_certificate(der: &[u8]) -> bool {
    let c = rustls_pki_types::CertificateDer::from(der);
    webpki::EndEntityCert::try_from(&c).is_ok()
}

/// The largest certificate a pairing takes: a pinned self-signed P-256 certificate is about 350 bytes; the cap
/// keeps a hostile pairing request from being kept on the stick and in memory for good.
pub const CERT_MAX: usize = 16 * 1024;

/// One DER element at the start of `d`: (tag, contents, the whole element, the rest). Only definite lengths of
/// at most four bytes (DER); None for anything else.
fn read_tlv(d: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let (&tag, rest) = d.split_first()?;
    let (&l0, rest) = rest.split_first()?;
    let (len, rest) = if l0 < 0x80 {
        (l0 as usize, rest)
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        (rest[..n].iter().fold(0usize, |a, &b| (a << 8) | b as usize), &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    let head = d.len() - rest.len();
    Some((tag, &rest[..len], &d[..head + len], &rest[len..]))
}

/// Whether `der` can be a paired client's certificate: at most `CERT_MAX` bytes, a certificate webpki parses,
/// its key a P-256 key and the certificate signed by that key with ECDSA SHA-256 (self-signed: the host that
/// sends it holds the private key, and the key is one the TLS 1.3 handshake can use). A pairing that fails here
/// does not spend the one-time pairing.
pub fn client_usable(der: &[u8]) -> Result<(), String> {
    if der.len() > CERT_MAX {
        return Err(format!("the certificate has {} bytes, more than {CERT_MAX}", der.len()));
    }
    let c = rustls_pki_types::CertificateDer::from(der);
    let ee = webpki::EndEntityCert::try_from(&c).map_err(|_| String::from("the certificate does not parse"))?;
    let parts = read_tlv(der).filter(|(tag, _, whole, rest)| *tag == 0x30 && whole.len() == der.len() && rest.is_empty()).and_then(|(_, body, _, _)| {
        let (_, _, tbs, rest) = read_tlv(body)?;
        let (_, alg, _, rest) = read_tlv(rest)?;
        let (tag, sig, _, rest) = read_tlv(rest)?;
        (tag == 0x03 && rest.is_empty()).then_some((tbs, alg, sig))
    });
    let Some((tbs, alg, sig)) = parts else { return Err(String::from("the certificate does not parse")) };
    let (otag, oid, _, _) = read_tlv(alg).ok_or_else(|| String::from("the certificate does not parse"))?;
    let Some((&0, sig)) = sig.split_first() else { return Err(String::from("the certificate's signature does not parse")) };
    if otag != 0x06 || oid != OID_ECDSA_SHA256 {
        return Err(String::from("the certificate is not signed with ECDSA SHA-256 (a P-256 key is needed)"));
    }
    // webpki's P-256 algorithm takes only a P-256 key: a P-384 or RSA key fails here as well.
    ee.verify_signature(webpki::ring::ECDSA_P256_SHA256, tbs, sig)
        .map_err(|_| String::from("the certificate is not signed by its own P-256 key (a self-signed P-256 certificate is needed)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths() {
        assert_eq!(tlv(0x04, &[&[1, 2]]), [0x04, 2, 1, 2]);
        assert_eq!(&tlv(0x04, &[&[0; 200]])[..3], [0x04, 0x81, 200]);
        assert_eq!(&tlv(0x04, &[&[0; 300]])[..4], [0x04, 0x82, 1, 44]);
    }

    #[test]
    fn a_generated_certificate_parses_and_fits_its_key() {
        let rng = ring::rand::SystemRandom::new();
        let (key, cert) = generate(&rng, "bitsmcp").unwrap();
        assert!(is_certificate(&cert));
        assert!(key_fits(&key, &cert, &rng));
        let (other, _) = generate(&rng, "bitsmcp").unwrap();
        assert!(!key_fits(&other, &cert, &rng), "another key does not fit");
        assert_eq!(fingerprint(&cert).len(), 64);
        assert!(!is_certificate(b"POST /mcp HTTP/1.1\r\n"));
    }

    #[test]
    fn a_client_certificate_must_be_self_signed_p256() {
        let rng = ring::rand::SystemRandom::new();
        let (_, cert) = generate(&rng, "desk").unwrap();
        assert_eq!(client_usable(&cert), Ok(()));
        // The signature's last byte changed: still a certificate, but nobody holds its key.
        let mut bad = cert.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(is_certificate(&bad) && client_usable(&bad).is_err());
        // A P-384 key, self-signed with ECDSA SHA-384: parses, but is no key for this pairing.
        use ring::signature::ECDSA_P384_SHA384_ASN1_SIGNING;
        let p8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P384_SHA384_ASN1_SIGNING, &rng).unwrap();
        let k = EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_ASN1_SIGNING, p8.as_ref(), &rng).unwrap();
        let alg = tlv(0x30, &[&tlv(0x06, &[&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03]])]);
        let validity = tlv(0x30, &[&tlv(0x17, &[b"000101000000Z"]), &tlv(0x18, &[b"99991231235959Z"])]);
        let spki = tlv(0x30, &[&tlv(0x30, &[&tlv(0x06, &[OID_EC_KEY]), &tlv(0x06, &[&[0x2b, 0x81, 0x04, 0x00, 0x22]])]), &tlv(0x03, &[&[0], k.public_key().as_ref()])]);
        let tbs = tlv(0x30, &[&tlv(0xa0, &[&tlv(0x02, &[&[2]])]), &tlv(0x02, &[&[1]]), &alg, &name("p384"), &validity, &name("p384"), &spki]);
        let sig = k.sign(&rng, &tbs).unwrap();
        let p384 = tlv(0x30, &[&tbs, &alg, &tlv(0x03, &[&[0], sig.as_ref()])]);
        assert!(is_certificate(&p384), "webpki parses the P-384 certificate");
        assert!(client_usable(&p384).is_err());
        assert!(client_usable(&[]).is_err() && client_usable(&vec![0x30; CERT_MAX + 1]).is_err());
    }
}
