//! Transferable public keys (RFC 9580 10.1) and the pacman keyring folder as `pacman-key --populate` reads it
//! (pacman/scripts/pacman-key.sh.in:283 populate_keyring): `<name>.gpg`, `<name>-trusted` (the main keys,
//! ownertrust lines `FPR:4:`), `<name>-revoked` (one fingerprint per line).

use super::key::Key;
use super::packet::{self, MAX_PACKET, TAG_KEY, TAG_MARKER, TAG_SIG, TAG_SUBKEY, TAG_TRUST, TAG_UATTR, TAG_UID};
use super::sig::Sig;
use std::collections::HashMap;
use std::path::Path;

/// The bound for one keyring file (brief: like a database, at most 256 MiB).
pub const MAX_KEYRING: u64 = 256 << 20;

#[derive(Clone, Debug)]
pub struct Uid {
    pub raw: Vec<u8>,
    pub text: String,
    pub sigs: Vec<Sig>,
}

#[derive(Clone, Debug)]
pub struct Sub {
    pub key: Key,
    pub sigs: Vec<Sig>,
}

#[derive(Clone, Debug)]
pub struct Cert {
    pub primary: Key,
    /// Signatures directly on the primary key: direct-key (0x1F) and key revocations (0x20).
    pub direct: Vec<Sig>,
    pub uids: Vec<Uid>,
    pub subs: Vec<Sub>,
    /// The keyring the key came from (`archlinux`, `cachyos`, ...).
    pub ring: String,
}

/// One keyring as `pacman-key --populate NAME` takes it.
pub struct Source {
    pub name: String,
    pub keys: Vec<u8>,
    pub trusted: Vec<Vec<u8>>,
    pub revoked: Vec<Vec<u8>>,
}

/// Reads the certificates of a (binary or armored) keyring. A packet that cannot be read is a refusal of the
/// whole keyring; a key with an unknown version or a signature this verifier cannot read is skipped and named
/// in `notes`, as gpg skips them on import.
pub fn parse_certs(input: &[u8], ring: &str, notes: &mut Vec<String>) -> Result<Vec<Cert>, String> {
    let bin = packet::dearmor(input)?;
    let mut certs: Vec<Cert> = Vec::new();
    // What the next signature belongs to: 0 primary, 1 the last user ID, 2 the last subkey, 3 skipped.
    let mut at = 3u8;
    let mut skip_cert = true;
    for p in packet::packets(&bin)? {
        match p.tag {
            TAG_KEY => {
                skip_cert = false;
                at = 0;
                match key_of(p.body) {
                    Ok(k) => certs.push(Cert { primary: k, direct: vec![], uids: vec![], subs: vec![], ring: ring.into() }),
                    Err(e) => {
                        notes.push(format!("{ring}: Schlüssel übersprungen: {e}"));
                        skip_cert = true;
                    }
                }
            }
            _ if skip_cert => {}
            TAG_UID => {
                if p.body.len() > MAX_PACKET {
                    return Err("Benutzerkennung länger als 64 KiB".into());
                }
                let c = certs.last_mut().ok_or("Benutzerkennung ohne Schlüssel")?;
                c.uids.push(Uid { raw: p.body.to_vec(), text: String::from_utf8_lossy(p.body).into_owned(), sigs: vec![] });
                at = 1;
            }
            TAG_UATTR => at = 3,
            TAG_SUBKEY => {
                let c = certs.last_mut().ok_or("Unterschlüssel ohne Schlüssel")?;
                match key_of(p.body) {
                    Ok(k) => {
                        c.subs.push(Sub { key: k, sigs: vec![] });
                        at = 2;
                    }
                    Err(e) => {
                        notes.push(format!("{ring}: Unterschlüssel von {} übersprungen: {e}", c.primary.fpr_hex()));
                        at = 3;
                    }
                }
            }
            TAG_SIG => {
                let c = certs.last_mut().ok_or("Signatur ohne Schlüssel")?;
                let s = match Sig::parse(p.body) {
                    Ok(s) => s,
                    Err(_) if p.body.first().is_some_and(|&v| v != 4 && v != 6) => continue,
                    Err(e) => {
                        notes.push(format!("{ring}: Signatur an {} übersprungen: {e}", c.primary.fpr_hex()));
                        continue;
                    }
                };
                // Key revocations and direct-key signatures belong to the primary key wherever they stand.
                let at = if s.typ == super::sig::T_KEY_REV || s.typ == super::sig::T_DIRECT { 0 } else { at };
                match at {
                    0 => c.direct.push(s),
                    1 => c.uids.last_mut().ok_or("Signatur ohne Benutzerkennung")?.sigs.push(s),
                    2 => c.subs.last_mut().ok_or("Signatur ohne Unterschlüssel")?.sigs.push(s),
                    _ => {}
                }
            }
            TAG_TRUST | TAG_MARKER => {}
            t if t < 40 => return Err(format!("Pakettyp {t} gehört nicht in einen Schlüsselbund")),
            _ => {}
        }
    }
    Ok(merge(certs))
}

/// The octets a signature over a user ID hashes (RFC 9580 5.2.4): 0xB4, a four-octet length, the ID.
pub fn uid_hashed(raw: &[u8]) -> Vec<u8> {
    let mut v = vec![0xb4];
    v.extend_from_slice(&(raw.len() as u32).to_be_bytes());
    v.extend_from_slice(raw);
    v
}

fn key_of(body: &[u8]) -> Result<Key, String> {
    if body.len() > MAX_PACKET {
        return Err("Schlüsselpaket länger als 64 KiB".into());
    }
    Key::parse(body)
}

/// The same key twice (a keyring may carry it in several blocks) becomes one, as gpg merges on import.
pub(crate) fn merge(certs: Vec<Cert>) -> Vec<Cert> {
    let mut out: Vec<Cert> = Vec::new();
    let mut at: HashMap<Vec<u8>, usize> = HashMap::new();
    for c in certs {
        let Some(&i) = at.get(&c.primary.fpr) else {
            at.insert(c.primary.fpr.clone(), out.len());
            out.push(c);
            continue;
        };
        let o = &mut out[i];
        o.direct.extend(c.direct);
        for u in c.uids {
            match o.uids.iter_mut().find(|x| x.raw == u.raw) {
                Some(x) => x.sigs.extend(u.sigs),
                None => o.uids.push(u),
            }
        }
        for s in c.subs {
            match o.subs.iter_mut().find(|x| x.key.fpr == s.key.fpr) {
                Some(x) => x.sigs.extend(s.sigs),
                None => o.subs.push(s),
            }
        }
    }
    out
}

fn read_bounded(p: &Path) -> Result<Option<Vec<u8>>, String> {
    let m = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    if !m.is_file() || m.len() > MAX_KEYRING {
        return Err(format!("{}: keine Datei oder größer als 256 MiB", p.display()));
    }
    std::fs::read(p).map(Some).map_err(|e| format!("{}: {e}", p.display()))
}

/// Fingerprint lines of `-trusted` (`FPR:4:`) or `-revoked` (`FPR`); empty lines and `#` comments are allowed, as
/// populate_keyring (pacman-key.sh.in:330) allows them.
fn fpr_lines(p: &Path, text: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let text = std::str::from_utf8(text).map_err(|_| format!("{}: kein Text", p.display()))?;
    let mut v = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f = l.split(':').next().unwrap_or("");
        v.push(packet::unhex(f).ok_or_else(|| format!("{}:{}: kein Fingerabdruck: {l:.80}", p.display(), i + 1))?);
    }
    Ok(v)
}

/// The keyrings in `dir`: every `<name>.gpg` with its `-trusted` and `-revoked` files, in name order.
pub fn read_dir(dir: &Path) -> Result<Vec<Source>, String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".gpg").map(str::to_string))
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(format!("{}: kein Schlüsselbund (*.gpg)", dir.display()));
    }
    let mut out = Vec::new();
    for n in names {
        let kp = dir.join(format!("{n}.gpg"));
        let keys = read_bounded(&kp)?.ok_or_else(|| format!("{}: fehlt", kp.display()))?;
        let mut lists = [Vec::new(), Vec::new()];
        for (i, suf) in ["trusted", "revoked"].iter().enumerate() {
            let p = dir.join(format!("{n}-{suf}"));
            if let Some(t) = read_bounded(&p)? {
                lists[i] = fpr_lines(&p, &t)?;
            }
        }
        let [trusted, revoked] = lists;
        out.push(Source { name: n, keys, trusted, revoked });
    }
    Ok(out)
}
