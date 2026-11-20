//! Public key packets v4 and v6 (RFC 9580 5.5.2): fingerprint, key ID, the octets a signature over the key hashes
//! (5.2.4), and the signature check with ring. Algorithms: RSA (1), EdDSA legacy on Ed25519 (22, v4 only) and
//! Ed25519 (27). Every other algorithm is kept as `Other` so its key can be named, but it verifies nothing.

use super::packet::{Rd, hex};
use super::rsa;
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, SHA256, digest};
use ring::signature::{self as rs, UnparsedPublicKey};

/// The curve OID of Ed25519 for algorithm 22 (RFC 9580 9.2, 1.3.6.1.4.1.11591.15.1).
const OID_ED25519: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0xda, 0x47, 0x0f, 0x01];

pub const ALG_RSA: u8 = 1;
pub const ALG_EDDSA_LEGACY: u8 = 22;
pub const ALG_ED25519: u8 = 27;

#[derive(Clone, Debug)]
pub enum Material {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ed25519([u8; 32]),
    Other(String),
}

#[derive(Clone, Debug)]
pub struct Key {
    pub version: u8,
    pub created: u32,
    pub algo: u8,
    pub material: Material,
    pub body: Vec<u8>,
    pub fpr: Vec<u8>,
}

/// The signature values of 5.2.3: RSA's MPI, or Ed25519's R || S (legacy MPIs zero-padded to 32 octets each).
#[derive(Clone, Debug)]
pub enum SigMat {
    Rsa(Vec<u8>),
    Ed([u8; 64]),
    Other,
}

fn alg_name(a: u8) -> String {
    match a {
        2 | 3 => "RSA (veraltete Variante)".into(),
        16 | 20 => "ElGamal".into(),
        17 => "DSA".into(),
        18 => "ECDH".into(),
        19 => "ECDSA".into(),
        25 | 26 => "X25519/X448".into(),
        28 => "Ed448".into(),
        _ => format!("Verfahren {a}"),
    }
}

impl Key {
    pub fn parse(body: &[u8]) -> Result<Key, String> {
        let mut r = Rd::new(body);
        let version = r.u8()?;
        if version != 4 && version != 6 {
            return Err(format!("Schlüssel der Version {version} wird nicht angenommen (nur 4 und 6)"));
        }
        let created = r.u32()?;
        let algo = r.u8()?;
        let mat = if version == 6 {
            let n = r.u32()? as usize;
            let m = r.take(n)?;
            if r.left() != 0 {
                return Err("v6-Schlüssel: Daten nach dem Schlüsselmaterial".into());
            }
            m
        } else {
            r.rest()
        };
        let mut m = Rd::new(mat);
        let material = match (algo, version) {
            (ALG_RSA, _) => {
                let n = m.mpi()?.to_vec();
                let e = m.mpi()?.to_vec();
                Material::Rsa { n, e }
            }
            (ALG_EDDSA_LEGACY, 4) => {
                let ol = m.u8()? as usize;
                let oid = m.take(ol)?;
                let q = m.mpi()?;
                if oid != OID_ED25519 {
                    Material::Other("EdDSA auf einer anderen Kurve als Ed25519".into())
                } else if q.len() != 33 || q[0] != 0x40 {
                    return Err("EdDSA-Schlüssel: Punkt nicht im 0x40-Format".into());
                } else {
                    Material::Ed25519(q[1..].try_into().map_err(|_| "EdDSA-Schlüssel: Länge")?)
                }
            }
            (ALG_EDDSA_LEGACY, _) => return Err("EdDSA (22) ist in v6-Schlüsseln verboten (RFC 9580 9.1)".into()),
            (ALG_ED25519, _) => Material::Ed25519(m.take(32)?.try_into().map_err(|_| "Ed25519-Schlüssel: Länge")?),
            _ => Material::Other(alg_name(algo)),
        };
        let fpr = if version == 4 {
            let mut v = vec![0x99, (body.len() >> 8) as u8, body.len() as u8];
            if body.len() > 0xffff {
                return Err("v4-Schlüssel länger als 65535 Bytes".into());
            }
            v.extend_from_slice(body);
            digest(&SHA1_FOR_LEGACY_USE_ONLY, &v).as_ref().to_vec()
        } else {
            let mut v = vec![0x9b];
            v.extend_from_slice(&(body.len() as u32).to_be_bytes());
            v.extend_from_slice(body);
            digest(&SHA256, &v).as_ref().to_vec()
        };
        Ok(Key { version, created, algo, material, body: body.to_vec(), fpr })
    }

    /// The key ID: the low 64 bits of a v4 fingerprint, the high 64 bits of a v6 one (RFC 9580 5.5.4).
    pub fn key_id(&self) -> [u8; 8] {
        let f = &self.fpr;
        let s = if self.version == 4 { &f[f.len() - 8..] } else { &f[..8] };
        s.try_into().unwrap_or([0; 8])
    }

    pub fn fpr_hex(&self) -> String {
        hex(&self.fpr)
    }

    /// The octets a signature over this key hashes (RFC 9580 5.2.4): 0x99 and a two-octet length for v4, 0x9B and a
    /// four-octet length for v6, then the packet body.
    pub fn hashed(&self) -> Vec<u8> {
        let mut v = if self.version == 4 {
            vec![0x99, (self.body.len() >> 8) as u8, self.body.len() as u8]
        } else {
            let mut v = vec![0x9b];
            v.extend_from_slice(&(self.body.len() as u32).to_be_bytes());
            v
        };
        v.extend_from_slice(&self.body);
        v
    }

    pub fn supported(&self) -> Result<(), String> {
        match &self.material {
            Material::Other(n) => Err(format!("{n} wird nicht angenommen")),
            Material::Rsa { n, e } => rsa::check_key(n, e).map(|_| ()),
            Material::Ed25519(_) => Ok(()),
        }
    }

    /// Checks signature values against this key. `digest` is the hash (with trailer) of the signed data; `whole` is
    /// that hashed input itself when it is in memory (then RSA goes through ring, which hashes it again).
    pub fn check(&self, hash: u8, digest: &[u8], whole: Option<&[u8]>, mat: &SigMat) -> Result<(), String> {
        let bad = |w: &str| Err(format!("Signatur passt nicht ({w})"));
        match (&self.material, mat) {
            (Material::Ed25519(pk), SigMat::Ed(s)) => {
                // EdDSA in OpenPGP signs the digest as its message (RFC 9580 5.2.3.3/5.2.3.4).
                match UnparsedPublicKey::new(&rs::ED25519, pk).verify(digest, s) {
                    Ok(()) => Ok(()),
                    Err(_) => bad("Ed25519"),
                }
            }
            (Material::Rsa { n, e }, SigMat::Rsa(s)) => match whole {
                Some(msg) => {
                    rsa::check_key(n, e)?;
                    let alg = match hash {
                        8 => &rs::RSA_PKCS1_2048_8192_SHA256,
                        9 => &rs::RSA_PKCS1_2048_8192_SHA384,
                        10 => &rs::RSA_PKCS1_2048_8192_SHA512,
                        // Only reachable from Sig::verify_revocation: every other path refuses SHA-1 first (sane()).
                        2 => &rs::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
                        _ => return Err("Hash-Verfahren für RSA nicht erlaubt".into()),
                    };
                    if s.len() > n.len() {
                        return bad("RSA");
                    }
                    // OpenPGP's MPI drops leading zero octets; ring wants the modulus' length.
                    let mut p = vec![0u8; n.len() - s.len()];
                    p.extend_from_slice(s);
                    match (rs::RsaPublicKeyComponents { n: &n[..], e: &e[..] }).verify(alg, msg, &p) {
                        Ok(()) => Ok(()),
                        Err(_) => bad("RSA"),
                    }
                }
                None => rsa::verify_digest(n, e, hash, digest, s),
            },
            (Material::Other(n), _) => Err(format!("{n} wird nicht angenommen")),
            _ => Err("Signaturverfahren passt nicht zum Schlüssel".into()),
        }
    }
}
