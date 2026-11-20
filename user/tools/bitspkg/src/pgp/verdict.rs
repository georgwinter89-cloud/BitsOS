//! The verdict on a detached data signature (RFC 9580 5.2.4, type 0x00) against a judged keyring (trust.rs):
//! which key made it, whether that key may sign at that time, and the values over the streamed data. pacman's
//! counterpart is signing.c:537 _alpm_gpgme_checksig with _alpm_process_siglist (:890).

use super::key::Key;
use super::packet::{self, TAG_MARKER, TAG_SIG};
use super::sig::{self, Hasher, Sig};
use super::trust::{Entry, MAX_SIG_FILE, SKEW, clock_floor, day, expired};
use super::{Keyring, Trust, Verdict};
use std::collections::HashSet;
use std::io::Read;

fn bad(reason: impl Into<String>) -> Verdict {
    Verdict { good: false, key_fpr: String::new(), owner_uid: String::new(), reason: reason.into(), trust: Trust::Invalid }
}

/// A candidate signer: (keyring entry, subkey index).
type Signer = (usize, Option<usize>);

/// The detached signature check (RFC 9580 5.2.4, type 0x00) over `data`, streamed once. All signatures in the file
/// must verify, as pacman requires (signing.c:890 _alpm_process_siglist); the verdict is the weakest of them.
///
/// Everything that needs no digest is checked for every signature before one byte of data is read, and one key
/// may sign only once per file: the number of hashes the data goes through is bounded by the valid keys of the
/// keyring (v4 signatures of one hash share one), not by the size of a hostile .sig file.
pub fn verify<R: Read>(kr: &Keyring, mut data: R, sig_bytes: &[u8], now: u64) -> Verdict {
    let floor = clock_floor();
    if now < floor {
        return bad(format!("Uhr falsch: die Systemzeit liegt vor {} (dem Bau dieser BitsOS-Version)", day(floor)));
    }
    if sig_bytes.is_empty() {
        return Verdict { trust: Trust::Missing, ..bad("Signaturdatei ist leer") };
    }
    if sig_bytes.len() > MAX_SIG_FILE {
        return bad("Signaturdatei größer als 1 MiB");
    }
    let sigs = match parse_sigs(sig_bytes) {
        Ok(s) => s,
        Err(e) => return bad(format!("Signatur unlesbar: {e}")),
    };
    let mut used: HashSet<Signer> = HashSet::new();
    let mut hs: Vec<((u8, Vec<u8>), Hasher)> = Vec::new();
    let mut plan: Vec<(&Sig, Vec<Signer>, usize)> = Vec::new();
    for s in &sigs {
        if s.typ != sig::T_BINARY {
            return bad(format!("Signaturtyp {:#04x} ist keine Signatur über Binärdaten", s.typ));
        }
        if let Err(e) = s.sane() {
            return bad(e);
        }
        let cand = match precheck(kr, s, now) {
            Ok(c) => c,
            Err(v) => return v,
        };
        if cand.iter().any(|c| used.contains(c)) {
            return bad("zwei Signaturen desselben Schlüssels in einer Signaturdatei");
        }
        used.extend(cand.iter().copied());
        let k = (s.hash_algo, s.salt.clone());
        let hi = match hs.iter().position(|(x, _)| *x == k) {
            Some(i) => i,
            None => match s.hasher() {
                Ok(h) => {
                    hs.push((k, h));
                    hs.len() - 1
                }
                Err(e) => return bad(e),
            },
        };
        plan.push((s, cand, hi));
    }
    let mut buf = vec![0u8; 256 << 10];
    loop {
        match data.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hs.iter_mut().for_each(|(_, h)| h.update(&buf[..n])),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return bad(format!("Daten nicht lesbar: {e}")),
        }
    }
    let mut weakest: Option<Verdict> = None;
    for (s, cand, hi) in plan {
        let v = judge(kr, s, &cand, &s.finish(hs[hi].1.clone()));
        if v.trust == Trust::Invalid {
            return v;
        }
        if weakest.as_ref().is_none_or(|w| v.trust.rank() < w.trust.rank()) {
            weakest = Some(v);
        }
    }
    weakest.unwrap_or_else(|| bad("keine Signatur"))
}

fn parse_sigs(b: &[u8]) -> Result<Vec<Sig>, String> {
    let bin = packet::dearmor(b)?;
    let mut v = Vec::new();
    for p in packet::packets(&bin)? {
        match p.tag {
            TAG_SIG => v.push(Sig::parse(p.body)?),
            TAG_MARKER => {}
            t => return Err(format!("Pakettyp {t} in einer Signaturdatei")),
        }
    }
    if v.is_empty() {
        return Err("keine Signatur in der Datei".into());
    }
    Ok(v)
}

fn signer_key(e: &Entry, sub: Option<usize>) -> &Key {
    sub.map_or(&e.cert.primary, |j| &e.cert.subs[j].key)
}

/// The keys that may have made `s` at its time, judged without the data: issuer in the keyring, same version
/// and algorithm, key usable (or only lacking main-key certifications), allowed to sign, signature not expired.
fn precheck(kr: &Keyring, s: &Sig, now: u64) -> Result<Vec<Signer>, Verdict> {
    let cand: Vec<Signer> = match (&s.issuer_fpr, &s.issuer_id) {
        (Some(f), _) => kr.by_fpr.get(f).into_iter().copied().collect(),
        (None, Some(id)) => kr.by_id.get(id).cloned().unwrap_or_default(),
        (None, None) => Vec::new(),
    };
    let issuer = s.issuer_fpr.as_deref().map(packet::hex).or(s.issuer_id.map(|i| packet::hex(&i))).unwrap_or_default();
    let mut last = bad(format!("Schlüssel {issuer} ist nicht im Schlüsselbund"));
    last.key_fpr = issuer;
    let mut ok = Vec::new();
    for (ei, sub) in cand {
        let e = &kr.entries[ei];
        let key = signer_key(e, sub);
        let why = if key.version != s.version {
            format!("v{}-Signatur von einem v{}-Schlüssel (RFC 9580 5.2.3)", s.version, key.version)
        } else if key.algo != s.pub_algo {
            "Signaturverfahren passt nicht zum Schlüssel".into()
        } else if let Some(x) = e.valid.as_ref().err().filter(|_| !e.web_only) {
            format!("Schlüssel {x}")
        } else if let Err(x) = check_signer(e, sub, s.created.unwrap_or(0), now) {
            x
        } else if expired(s, now) {
            "Signatur abgelaufen".into()
        } else {
            ok.push((ei, sub));
            continue;
        };
        last = Verdict { key_fpr: e.cert.primary.fpr_hex(), owner_uid: e.owner.clone(), ..bad(why) };
    }
    if ok.is_empty() { Err(last) } else { Ok(ok) }
}

/// One signature with its digest against the candidates `precheck` left; the trust is the signer's validity.
fn judge(kr: &Keyring, s: &Sig, cand: &[Signer], d: &[u8]) -> Verdict {
    let mut last = bad("kein Schlüssel");
    for &(ei, sub) in cand {
        let e = &kr.entries[ei];
        let base = Verdict { key_fpr: e.cert.primary.fpr_hex(), owner_uid: e.owner.clone(), ..bad("") };
        if let Err(x) = s.verify_digest(signer_key(e, sub), d, None) {
            last = Verdict { reason: x, ..base };
            continue;
        }
        return match &e.valid {
            Ok(()) => Verdict { good: true, reason: "gut".into(), trust: Trust::Full, ..base },
            Err(x) => {
                let trust = if e.certs > 0 { Trust::Marginal } else { Trust::Unknown };
                Verdict { reason: format!("Schlüssel {x}"), trust, ..base }
            }
        };
    }
    last
}

/// The key that made the signature: usable now, allowed to sign, and the signature made inside its lifetime.
fn check_signer(e: &Entry, sub: Option<usize>, created: u32, now: u64) -> Result<(), String> {
    if created as u64 > now + SKEW {
        return Err(format!("Signatur liegt in der Zukunft ({}; Uhr falsch?)", day(created as u64)));
    }
    let mut chain = vec![(&e.primary, "Schlüssel")];
    if let Some(j) = sub {
        chain.push((&e.subs[j], "Unterschlüssel"));
    }
    for (k, what) in &chain {
        k.usable(now).map_err(|x| format!("{what} {x}"))?;
        if created < k.created || k.expires.is_some_and(|x| created as u64 >= x) {
            return Err(format!("Signatur außerhalb der Gültigkeit des {what}s gemacht"));
        }
    }
    let signer = chain.last().map(|c| c.0).ok_or("kein Schlüssel")?;
    if !signer.can_sign {
        return Err(if sub.is_some() {
            "Unterschlüssel darf nicht signieren (Schlüsselflag 0x02 oder Rücksignatur fehlt)".into()
        } else {
            "Schlüssel darf nicht signieren (Schlüsselflag 0x02 fehlt)".into()
        });
    }
    Ok(())
}
