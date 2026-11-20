//! The AI providers Georg added (CL-5s; V2-PLAN §8a.3, DECISIONS 21.10.). `ai.providers` lists their names; for
//! every name settingsd makes the five keys `ai.provider.<id>.{kind,address,model,key,place}` itself, so they
//! exist from boot and the settings page "KI" adds a provider by putting its name on the list.
//!
//! The keys are `assist`'s (owner and applier): it reads them for every question, and it alone reads the
//! secret `key`, as the program of that name in settingsd's own directory (`builtin::HOLDERS`). settingsd makes
//! the secret's record (`owners.rs`) together with the definition, so nobody has to register first.
//!
//! Taking a name off the list removes the provider: its values, the stored key, their journal entries and the
//! key's record, in the same step and the same write of `ai.toml` as the list change. Whenever that loses
//! something stored it needs Georg's own yes: `"confirmed": true` from the session's compositor or from root
//! (`vouch.rs`; from anybody else `not_confirmer`). Only a request removes. A list that comes from a file (a
//! start, the stick coming back) never deletes: what is stored for a name missing there stays in the file as
//! settings nobody registered and comes back with the name.
//!
//! A removed provider also leaves every role (`ai.role.*`) that named it, each as a change of its own in the
//! journal just before the list's, so no role is left pointing at a name that is gone. What a request
//! writes into a role is not checked against the list (the page may set the role first); `assist` says in
//! German when a role names a provider that is not there.

use crate::change::Who;
use crate::events::{Level, q};
use crate::key::{Applier, KeyDef, Restart, Safety};
use crate::registry::{Error, Registry, under};
use crate::store::Place;
use crate::value::{Type, Value};
use std::collections::{BTreeSet, HashSet};

pub const LIST: &str = "ai.providers";
const PREFIX: &str = "ai.provider";
const ROLES: &str = "ai.role";
/// A role's fallback list: `ai.role.<job>_fallback`.
const FALLBACK: &str = "_fallback";
/// The system program the provider keys are for.
pub const HOLDER: &str = "assist";
/// The value of a choice nobody set yet: kind and place have no default (nothing is guessed).
pub const UNSET: &str = "unset";

/// A provider id is one segment of a settings key.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != UNSET && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn words(list: &str) -> impl Iterator<Item = &str> {
    list.split([',', ' ', ';']).map(str::trim).filter(|s| !s.is_empty())
}

/// The ids a list names, in its order, each once. What is not an id is left out: a list from a file is read
/// as far as it can be (a request with such a word is refused, [`check_value`]).
pub fn ids(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in words(list).filter(|s| valid_id(s)) {
        if !out.iter().any(|i| i == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// What a request may write into the list and the role keys: provider names only. Err is German (Georg reads
/// it on the page).
pub fn check_value(key: &str, v: &Value) -> Result<(), String> {
    let Value::Text(t) = v else { return Ok(()) };
    let list = key == LIST || (under(key, ROLES) && key.ends_with(FALLBACK));
    if !list && !under(key, ROLES) {
        return Ok(());
    }
    if !list && words(t).count() > 1 {
        return Err("Hier steht genau ein Anbieter (mehrere der Reihe nach: der Schlüssel mit _fallback am Ende)".into());
    }
    match words(t).find(|w| !valid_id(w)) {
        Some(bad) => Err(format!("Der Name „{bad}“ geht nicht: nur Kleinbuchstaben, Ziffern, - und _ (zum Beispiel claude oder strata)")),
        None => Ok(()),
    }
}

/// The five keys of provider `id`. The byte limits are those `assist` registered the keys with before CL-5s
/// (stored values keep fitting): room for any address, model name or key in use today, with a wide margin.
pub fn keys(id: &str) -> Vec<KeyDef> {
    let choice = |c: &[(&str, &str)]| Type::Choice {
        values: c.iter().map(|p| p.0.to_string()).collect(),
        de: c.iter().map(|p| p.1.to_string()).collect(),
    };
    let k = |name: &str, ty: Type, default: &str, de: String, en: String| KeyDef {
        name: format!("{PREFIX}.{id}.{name}"),
        ty,
        default: Value::Text(default.into()),
        de,
        en,
        applier: Applier::Service(HOLDER.into()),
        restart: Restart::None,
        safety: if name == "key" { Safety::Secret } else { Safety::Normal },
        owner: HOLDER.into(),
    };
    vec![
        k(
            "kind",
            choice(&[
                (UNSET, "Nicht gesetzt"),
                ("anthropic", "Anthropic (Claude)"),
                ("openai", "OpenAI (ChatGPT)"),
                ("gemini", "Google (Gemini)"),
                ("openai_compatible", "OpenAI-kompatibel (lokale Programme)"),
                ("program", "Hilfsprogramm auf diesem Rechner"),
            ]),
            UNSET,
            format!("Anbieter {id}: welche Schnittstelle er spricht"),
            format!("Provider {id}: which interface it speaks"),
        ),
        k(
            "address",
            Type::Text { max: 2048 },
            "",
            format!("Anbieter {id}: Adresse (http:// nur lokal im Heimnetz; bei der Art program der volle Pfad des Hilfsprogramms)"),
            format!("Provider {id}: address (http:// only local, on the home network; for kind program the full path of the helper program)"),
        ),
        k("model", Type::Text { max: 512 }, "", format!("Anbieter {id}: Name des Modells"), format!("Provider {id}: model name")),
        k("key", Type::Text { max: 16384 }, "", format!("Anbieter {id}: Schlüssel (wird nie angezeigt)"), format!("Provider {id}: key (never shown)")),
        k(
            "place",
            choice(&[(UNSET, "Nicht gesetzt"), ("cloud", "Cloud"), ("local", "Lokal")]),
            UNSET,
            format!("Anbieter {id}: läuft in der Cloud oder lokal (das legst du fest)"),
            format!("Provider {id}: runs in the cloud or locally (you decide)"),
        ),
    ]
}

fn prefix(id: &str) -> String {
    format!("{PREFIX}.{id}")
}

impl Registry {
    fn listed(&self, list: Option<&Value>) -> Vec<String> {
        ids(&list.map(Value::show).unwrap_or_default())
    }

    /// The providers a change of the list to `new` takes away. Err: something is stored for one of them and
    /// the change does not carry Georg's own yes (`vouch.rs`: `confirmed` from the compositor or root).
    pub(crate) fn leaving(&self, new: Option<&Value>, who: &Who) -> Result<Vec<String>, Error> {
        let stay = self.listed(new);
        let gone: Vec<String> = self.listed(self.values.get(LIST)).into_iter().filter(|id| !stay.contains(id)).collect();
        let stored = |id: &String| self.values.keys().chain(self.orphans.keys()).any(|k| under(k, &prefix(id)));
        if let Some(id) = gone.iter().find(|id| stored(id)) {
            let what = format!("Der Anbieter „{id}“ wird damit entfernt, mit allen seinen Angaben und seinem Schlüssel.");
            self.own_yes("remove", LIST, who, &what)?;
        }
        Ok(gone)
    }

    /// The roles that name one of `gone`, each with what is left of its value (None: nothing, the default).
    pub(crate) fn roles_without(&self, gone: &[String]) -> Vec<(KeyDef, Option<Value>)> {
        let mut out = Vec::new();
        for (k, v) in self.values.iter().filter(|(k, _)| under(k, ROLES)) {
            let (Value::Text(t), Some(d)) = (v, self.defs.get(k)) else { continue };
            if !words(t).any(|w| gone.iter().any(|g| g == w)) {
                continue;
            }
            let left: Vec<&str> = words(t).filter(|w| !gone.iter().any(|g| g == w)).collect();
            let new = Value::Text(left.join(", "));
            out.push((d.clone(), if new == d.default { None } else { Some(new) }));
        }
        out
    }

    /// Takes the providers `gone` out of memory: definitions, values, journal entries. The caller stores (the
    /// list's own change writes `ai.toml` and the journal). Returns their keys and whether the journal lost
    /// entries.
    pub(crate) fn drop_providers(&mut self, gone: &[String], source: &str) -> (BTreeSet<String>, bool) {
        let mut keys = BTreeSet::new();
        for id in gone {
            let p = prefix(id);
            let mine: Vec<String> =
                self.defs.keys().chain(self.values.keys()).chain(self.orphans.keys()).filter(|k| under(k, &p)).cloned().collect();
            let had_key = self.holds_key(id);
            for k in &mine {
                self.defs.remove(k);
                self.values.remove(k);
                self.orphans.remove(k);
            }
            self.made.remove(id);
            self.ev.emit(Level::Info, "provider.removed", &format!("provider={} key_stored={had_key} source={}", q(id), q(source)));
            keys.extend(mine);
        }
        let entries: HashSet<String> = keys.iter().cloned().collect();
        let forgot = !entries.is_empty() && self.journal.forget(&entries);
        (keys, forgot)
    }

    /// After the list's change is stored: the removed keys' records go, on the fallback the stick is told at
    /// the resync, and the keys of the names that are new get made.
    pub(crate) fn settle_providers(&mut self, gone: &BTreeSet<String>) {
        if matches!(self.place, Place::Stick) {
            // `change` put them there for the case that the write moved everything to the fallback.
            self.pending.retain(|k| !gone.contains(k));
        }
        if !gone.is_empty() {
            self.forget_records(gone);
            self.notify_keys("unregistered", HOLDER, gone);
        }
        self.sync_providers(true);
    }

    /// Another key that could not live in the same file as `name` (a key cannot be a group of keys).
    fn clash(&self, name: &str) -> Option<&str> {
        self.defs.keys().map(String::as_str).find(|k| *k != name && (under(k, name) || under(name, k)))
    }

    /// Makes the keys of every listed provider that has none yet, and drops those settingsd made for a name
    /// the list (as a file brought it) no longer holds; their values stay stored. `store`: write
    /// `owners.list` when a record was made (not at a start: the stick may not take a write yet, and the
    /// records are made again at every start anyway).
    pub(crate) fn sync_providers(&mut self, store: bool) {
        let ids = self.listed(self.values.get(LIST));
        let before = self.secrets.clone();
        let mut left = BTreeSet::new();
        for id in self.made.clone().into_iter().filter(|id| !ids.contains(id)) {
            self.made.remove(&id);
            let p = prefix(&id);
            let mine: Vec<String> = self.defs.values().filter(|d| d.owner == HOLDER && under(&d.name, &p)).map(|d| d.name.clone()).collect();
            for k in mine {
                self.defs.remove(&k);
                if let Some(v) = self.values.remove(&k) {
                    self.orphans.insert(k.clone(), v);
                }
                left.insert(k);
            }
        }
        let (mut came, mut dropped) = (BTreeSet::new(), BTreeSet::new());
        for id in &ids {
            let defs = keys(id);
            if let Some((d, c)) = defs.iter().find_map(|d| self.clash(&d.name).map(|c| (d, c))) {
                self.ev.emit(Level::Warn, "provider.skipped", &format!("provider={} key={} conflicts_with={}", q(id), d.name, q(c)));
                continue;
            }
            self.made.insert(id.clone());
            for d in defs {
                if d.safety == Safety::Secret {
                    self.hold(&d.name, HOLDER);
                }
                if self.defs.contains_key(&d.name) {
                    continue;
                }
                // What was stored for the name earlier (before a restart, by an older assist) is its value again.
                if let Some(raw) = self.orphans.remove(&d.name) {
                    match self.coerce(&d, &raw) {
                        Ok(Some(v)) => {
                            self.values.insert(d.name.clone(), v);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            self.ev.emit(Level::Warn, "value.dropped", &format!("key={} reason={}", d.name, q(&e)));
                            dropped.insert(d.name.clone());
                        }
                    }
                }
                came.insert(d.name.clone());
                self.defs.insert(d.name.clone(), d);
            }
        }
        if !dropped.is_empty() {
            self.rewrite_groups(&dropped);
        }
        if store {
            self.store_owners(&before);
        }
        if !left.is_empty() {
            self.notify_keys("unregistered", HOLDER, &left);
        }
        if !came.is_empty() {
            self.ev.emit(Level::Info, "providers", &format!("listed={} keys_made={}", ids.len(), came.len()));
            self.notify_keys("registered", HOLDER, &came);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_is_read_as_far_as_it_can_be() {
        assert_eq!(ids("claude, strata;l-1  claude"), ["claude", "strata", "l-1"]);
        assert_eq!(ids("Claude, unset, a.b, ok"), ["ok"]);
        assert!(ids("").is_empty());
    }

    #[test]
    fn a_request_names_providers_only() {
        let t = |s: &str| Value::Text(s.into());
        assert!(check_value(LIST, &t("claude, strata")).is_ok());
        assert!(check_value(LIST, &t("")).is_ok());
        assert!(check_value(LIST, &t("claude, Strata")).unwrap_err().contains("„Strata“"));
        assert!(check_value("ai.role.chat", &t("claude")).is_ok());
        assert!(check_value("ai.role.chat", &t("claude, strata")).is_err());
        assert!(check_value("ai.role.jobs_fallback", &t("claude, strata")).is_ok());
        assert!(check_value("ai.role.jobs_fallback", &t("a.b")).is_err());
        // Not this module's keys.
        assert!(check_value("ai.assistant.name", &t("Mein Helfer!")).is_ok());
        assert!(check_value("ai.provider.x.model", &t("Some Model 4.5")).is_ok());
    }

    #[test]
    fn the_five_keys_are_assists_and_one_is_secret() {
        let ks = keys("claude");
        assert_eq!(ks.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["ai.provider.claude.kind", "ai.provider.claude.address", "ai.provider.claude.model", "ai.provider.claude.key", "ai.provider.claude.place"]);
        for d in &ks {
            d.validate().unwrap();
            assert_eq!((d.owner.as_str(), &d.applier), (HOLDER, &Applier::Service(HOLDER.into())));
            assert_eq!(d.safety == Safety::Secret, d.name.ends_with(".key"), "{}", d.name);
        }
    }
}
