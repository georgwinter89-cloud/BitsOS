//! Whose secrets are whose (CL-S). A secret key belongs to the program that registered it: the uid and the
//! executable the kernel named for the registering peer (`peer.rs`), kept in `owners.list` next to the values,
//! so the next start of settingsd still knows it before the owner comes back. An owner name in a request is
//! only a claim; it counts when the peer is that program.
//!
//! - reading a secret: the key's owner, from that program;
//! - registering or removing keys under an owner name that holds a secret: that program;
//! - registering a key name on record as someone's secret, or a key in a namespace of its own that holds
//!   one: that owner (so nobody adopts the stored value or squats the namespace before the owner is back);
//! - writing a secret: that program, or a change Georg confirmed himself: `"confirmed": true` from the
//!   session's compositor or from root, nobody else's (`vouch.rs`);
//! - a namespace several owners share holds secrets only where settingsd names one system program for the
//!   keys (`builtin::HOLDERS`: `ai.provider.*` and `ai.tools.*` are `assist`'s, from settingsd's own
//!   directory); nobody else registers a key there, secret or not;
//! - the provider keys settingsd makes itself from the list `ai.providers` (CL-5s, `providers.rs`) are that
//!   program's without a registration: settingsd makes the record with the key.
//!
//! Every refusal is an event naming the peer, never a value: `owner.refused`, or `peer.unknown` (error
//! `peer_unknown`) when the refusal turns on a program settingsd could not tell.

use crate::builtin;
use crate::events::{Level, q};
use crate::json::{self, Json};
use crate::key::{self, KeyDef, Safety};
use crate::peer::Peer;
use crate::registry::{Error, Registry, err, under};
use crate::store::{Job, Place};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const FILE: &str = "owners.list";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub owner: String,
    /// 0: registered by root, which stands for no user in particular (see [`Bound::admits`]).
    pub uid: u32,
    pub exe: String,
}

impl Bound {
    /// The same program, run by the same user. Root passes for any user (it reads the files anyway), and a
    /// record root made admits every user of the program until one of them registers (the console is root's,
    /// the desktop uid 1000's; the first registration from the desktop makes the record that user's).
    pub fn admits(&self, p: &Peer) -> bool {
        p.exe.as_deref() == Some(self.exe.as_str()) && p.uid.is_some_and(|u| u == 0 || self.uid == 0 || u == self.uid)
    }
}

pub type Records = BTreeMap<String, Bound>;

/// One record per line; lines that are not a record are dropped (the count is reported by the caller).
pub fn parse(text: &str) -> (Records, usize) {
    let mut out = Records::new();
    let mut bad = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let rec = json::parse(line).ok().and_then(|j| {
            let uid = u32::try_from(j.get("uid")?.as_i64()?).ok()?;
            let b = Bound { owner: j.str("owner")?.to_string(), uid, exe: j.str("exe")?.to_string() };
            (key::valid_name(j.str("key")?) && b.exe.starts_with('/')).then(|| (j.str("key").unwrap_or("").to_string(), b))
        });
        match rec {
            Some((k, b)) => {
                out.insert(k, b);
            }
            None => bad += 1,
        }
    }
    (out, bad)
}

pub fn text(r: &Records) -> String {
    r.iter()
        .map(|(k, b)| {
            let j = Json::obj()
                .with("key", k.as_str())
                .with("owner", b.owner.as_str())
                .with("uid", u64::from(b.uid))
                .with("exe", b.exe.as_str());
            format!("{}\n", j.to_text())
        })
        .collect()
}

impl Registry {
    /// The program an owner name is bound to: every secret of one owner carries the same one.
    pub(crate) fn bound(&self, owner: &str) -> Option<&Bound> {
        self.secrets.values().find(|b| b.owner == owner)
    }

    fn refuse(&self, what: &str, key: &str, owner: &str, peer: &Peer, msg: String) -> Error {
        self.ev.emit(
            Level::Warn,
            "owner.refused",
            &format!("what={what} key={} owner={} {}", q(key), q(owner), peer.show()),
        );
        err("not_owner", msg)
    }

    /// A refusal that turns on who the peer is. A program on the socket that settingsd could not tell (gone
    /// or replaced before it was looked at) is told so, with a code and an event of its own: its next try on
    /// a connection it keeps open may pass, and the log does not show an owner's hasty exit as an intruder.
    fn deny(&self, what: &str, key: &str, owner: &str, peer: &Peer, msg: String) -> Error {
        if !peer.untold() {
            return self.refuse(what, key, owner, peer, msg);
        }
        self.ev.emit(
            Level::Warn,
            "peer.unknown",
            &format!("what={what} key={} owner={} {}", q(key), q(owner), peer.show()),
        );
        let why = "settingsd could not tell which program asks (it must stay connected until the answer)";
        err("peer_unknown", format!("{msg}; {why}"))
    }

    /// Whether `peer` may act under the owner name `owner` (register, unregister).
    pub(crate) fn check_owner(&self, what: &str, owner: &str, peer: &Peer) -> Result<(), Error> {
        match self.bound(owner) {
            Some(b) if !b.admits(peer) => {
                Err(self.deny(what, "-", owner, peer, format!("{owner} is the program {}", b.exe)))
            }
            _ => Ok(()),
        }
    }

    /// Whether `peer`, saying it is `as_owner`, gets the clear value of the secret `d`.
    pub(crate) fn check_reveal(&self, d: &KeyDef, as_owner: &str, peer: &Peer) -> Result<(), Error> {
        if as_owner == d.owner && self.secrets.get(&d.name).is_some_and(|b| b.admits(peer)) {
            return Ok(());
        }
        let msg = format!("{}: only its owner's program reads a secret", d.name);
        // Under another owner's name the answer is no, whoever asks.
        let no = if as_owner == d.owner { Self::deny } else { Self::refuse };
        Err(no(self, "get", &d.name, as_owner, peer, msg))
    }

    /// Whether `peer` may write the secret `key` (`set`, `reset`); other keys are not this module's business.
    /// The owner's program may. Anybody else's `confirmed` counts only when it can be Georg's own yes
    /// (`vouch.rs`): another program of his user must not be able to clear or replace a stored key.
    pub fn check_secret_write(&self, key: &str, peer: &Peer, confirmed: bool) -> Result<(), Error> {
        let Some(d) = self.defs.get(key).filter(|d| d.safety == Safety::Secret) else { return Ok(()) };
        if self.secrets.get(key).is_some_and(|b| b.admits(peer)) {
            return Ok(());
        }
        let msg = format!(
            "{key}: Einen gespeicherten Schlüssel ändert oder löscht nur das Programm, dem er gehört, oder eine \
             Änderung, die du selbst bestätigst: {} (settings set --confirm, settings reset --confirm).",
            crate::vouch::WHERE
        );
        if !confirmed {
            return Err(self.deny("set", key, &d.owner, peer, msg));
        }
        self.vouches(Some(peer)).map_err(|why| self.not_confirmer("set", key, Some(peer), &why, format!("{msg} {}", crate::vouch::NOT_YOURS)))
    }

    /// Whether `owner`, asked for by `peer`, is the system program `holder`.
    fn is_holder(&self, holder: &str, owner: &str, peer: &Peer) -> bool {
        let path = self.sysdir.as_deref().map(|d| d.join(holder));
        owner == holder && peer.uid.is_some() && path.is_some() && peer.exe.as_deref().map(Path::new) == path.as_deref()
    }

    /// The checks of a registration that concern secrets; nothing is changed here.
    pub(crate) fn check_register(&self, owner: &str, defs: &[KeyDef], peer: &Peer) -> Result<(), Error> {
        self.check_owner("register", owner, peer)?;
        for d in defs {
            if let Some(b) = self.secrets.get(&d.name)
                && b.owner != owner
            {
                let msg = format!("{}: a secret of {}", d.name, b.owner);
                return Err(self.refuse("register", &d.name, owner, peer, msg));
            }
            // A namespace of one's own stays with the owner of the secrets in it, also before it is back.
            let ns = key::group(&d.name);
            if !builtin::SHARED.contains(&ns)
                && let Some(b) = self.secrets.iter().find(|(k, b)| key::group(k) == ns && b.owner != owner).map(|(_, b)| b)
            {
                let msg = format!("{}: namespace {ns} holds a secret of {}", d.name, b.owner);
                return Err(self.refuse("register", &d.name, owner, peer, msg));
            }
            let holder = builtin::HOLDERS.iter().find(|(p, _)| under(&d.name, p)).map(|(_, o)| *o);
            if let Some(h) = holder
                && !self.is_holder(h, owner, peer)
            {
                let msg = format!("{}: these keys are the program {h}'s", d.name);
                // Under another owner name the answer is no, whoever asks.
                let no = if owner == h { Self::deny } else { Self::refuse };
                return Err(no(self, "register", &d.name, owner, peer, msg));
            }
            if d.safety != Safety::Secret {
                continue;
            }
            if peer.ident().is_none() {
                let msg = format!("{}: a secret needs an owner settingsd can tell (a program on a socket)", d.name);
                return Err(self.deny("register", &d.name, owner, peer, msg));
            }
            if builtin::SHARED.contains(&ns) && holder.is_none() {
                let msg = format!("{}: the shared namespace {ns} holds no secrets here", d.name);
                return Err(self.refuse("register", &d.name, owner, peer, msg));
            }
        }
        Ok(())
    }

    /// Records the secrets of a registration that passed [`Registry::check_register`]. A key the owner now
    /// registers as an ordinary one loses its record (its own choice: the value is then shown to all).
    pub(crate) fn record(&mut self, owner: &str, defs: &[KeyDef], peer: &Peer) {
        let before = self.secrets.clone();
        // Without a program no secret is among `defs` and no record is this owner's (check_register).
        let (Some(uid), Some(exe)) = (peer.uid, peer.exe.as_deref()) else { return };
        // A record root made becomes the first user's who registers; a user's record stays that user's.
        let uid = match self.bound(owner) {
            Some(b) if b.uid != 0 => b.uid,
            _ => uid,
        };
        for b in self.secrets.values_mut().filter(|b| b.owner == owner) {
            b.uid = uid;
        }
        for d in defs {
            if d.safety == Safety::Secret {
                let b = Bound { owner: owner.to_string(), uid, exe: exe.to_string() };
                self.secrets.insert(d.name.clone(), b);
            } else {
                self.secrets.remove(&d.name);
            }
        }
        self.store_owners(&before);
    }

    /// Binds the secret `key`, which settingsd defines itself (`providers.rs`), to the system program `holder`:
    /// the program of that name in settingsd's own directory, wherever an older record pointed. The record
    /// is for every user of the program, as one root made, until the program registers as a user (`record`);
    /// a user it already belongs to keeps it. Kept in memory here; the caller decides when to store.
    pub(crate) fn hold(&mut self, key: &str, holder: &str) {
        let Some(exe) = self.sysdir.as_deref().map(|d| d.join(holder)).and_then(|p| p.to_str().map(str::to_string)) else {
            self.ev.emit(Level::Warn, "holder.unknown", &format!("key={key} holder={holder}"));
            return;
        };
        let uid = match self.secrets.get(key) {
            Some(b) if b.owner == holder && b.exe == exe => return,
            Some(b) if b.owner == holder => b.uid,
            _ => self.bound(holder).map_or(0, |b| b.uid),
        };
        self.secrets.insert(key.to_string(), Bound { owner: holder.to_string(), uid, exe });
    }

    /// Drops the records of keys whose values were forgotten.
    pub(crate) fn forget_records(&mut self, keys: &BTreeSet<String>) {
        let before = self.secrets.clone();
        self.secrets.retain(|k, _| !keys.contains(k));
        self.store_owners(&before);
    }

    /// Drops the records the stick still had for keys the fallback's changes left with neither a definition
    /// nor a value: a provider removed while the stick was away (`providers.rs`). Before the pending keys
    /// are written back.
    pub(crate) fn drop_forgotten_records(&mut self) {
        let kept = |k: &String| self.defs.contains_key(k) || self.values.contains_key(k) || self.orphans.contains_key(k);
        let gone: BTreeSet<String> = self.pending.iter().filter(|k| self.secrets.contains_key(*k) && !kept(k)).cloned().collect();
        self.secrets.retain(|k, _| !gone.contains(k));
    }

    pub(crate) fn owners_job(&self, place: &Place) -> Job {
        let path = self.paths.dir(place).join(FILE);
        if self.secrets.is_empty() { Job::Remove(path) } else { Job::Write(path, text(&self.secrets).into_bytes()) }
    }

    pub(crate) fn store_owners(&mut self, before: &Records) {
        if *before == self.secrets {
            return;
        }
        if let Err(e) = self.writer.run(vec![self.owners_job(&self.place)]) {
            self.ev.emit(Level::Warn, "store.failed", &format!("what=owners error={}", q(&e)));
        }
    }

    /// Takes in the records of a place just read (the stick coming back, the fallback at a start). `theirs`
    /// win over a different record held here, and a key registered meanwhile without its record's check loses
    /// its definition: the stored secret stays with the settings nobody registered until its owner comes.
    pub(crate) fn merge_records(&mut self, theirs: &Records) {
        let mut dropped = BTreeSet::new();
        for (k, b) in theirs {
            if self.secrets.get(k).is_some_and(|m| m.owner == b.owner && m.exe == b.exe) {
                continue;
            }
            self.secrets.insert(k.clone(), b.clone());
            if let Some(d) = self.defs.remove(k) {
                if let Some(v) = self.values.remove(k) {
                    self.orphans.insert(k.clone(), v);
                }
                // What the unchecked registration wrote meanwhile does not replace the owner's stored value.
                self.pending.remove(k);
                self.ev.emit(
                    Level::Warn,
                    "owner.reclaimed",
                    &format!("key={k} owner={} exe={} registered_as={}", q(&b.owner), q(&b.exe), q(&d.owner)),
                );
                dropped.insert((d.owner, k.clone()));
            }
        }
        self.release_namespaces();
        let owners: BTreeSet<&str> = dropped.iter().map(|(o, _)| o.as_str()).collect();
        for o in owners {
            let keys: BTreeSet<String> = dropped.iter().filter(|(x, _)| x == o).map(|(_, k)| k.clone()).collect();
            self.notify_keys("unregistered", o, &keys);
        }
    }
}
