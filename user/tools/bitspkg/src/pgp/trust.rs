//! Key validity as a pacman keyring gives it after `pacman-key --populate` (pacman/scripts/pacman-key.sh.in:283):
//! the main keys of `<ring>-trusted` are lsigned by the local master key (ultimate trust), so they are valid by
//! fiat; their ownertrust is `4`, which in gpg's ownertrust file is TRUST_MARGINAL (gnupg trustdb.h: 2 undefined,
//! 3 never, 4 marginal, 5 full, 6 ultimate; the comment at pacman-key.sh.in:323-325 lists 5 and 6 the same way).
//! With gpg's default pgp model (marginals-needed 3, completes-needed 1; pacman's gpg.conf sets neither) a packager
//! key is fully valid with certifications from at least 3 main keys. Validity FULL is what pacman's default
//! SigLevel TrustedOnly accepts (pacman/lib/libalpm/signing.c:890 _alpm_process_siglist, the FULL case at :907).

use super::key::Key;
use super::keyring::{Cert, uid_hashed};
use super::sig::{self, Sig};
use std::collections::{HashMap, HashSet};

pub const MARGINALS_NEEDED: usize = 3;
/// A detached signature file: a few signature packets of at most 64 KiB each, armored or not.
pub const MAX_SIG_FILE: usize = 1 << 20;
/// How far a key or signature may lie in the future before it is refused: gpg's time conflict, with the five
/// minutes of clock drift between a packager's machine and this one.
pub const SKEW: u64 = 300;

/// 2026-11-11 00:00:00 UTC: the date of this verifier's generation. xtask's user build sets BITSOS_BUILD_EPOCH
/// to the commit time of the generation (image.rs `build_epoch`); the later of the two is the floor. A build
/// without it (host tests, plain cargo) keeps this one.
const HAND_FLOOR: u64 = 1_794_355_200;

pub fn clock_floor() -> u64 {
    option_env!("BITSOS_BUILD_EPOCH").and_then(|s| s.parse().ok()).map_or(HAND_FLOOR, |b: u64| b.max(HAND_FLOOR))
}

#[derive(Clone, Debug, Default)]
pub struct KeyState {
    pub created: u32,
    pub expires: Option<u64>,
    pub revoked: Option<String>,
    pub can_sign: bool,
    /// Why the key cannot be used at all (unknown algorithm, no valid binding).
    pub broken: Option<String>,
}

impl KeyState {
    pub(crate) fn usable(&self, now: u64) -> Result<(), String> {
        if self.created as u64 > now + SKEW {
            return Err(format!("mit Erstellungszeit in der Zukunft ({}; Uhr falsch?)", day(self.created as u64)));
        }
        if let Some(r) = &self.revoked {
            return Err(format!("widerrufen ({r})"));
        }
        if let Some(b) = &self.broken {
            return Err(b.clone());
        }
        if let Some(e) = self.expires.filter(|&e| e <= now) {
            return Err(format!("abgelaufen seit {}", day(e)));
        }
        Ok(())
    }
}

pub struct Entry {
    pub cert: Cert,
    pub primary: KeyState,
    pub subs: Vec<KeyState>,
    pub uid_ok: Vec<bool>,
    pub owner: String,
    pub main: bool,
    pub valid: Result<(), String>,
    /// Certifications by distinct main keys (the 3-main-key rule).
    pub certs: usize,
    /// The key is usable and only lacks main-key certifications: gpg's validity MARGINAL (1-2) or UNKNOWN (0).
    pub web_only: bool,
}

/// A calendar day (UTC) for reasons, without a date library (Howard Hinnant's civil_from_days).
pub fn day(t: u64) -> String {
    let z = (t / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The reason of a revocation (RFC 9580 5.2.3.31) in words.
fn why(s: &Sig) -> String {
    let (code, text) = s.revocation.clone().unwrap_or((0, String::new()));
    let c = match code {
        1 => "ersetzt",
        2 => "kompromittiert",
        3 => "nicht mehr benutzt",
        32 => "Benutzerkennung ungültig",
        _ => "ohne Angabe",
    };
    if text.is_empty() { c.to_string() } else { format!("{c}: {text}") }
}

pub(crate) fn expired(s: &Sig, now: u64) -> bool {
    s.sig_expiry != 0 && s.created.unwrap_or(0) as u64 + s.sig_expiry as u64 <= now
}

/// A revocation by `by` over `msg`; SHA-1 and unknown critical subpackets do not stop it (Sig::verify_revocation).
fn revokes(s: &Sig, by: &Key, msg: &[u8]) -> bool {
    s.issued_by(by) && s.verify_revocation(by, msg).is_ok()
}

/// A signature made by `by` over `msg`, not before `by` existed and not expired at `now`.
fn good(s: &Sig, by: &Key, msg: &[u8], now: u64) -> bool {
    s.issued_by(by) && s.created.is_some_and(|c| c >= by.created) && !expired(s, now) && s.verify_mem(by, msg).is_ok()
}

fn latest<'a>(it: impl Iterator<Item = &'a Sig>) -> Option<&'a Sig> {
    it.max_by_key(|s| s.created)
}

/// The self-signatures of one key: revocation, user IDs, expiry, usage, subkey bindings with back signatures.
fn self_state(c: &Cert, now: u64) -> (KeyState, Vec<KeyState>, Vec<bool>, String) {
    let pk = &c.primary;
    let mut st = KeyState { created: pk.created, ..Default::default() };
    if let Err(e) = pk.supported() {
        st.broken = Some(format!("mit nicht angenommenem Verfahren ({e})"));
        return (st, vec![KeyState { broken: Some("am unbrauchbaren Hauptschlüssel".into()), ..Default::default() }; c.subs.len()], vec![false; c.uids.len()], String::new());
    }
    let ph = pk.hashed();
    if let Some(r) = c.direct.iter().find(|s| s.typ == sig::T_KEY_REV && revokes(s, pk, &ph)) {
        st.revoked = Some(why(r));
    }
    let direct = latest(c.direct.iter().filter(|s| s.typ == sig::T_DIRECT && good(s, pk, &ph, now)));
    let mut best: Vec<Option<&Sig>> = Vec::new();
    for u in &c.uids {
        let mut msg = ph.clone();
        msg.extend_from_slice(&uid_hashed(&u.raw));
        let b = latest(u.sigs.iter().filter(|s| sig::is_cert(s.typ) && good(s, pk, &msg, now)));
        let rev = b.is_some_and(|b| {
            b.revocable && u.sigs.iter().any(|s| s.typ == sig::T_CERT_REV && s.created >= b.created && revokes(s, pk, &msg))
        });
        best.push(if rev { None } else { b });
    }
    let uid_ok: Vec<bool> = best.iter().map(Option::is_some).collect();
    let pick = (0..best.len())
        .filter(|&i| best[i].is_some_and(|s| s.primary_uid))
        .max_by_key(|&i| best[i].map(|s| s.created))
        .or_else(|| (0..best.len()).filter(|&i| best[i].is_some()).max_by_key(|&i| best[i].map(|s| s.created)));
    let owner = pick.map(|i| c.uids[i].text.clone()).unwrap_or_default();
    let keysig = match (pick.and_then(|i| best[i]), direct) {
        (Some(u), Some(d)) => Some(if d.created > u.created { d } else { u }),
        (u, d) => u.or(d),
    };
    match keysig {
        Some(s) => {
            st.expires = (s.key_expiry != 0).then(|| pk.created as u64 + s.key_expiry as u64);
            st.can_sign = s.key_flags.is_none_or(|f| f & 0x02 != 0);
        }
        None => st.broken = Some("ohne gültige Selbstsignatur".into()),
    }
    let subs = c.subs.iter().map(|sub| sub_state(pk, &ph, sub, now)).collect();
    (st, subs, uid_ok, owner)
}

fn sub_state(pk: &Key, ph: &[u8], sub: &super::keyring::Sub, now: u64) -> KeyState {
    let sk = &sub.key;
    let mut st = KeyState { created: sk.created, ..Default::default() };
    if let Err(e) = sk.supported() {
        st.broken = Some(format!("mit nicht angenommenem Verfahren ({e})"));
        return st;
    }
    let mut msg = ph.to_vec();
    msg.extend_from_slice(&sk.hashed());
    if let Some(r) = sub.sigs.iter().find(|s| s.typ == sig::T_SUBKEY_REV && revokes(s, pk, &msg)) {
        st.revoked = Some(why(r));
    }
    let Some(b) = latest(sub.sigs.iter().filter(|s| s.typ == sig::T_SUBKEY_BIND && good(s, pk, &msg, now))) else {
        st.broken = Some("ohne gültige Bindung".into());
        return st;
    };
    st.expires = (b.key_expiry != 0).then(|| sk.created as u64 + b.key_expiry as u64);
    // A signing subkey needs its back signature (0x19), as gpg's require-cross-certification does.
    let flags = b.key_flags.unwrap_or(0);
    st.can_sign = flags & 0x02 != 0
        && b.embedded.as_deref().is_some_and(|e| e.typ == sig::T_PRIMARY_BIND && good(e, sk, &msg, now));
    st
}

/// A key revocation (0x20) of entry `i` by a designated revoker (RFC 9580 5.2.3.23), as gpg judges it
/// (g10/getkey.c merge_selfsigs_main -> check_revocation_keys): the revokers are named in the hashed area of
/// any verifying direct-key self-signature (0x1F) of the key, not only the latest; the revocation must verify
/// over the key with the primary key of a keyring entry whose fingerprint is named. That revoking key need not
/// be valid itself.
fn designated_revocation(es: &[Entry], i: usize) -> Option<String> {
    let c = &es[i].cert;
    let pk = &c.primary;
    if pk.supported().is_err() {
        return None;
    }
    let ph = pk.hashed();
    let named: Vec<&[u8]> = c
        .direct
        .iter()
        .filter(|s| s.typ == sig::T_DIRECT && !s.revokers.is_empty())
        .filter(|s| s.issued_by(pk) && s.created.is_some_and(|t| t >= pk.created) && s.verify_mem(pk, &ph).is_ok())
        .flat_map(|s| s.revokers.iter().map(Vec::as_slice))
        .collect();
    if named.is_empty() {
        return None;
    }
    for s in c.direct.iter().filter(|s| s.typ == sig::T_KEY_REV) {
        for (j, e) in es.iter().enumerate() {
            let rk = &e.cert.primary;
            if j != i && named.contains(&rk.fpr.as_slice()) && revokes(s, rk, &ph) {
                return Some(format!("durch den benannten Widerrufer {}: {}", rk.fpr_hex(), why(s)));
            }
        }
    }
    None
}

/// Evaluates every key at `now`: self-signatures, main keys, then the 3-main-key rule for all others.
pub fn evaluate(certs: Vec<Cert>, trusted: &HashSet<Vec<u8>>, revoked: &HashSet<Vec<u8>>, now: u64) -> Vec<Entry> {
    let mut es: Vec<Entry> = certs
        .into_iter()
        .map(|cert| {
            let (primary, subs, uid_ok, owner) = self_state(&cert, now);
            Entry { cert, primary, subs, uid_ok, owner, main: false, valid: Ok(()), certs: 0, web_only: false }
        })
        .collect();
    let designated: Vec<Option<String>> = (0..es.len()).map(|i| designated_revocation(&es, i)).collect();
    for (e, d) in es.iter_mut().zip(designated) {
        if e.primary.revoked.is_none() {
            e.primary.revoked = d;
        }
    }
    for e in es.iter_mut() {
        let f = &e.cert.primary.fpr;
        e.valid = if revoked.contains(f) {
            Err(format!("steht in {}-revoked (gesperrt)", e.cert.ring))
        } else if let Err(x) = e.primary.usable(now) {
            Err(x)
        } else if !e.uid_ok.contains(&true) {
            Err("ohne gültige Benutzerkennung".into())
        } else {
            Ok(())
        };
        e.main = trusted.contains(f) && e.valid.is_ok();
    }
    let mains: Vec<usize> = (0..es.len()).filter(|&i| es[i].main).collect();
    let mut by_id: HashMap<[u8; 8], Vec<usize>> = HashMap::new();
    for &i in &mains {
        by_id.entry(es[i].cert.primary.key_id()).or_default().push(i);
    }
    let counts: Vec<usize> = (0..es.len())
        .map(|i| if es[i].main || es[i].valid.is_err() { 0 } else { main_certs(&es, i, &mains, &by_id, now) })
        .collect();
    for (e, n) in es.iter_mut().zip(counts) {
        e.certs = n;
        if e.valid.is_ok() && !e.main && n < MARGINALS_NEEDED {
            e.valid = Err(format!("nicht beglaubigt: {n} von {MARGINALS_NEEDED} nötigen Beglaubigungen durch Hauptschlüssel"));
            e.web_only = true;
        }
    }
    es
}

/// The most main keys that certify one valid user ID of entry `i` (unexpired, exportable, not revoked by the
/// same main key later).
fn main_certs(es: &[Entry], i: usize, mains: &[usize], by_id: &HashMap<[u8; 8], Vec<usize>>, now: u64) -> usize {
    let c = &es[i].cert;
    let ph = c.primary.hashed();
    let mut most = 0;
    for (u, ok) in c.uids.iter().zip(&es[i].uid_ok) {
        if !ok {
            continue;
        }
        let mut msg = ph.clone();
        msg.extend_from_slice(&uid_hashed(&u.raw));
        let mut who: HashSet<usize> = HashSet::new();
        for s in u.sigs.iter().filter(|s| sig::is_cert(s.typ) && s.exportable) {
            let cand: Vec<usize> = match (&s.issuer_fpr, &s.issuer_id) {
                (Some(f), _) => mains.iter().copied().filter(|&m| es[m].cert.primary.fpr == *f).collect(),
                (None, Some(id)) => by_id.get(id).cloned().unwrap_or_default(),
                (None, None) => Vec::new(),
            };
            for m in cand {
                if m == i || who.contains(&m) {
                    continue;
                }
                let mk = &es[m].cert.primary;
                if !good(s, mk, &msg, now) {
                    continue;
                }
                let revoked = s.revocable
                    && u.sigs.iter().any(|r| r.typ == sig::T_CERT_REV && r.created >= s.created && revokes(r, mk, &msg));
                if !revoked {
                    who.insert(m);
                }
            }
        }
        most = most.max(who.len());
    }
    most
}
