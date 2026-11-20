//! Signature packets v4 and v6 (RFC 9580 5.2.3), their subpackets (5.2.3.7) and the hashed trailer (5.2.4).
//! Hashes: SHA-256, SHA-384, SHA-512 through sha2; SHA-1, MD5 and all others are refused, as gpg refuses SHA-1
//! data signatures. Only revocations may use SHA-1 (`verify_revocation`): they only take validity away.

use super::key::{ALG_ED25519, ALG_EDDSA_LEGACY, ALG_RSA, Key, SigMat};
use super::packet::{MAX_PACKET, Rd};
use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use sha2::{Digest, Sha256, Sha384, Sha512};

pub const T_BINARY: u8 = 0x00;
pub const T_SUBKEY_BIND: u8 = 0x18;
pub const T_PRIMARY_BIND: u8 = 0x19;
pub const T_DIRECT: u8 = 0x1f;
pub const T_KEY_REV: u8 = 0x20;
pub const T_SUBKEY_REV: u8 = 0x28;
pub const T_CERT_REV: u8 = 0x30;

pub fn is_cert(t: u8) -> bool {
    (0x10..=0x13).contains(&t)
}

#[derive(Clone, Debug)]
pub struct Sig {
    pub version: u8,
    pub typ: u8,
    pub pub_algo: u8,
    pub hash_algo: u8,
    /// Version through the hashed subpackets: the part the trailer hashes.
    pub hashed: Vec<u8>,
    pub created: Option<u32>,
    pub sig_expiry: u32,
    pub key_expiry: u32,
    pub issuer_fpr: Option<Vec<u8>>,
    pub issuer_id: Option<[u8; 8]>,
    pub key_flags: Option<u8>,
    pub primary_uid: bool,
    pub exportable: bool,
    /// Subpacket 7: a certification marked non-revocable ignores later revocations (RFC 9580 5.2.3.20).
    pub revocable: bool,
    pub revocation: Option<(u8, String)>,
    /// Subpacket 12 (hashed only): primary fingerprints of designated revokers, class bit 0x80 set, as gpg's
    /// parse_revkeys reads them. Bounded by the signature packet (64 KiB).
    pub revokers: Vec<Vec<u8>>,
    pub embedded: Option<Box<Sig>>,
    /// The first critical subpacket this verifier does not know: the signature is invalid (RFC 9580 5.2.3.7).
    pub critical_unknown: Option<u8>,
    pub left16: [u8; 2],
    pub salt: Vec<u8>,
    pub mat: SigMat,
}

/// Subpacket types read or deliberately ignored. A critical one outside this list invalidates the signature;
/// so does a critical notation (20) or regular expression (6), which this verifier does not evaluate.
const KNOWN: &[u8] = &[2, 3, 4, 5, 7, 9, 11, 12, 16, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 39];

#[derive(Clone)]
pub enum Hasher {
    S256(Sha256),
    S384(Sha384),
    S512(Sha512),
    /// SHA-1, only ever made by `new_revocation`.
    S1(Context),
}

impl Hasher {
    pub fn new(alg: u8) -> Result<Hasher, String> {
        Ok(match alg {
            8 => Hasher::S256(Sha256::new()),
            9 => Hasher::S384(Sha384::new()),
            10 => Hasher::S512(Sha512::new()),
            1 => return Err("MD5-Signaturen werden nicht angenommen".into()),
            2 => return Err("SHA-1-Signaturen werden nicht angenommen".into()),
            a => return Err(format!("Hash-Verfahren {a} wird nicht angenommen (nur SHA-256, SHA-384, SHA-512)")),
        })
    }
    /// The hash of a revocation: the data hashes, and also SHA-1, which gpg 2.4 still honours in revocations.
    pub fn new_revocation(alg: u8) -> Result<Hasher, String> {
        if alg == 2 { Ok(Hasher::S1(Context::new(&SHA1_FOR_LEGACY_USE_ONLY))) } else { Self::new(alg) }
    }
    pub fn update(&mut self, d: &[u8]) {
        match self {
            Hasher::S256(h) => h.update(d),
            Hasher::S384(h) => h.update(d),
            Hasher::S512(h) => h.update(d),
            Hasher::S1(h) => h.update(d),
        }
    }
    fn finish(self) -> Vec<u8> {
        match self {
            Hasher::S256(h) => h.finalize().to_vec(),
            Hasher::S384(h) => h.finalize().to_vec(),
            Hasher::S512(h) => h.finalize().to_vec(),
            Hasher::S1(h) => h.finish().as_ref().to_vec(),
        }
    }
}

/// The salt size of a v6 signature by hash algorithm (RFC 9580 table 23).
fn salt_len(hash: u8) -> Option<usize> {
    match hash {
        8 => Some(16),
        9 => Some(24),
        10 => Some(32),
        _ => None,
    }
}

impl Sig {
    pub fn parse(body: &[u8]) -> Result<Sig, String> {
        Self::parse_at(body, 0)
    }

    /// `depth` 1 is an embedded signature (5.2.3.34), which may not embed another: no unbounded recursion.
    fn parse_at(body: &[u8], depth: u8) -> Result<Sig, String> {
        if body.len() > MAX_PACKET {
            return Err(format!("Signaturpaket mit {} Bytes ist zu groß (höchstens 64 KiB)", body.len()));
        }
        let mut r = Rd::new(body);
        let version = r.u8()?;
        if version != 4 && version != 6 {
            return Err(format!("Signatur der Version {version} wird nicht angenommen (nur 4 und 6)"));
        }
        let typ = r.u8()?;
        let pub_algo = r.u8()?;
        let hash_algo = r.u8()?;
        let hl = if version == 4 { r.u16()? as usize } else { r.u32()? as usize };
        let hsub = r.take(hl)?;
        let hashed = body[..r.pos()].to_vec();
        let ul = if version == 4 { r.u16()? as usize } else { r.u32()? as usize };
        let usub = r.take(ul)?;
        let l = r.take(2)?;
        let left16 = [l[0], l[1]];
        let salt = if version == 6 {
            let n = r.u8()? as usize;
            if Some(n) != salt_len(hash_algo) {
                return Err("v6-Signatur: Salzlänge passt nicht zum Hash-Verfahren".into());
            }
            r.take(n)?.to_vec()
        } else {
            Vec::new()
        };
        let mat = match pub_algo {
            ALG_RSA => SigMat::Rsa(r.mpi()?.to_vec()),
            ALG_EDDSA_LEGACY if version == 4 => {
                let mut s = [0u8; 64];
                for half in 0..2 {
                    let v = r.mpi()?;
                    if v.len() > 32 {
                        return Err("EdDSA-Signatur: Wert länger als 32 Bytes".into());
                    }
                    s[half * 32 + 32 - v.len()..half * 32 + 32].copy_from_slice(v);
                }
                SigMat::Ed(s)
            }
            ALG_ED25519 => SigMat::Ed(r.take(64)?.try_into().map_err(|_| "Ed25519-Signatur: Länge")?),
            _ => SigMat::Other,
        };
        let mut s = Sig {
            version,
            typ,
            pub_algo,
            hash_algo,
            hashed,
            created: None,
            sig_expiry: 0,
            key_expiry: 0,
            issuer_fpr: None,
            issuer_id: None,
            key_flags: None,
            primary_uid: false,
            exportable: true,
            revocable: true,
            revocation: None,
            revokers: Vec::new(),
            embedded: None,
            critical_unknown: None,
            left16,
            salt,
            mat,
        };
        s.subpackets(hsub, true, depth)?;
        s.subpackets(usub, false, depth)?;
        Ok(s)
    }

    fn subpackets(&mut self, area: &[u8], hashed: bool, depth: u8) -> Result<(), String> {
        let mut r = Rd::new(area);
        while r.left() > 0 {
            let o = r.u8()? as usize;
            let len = match o {
                0..192 => o,
                192..255 => ((o - 192) << 8) + r.u8()? as usize + 192,
                _ => r.u32()? as usize,
            };
            let d = r.take(len)?;
            let (&t, d) = d.split_first().ok_or("Unterpaket ohne Typ")?;
            let crit = t & 0x80 != 0;
            let t = t & 0x7f;
            if crit && self.critical_unknown.is_none() && (!KNOWN.contains(&t)) {
                self.critical_unknown = Some(t);
            }
            if crit && (t == 20 || t == 6) && self.critical_unknown.is_none() {
                self.critical_unknown = Some(t);
            }
            let u32v = |d: &[u8]| -> Result<u32, String> {
                d.try_into().map(u32::from_be_bytes).map_err(|_| format!("Unterpaket {t}: Länge {}", d.len()))
            };
            // Issuer and the embedded back signature may sit in the unhashed area (gpg reads them there too);
            // everything that changes the meaning is taken from the hashed area only.
            match t {
                16 => self.issuer_id = Some(d.try_into().map_err(|_| "Aussteller-ID: Länge".to_string())?),
                33 => {
                    let ok = matches!(d, [4, r @ ..] if r.len() == 20) || matches!(d, [6, r @ ..] if r.len() == 32);
                    if !ok {
                        return Err("Aussteller-Fingerabdruck: Länge".into());
                    }
                    if hashed || self.issuer_fpr.is_none() {
                        self.issuer_fpr = Some(d[1..].to_vec());
                    }
                }
                32 if depth > 0 => return Err("eingebettete Signatur in einer eingebetteten Signatur".into()),
                32 if self.embedded.is_none() => self.embedded = Some(Box::new(Sig::parse_at(d, 1)?)),
                _ if !hashed => {}
                2 => self.created = Some(u32v(d)?),
                3 => self.sig_expiry = u32v(d)?,
                9 => self.key_expiry = u32v(d)?,
                4 => self.exportable = d.first() != Some(&0),
                7 => self.revocable = d.first() != Some(&0),
                25 => self.primary_uid = d.first().is_some_and(|&b| b != 0),
                27 => self.key_flags = Some(d.first().copied().unwrap_or(0)),
                // class, algorithm, fingerprint (20 octets v4, 32 v6); another length or a class without 0x80
                // names no revoker (ignored, not an error, so the self-signature keeps its meaning).
                12 => {
                    if let [class, _algo, f @ ..] = d
                        && class & 0x80 != 0
                        && (f.len() == 20 || f.len() == 32)
                    {
                        self.revokers.push(f.to_vec());
                    }
                }
                29 => {
                    let (&c, txt) = d.split_first().ok_or("Widerrufsgrund ohne Code")?;
                    self.revocation = Some((c, String::from_utf8_lossy(txt).chars().take(200).collect()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Starts the hash of this signature: the v6 salt comes first (RFC 9580 5.2.4).
    pub fn hasher(&self) -> Result<Hasher, String> {
        let mut h = Hasher::new(self.hash_algo)?;
        h.update(&self.salt);
        Ok(h)
    }

    /// Adds the trailer (5.2.4: the hashed part, then version, 0xFF and its four-octet length) and returns the digest.
    pub fn finish(&self, mut h: Hasher) -> Vec<u8> {
        h.update(&self.hashed);
        h.update(&[self.version, 0xff]);
        h.update(&(self.hashed.len() as u32).to_be_bytes());
        h.finish()
    }

    /// Everything that makes a signature unusable before any key is involved.
    pub fn sane(&self) -> Result<u32, String> {
        if let Some(t) = self.critical_unknown {
            return Err(format!("Signatur mit unbekanntem kritischem Unterpaket {t}"));
        }
        Hasher::new(self.hash_algo)?;
        self.created.ok_or_else(|| "Signatur ohne Erstellungszeit".to_string())
    }

    /// Whether `key` may have made this signature: issuer fingerprint (33) first, else key ID (16), else any key.
    pub fn issued_by(&self, key: &Key) -> bool {
        match (&self.issuer_fpr, &self.issuer_id) {
            (Some(f), _) => *f == key.fpr,
            (None, Some(id)) => *id == key.key_id(),
            (None, None) => true,
        }
    }

    /// Verifies this signature by `key` over `data` in memory (keys, user IDs, bindings).
    pub fn verify_mem(&self, key: &Key, data: &[u8]) -> Result<(), String> {
        self.sane()?;
        self.verify_with(key, data, self.hasher()?)
    }

    /// Verifies a revocation (0x20, 0x28, 0x30) by `key` over `data`. Unlike every other signature it may use
    /// SHA-1 and carry critical subpackets this verifier does not know: a revocation only takes validity away, so
    /// reading it more liberally fails closed (gpg 2.4 honours SHA-1 revocations too).
    pub fn verify_revocation(&self, key: &Key, data: &[u8]) -> Result<(), String> {
        if !matches!(self.typ, T_KEY_REV | T_SUBKEY_REV | T_CERT_REV) {
            return Err(format!("Signaturtyp {:#04x} ist kein Widerruf", self.typ));
        }
        let mut h = Hasher::new_revocation(self.hash_algo)?;
        h.update(&self.salt);
        self.verify_with(key, data, h)
    }

    /// `h` already holds the v6 salt; hashes `data` and the trailer and checks the values.
    fn verify_with(&self, key: &Key, data: &[u8], mut h: Hasher) -> Result<(), String> {
        let mut whole = self.salt.clone();
        whole.extend_from_slice(data);
        h.update(data);
        let d = self.finish(h);
        whole.extend_from_slice(&self.hashed);
        whole.extend_from_slice(&[self.version, 0xff]);
        whole.extend_from_slice(&(self.hashed.len() as u32).to_be_bytes());
        self.verify_digest(key, &d, Some(&whole))
    }

    /// Checks the digest's first two octets and the signature values; `whole` as in `Key::check`.
    pub fn verify_digest(&self, key: &Key, d: &[u8], whole: Option<&[u8]>) -> Result<(), String> {
        if key.version != self.version {
            return Err(format!("v{}-Signatur von einem v{}-Schlüssel (RFC 9580 5.2.3)", self.version, key.version));
        }
        if key.algo != self.pub_algo {
            return Err("Signaturverfahren passt nicht zum Schlüssel".into());
        }
        if d.get(..2) != Some(&self.left16[..]) {
            return Err("Signatur passt nicht (Prüfwert der ersten zwei Bytes)".into());
        }
        key.check(self.hash_algo, d, whole, &self.mat)
    }
}
