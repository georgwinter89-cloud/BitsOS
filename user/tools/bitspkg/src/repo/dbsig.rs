//! The signature check of a database (plan 1.4, item 3): a trait, so refresh does not depend on the verifier.
//! PK-6b's OpenPGP verifier (`pgp::Keyring::verify_detached`) implements it as `Pgp`; without a keyring
//! `NoKeyring` refuses every signature with the reason, so a database whose source requires a signature is never
//! taken unchecked. Tests use their own double.

use std::path::Path;

pub trait DbVerifier {
    /// `data` is the downloaded database, `sig` its detached `.sig`. Ok: who signed it (for the report); Err: the
    /// German reason it is refused. Ok only for a key that is fully valid by plan 1.4's trust model (a main key of
    /// the keyring, or a key certified by 3 of them), unexpired and unrevoked, with the signature made inside the
    /// key's validity: pacman's `TrustedOnly` (signing.c:890 _alpm_process_siglist). Marginal or unknown
    /// validity, an expired key or signature, a revoked key: Err. So there is no trust grade to pass on here.
    fn verify(&self, keyring: &str, data: &Path, sig: &[u8]) -> Result<String, String>;
}

/// PK-6b's verifier over the active keyrings (pgp::default_dir below the root): what refresh uses since PK-6d.
/// One keyring for all sources, as `pacman-key --populate` makes one (the source's `keyring` names where its
/// keys come from; pacman too checks against the whole populated keyring).
pub struct Pgp {
    pub kr: crate::pgp::Keyring,
}

impl Pgp {
    /// The keyring below `root`; without one the reason (a signed database is then refused, never taken).
    pub fn load(root: &Path) -> Result<Pgp, String> {
        let dir = crate::pgp::default_dir(root)?;
        Ok(Pgp { kr: crate::pgp::Keyring::load(&dir)? })
    }
}

impl DbVerifier for Pgp {
    fn verify(&self, _keyring: &str, data: &Path, sig: &[u8]) -> Result<String, String> {
        let f = std::fs::File::open(data).map_err(|e| format!("{}: {e}", data.display()))?;
        let v = self.kr.verify_detached(f, sig);
        if v.good { Ok(format!("{} {}", v.key_fpr, v.owner_uid)) } else { Err(v.reason) }
    }
}

/// No keyring could be loaded: every signature is refused with that reason.
pub struct NoKeyring(pub String);

impl DbVerifier for NoKeyring {
    fn verify(&self, keyring: &str, _data: &Path, _sig: &[u8]) -> Result<String, String> {
        Err(format!("kein Schlüsselbund geladen ({keyring}): {}", self.0))
    }
}
