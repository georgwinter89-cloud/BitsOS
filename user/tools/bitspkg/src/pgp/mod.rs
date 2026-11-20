//! Our own minimal OpenPGP verifier (plan/spec/program-library.md section 2) for pacman package and database
//! signatures: RFC 9580 v4 and v6 keys and signatures, RSA through ring (and RSAVP1 in rsa.rs for streamed data),
//! Ed25519 through ring, SHA-2 through sha2, and the trust of a populated pacman keyring (trust.rs). No unsafe
//! code; every input is untrusted and refused with a German reason.

pub mod key;
pub mod keyring;
pub mod packet;
mod rsa;
pub mod sig;
pub mod trust;
mod verdict;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// How far the signer is trusted, as gpgme reports a key's validity to pacman (signing.c:890-935
/// _alpm_process_siglist). pacman's default SigLevel is TrustedOnly: only `Full` counts, and `good` is exactly
/// that. TrustAll (conf.c:460 process_siglevel) would also take `Marginal` and `Unknown`; Optional decides what
/// `Missing` means. Those choices belong to the caller (PK-6d), not to this verifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The signature verifies and its key is valid: a main key, or one certified by at least 3 main keys.
    Full,
    /// The signature verifies; the key is usable but certified by only 1 or 2 main keys (gpg: MARGINAL).
    Marginal,
    /// The signature verifies; the key is usable but no main key certifies it (gpg: UNKNOWN).
    Unknown,
    /// No usable signature: it does not verify, or its key is unknown, revoked, expired or may not sign.
    Invalid,
    /// The signature file is missing or empty.
    Missing,
}

impl Trust {
    /// Order among signatures that verify: the weakest decides a file with several.
    pub fn rank(self) -> u8 {
        match self {
            Trust::Full => 3,
            Trust::Marginal => 2,
            Trust::Unknown => 1,
            Trust::Invalid | Trust::Missing => 0,
        }
    }
}

/// The verdict on one detached signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The signature verifies and its key is fully valid (`trust == Full`): what TrustedOnly accepts.
    pub good: bool,
    /// The primary key's fingerprint (hex), or the issuer the signature names when the key is unknown.
    pub key_fpr: String,
    pub owner_uid: String,
    pub reason: String,
    pub trust: Trust,
}

/// A value for a one-line event (B6): quotes, backslashes and control characters cannot end the field or start
/// a forged second event.
pub fn ev(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            c if c.is_control() => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

pub struct Keyring {
    pub(crate) entries: Vec<trust::Entry>,
    /// Fingerprint of a primary key or subkey -> (entry, subkey index).
    pub(crate) by_fpr: HashMap<Vec<u8>, (usize, Option<usize>)>,
    pub(crate) by_id: HashMap<[u8; 8], Vec<(usize, Option<usize>)>>,
    /// Keys, subkeys and signatures that were skipped while reading, and why.
    pub notes: Vec<String>,
    /// The time validity was judged at (the load); bitspkg loads the keyring once per command.
    pub now: u64,
}

pub fn system_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Keyring {
    /// The keyrings of `dir` (`archlinux.gpg` + `-trusted` + `-revoked`, `cachyos.gpg` + ...), judged now.
    pub fn load(dir: &Path) -> Result<Keyring, String> {
        Self::load_at(dir, system_now())
    }

    pub fn load_at(dir: &Path, now: u64) -> Result<Keyring, String> {
        let kr = Self::from_sources(keyring::read_dir(dir)?, now)?;
        let (main, valid) = kr.counts();
        eprintln!(
            "bitspkg: event=pkg.pgp.keyring dir=\"{}\" keys={} main={main} valid={valid} skipped={}",
            ev(&dir.to_string_lossy()),
            kr.entries.len(),
            kr.notes.len()
        );
        Ok(kr)
    }

    /// Like `pacman-key --populate`: all keyrings into one, the trusted and revoked lists as their union.
    pub fn from_sources(sources: Vec<keyring::Source>, now: u64) -> Result<Keyring, String> {
        let mut notes = Vec::new();
        let mut certs = Vec::new();
        let mut trusted = HashSet::new();
        let mut revoked = HashSet::new();
        for s in sources {
            let c = keyring::parse_certs(&s.keys, &s.name, &mut notes).map_err(|e| format!("{}.gpg: {e}", s.name))?;
            certs.extend(c);
            trusted.extend(s.trusted);
            revoked.extend(s.revoked);
        }
        // The same key in two keyrings is one key, as in gpg's one pubring.
        let merged = keyring::merge(certs);
        let entries = trust::evaluate(merged, &trusted, &revoked, now);
        let mut by_fpr = HashMap::new();
        let mut by_id: HashMap<[u8; 8], Vec<(usize, Option<usize>)>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_fpr.insert(e.cert.primary.fpr.clone(), (i, None));
            by_id.entry(e.cert.primary.key_id()).or_default().push((i, None));
            for (j, s) in e.cert.subs.iter().enumerate() {
                by_fpr.entry(s.key.fpr.clone()).or_insert((i, Some(j)));
                by_id.entry(s.key.key_id()).or_default().push((i, Some(j)));
            }
        }
        Ok(Keyring { entries, by_fpr, by_id, notes, now })
    }

    /// (main keys, valid keys).
    pub fn counts(&self) -> (usize, usize) {
        let main = self.entries.iter().filter(|e| e.main).count();
        (main, self.entries.iter().filter(|e| e.valid.is_ok()).count())
    }

    /// Verifies the detached signature `sig` over `data`, read once in chunks (a package is never held in memory).
    pub fn verify_detached<R: Read>(&self, data: R, sig: &[u8]) -> Verdict {
        let v = verdict::verify(self, data, sig, self.now);
        if !v.good {
            eprintln!(
                "bitspkg: event=pkg.pgp.refused key={} trust={:?} reason=\"{}\"",
                ev(&v.key_fpr),
                v.trust,
                ev(&v.reason)
            );
        }
        v
    }

    /// The validity of every key, one line each (`bitspkg pgp keys`).
    pub fn describe(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| {
                let state = match &e.valid {
                    Ok(()) if e.main => "Hauptschlüssel".to_string(),
                    Ok(()) => "gültig".to_string(),
                    Err(x) => x.clone(),
                };
                format!("{} {} [{}] {}", e.cert.primary.fpr_hex(), e.cert.ring, state, e.owner)
            })
            .collect()
    }
}

/// Where bitspkg finds the active keyrings, below `--root`: the updated ones on /data first (PK-6e), else the
/// image's first keyring (CR to the desk: the image step puts it there).
pub const KEYRING_DIRS: &[&str] = &["data/bitspkg/keyrings", "etc/bitspkg/keyrings"];

pub fn default_dir(root: &Path) -> Result<std::path::PathBuf, String> {
    for d in KEYRING_DIRS.iter().map(|d| root.join(d)) {
        let has = std::fs::read_dir(&d)
            .map(|r| r.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with(".gpg")))
            .unwrap_or(false);
        if has {
            return Ok(d);
        }
    }
    Err(format!("kein Schlüsselbund gefunden ({} unter {})", KEYRING_DIRS.join(", "), root.display()))
}

fn verify_file(kr: &Keyring, file: &Path, sig: &Path) -> Verdict {
    let fail = |r: String| Verdict { good: false, key_fpr: String::new(), owner_uid: String::new(), reason: r, trust: Trust::Invalid };
    let sig_bytes = match std::fs::metadata(sig) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Verdict { trust: Trust::Missing, ..fail(format!("{}: Signaturdatei fehlt", sig.display())) };
        }
        Ok(m) if m.is_file() && m.len() <= trust::MAX_SIG_FILE as u64 => match std::fs::read(sig) {
            Ok(b) => b,
            Err(e) => return fail(format!("{}: {e}", sig.display())),
        },
        Ok(_) => return fail(format!("{}: keine Datei oder größer als 1 MiB", sig.display())),
        Err(e) => return fail(format!("{}: {e}", sig.display())),
    };
    match std::fs::File::open(file) {
        Ok(f) if f.metadata().is_ok_and(|m| m.is_file()) => kr.verify_detached(f, &sig_bytes),
        Ok(_) => fail(format!("{}: keine Datei", file.display())),
        Err(e) => fail(format!("{}: {e}", file.display())),
    }
}

/// `bitspkg pgp verify FILE SIG`, `pgp verify-list LIST` (lines `FILE<TAB>SIG`, one keyring load for all) and
/// `pgp keys`, each with `--keyring DIR` (default: the keyring folders below `root`, bitspkg's `--root`).
pub fn cli(mut a: Vec<String>, root: &Path) -> Result<i32, String> {
    let dir = match a.iter().position(|x| x == "--keyring") {
        Some(i) if i + 1 < a.len() => {
            a.remove(i);
            a.remove(i).into()
        }
        Some(_) => return Err("--keyring braucht einen Ordner".into()),
        None => default_dir(root)?,
    };
    if let Some(x) = a.iter().find(|x| x.starts_with("--")) {
        return Err(format!("unknown option {x}"));
    }
    let kr = Keyring::load(&dir)?;
    match (a.first().map(String::as_str), a.len()) {
        (Some("verify"), 3) => {
            let v = verify_file(&kr, Path::new(&a[1]), Path::new(&a[2]));
            if v.good {
                println!("gut {} {}", v.key_fpr, v.owner_uid);
                Ok(0)
            } else {
                println!("abgelehnt {}: {}", a[1], v.reason);
                Ok(1)
            }
        }
        (Some("verify-list"), 2) => {
            let list = std::fs::read_to_string(&a[1]).map_err(|e| format!("{}: {e}", a[1]))?;
            for l in list.lines().filter(|l| !l.is_empty()) {
                let (f, s) = l.split_once('\t').ok_or_else(|| format!("{}: Zeile ohne Tabulator: {l}", a[1]))?;
                let v = verify_file(&kr, Path::new(f), Path::new(s));
                println!("{}\t{f}\t{}\t{}", if v.good { "good" } else { "bad" }, v.key_fpr, v.reason);
            }
            Ok(0)
        }
        (Some("keys"), 1) => {
            kr.describe().iter().for_each(|l| println!("{l}"));
            kr.notes.iter().for_each(|n| println!("# {n}"));
            Ok(0)
        }
        _ => {
            eprintln!("usage: bitspkg pgp verify FILE SIG | verify-list LIST | keys  [--keyring DIR]");
            Ok(2)
        }
    }
}
