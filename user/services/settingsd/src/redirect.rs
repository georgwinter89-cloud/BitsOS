//! A stored provider key goes only where it was entered for (INT-CL3T; CL-S change request 5).
//!
//! `ai.provider.<id>.address`, `.kind` and `.place` decide where and how the provider's key is sent, and they
//! are ordinary settings: any program of Georg's user may set them. So while a key is stored for `<id>`, these
//! three do not change, whoever asks and whatever the request claims (`confirmed` is a flag any program can
//! send, and a question on a terminal can be answered by whoever holds that terminal). The one confirmation
//! no program can fake is the key itself: remove it, change the provider, enter the key again
//! (`assist --key <id>` shows where it will go).
//!
//! The same holds for what comes back from the fallback: a change of one of the three made while the stick was
//! away does not reach a provider whose key the stick holds, unless the key was written on the fallback too.

use crate::change::Who;
use crate::events::{Level, q};
use crate::key::KeyDef;
use crate::registry::{Error, Registry, err};
use crate::value::Value;
use std::collections::BTreeSet;

/// The namespace of the providers (`builtin::HOLDERS` gives it to `assist`).
const PROVIDERS: &str = "ai.provider.";
/// The provider's secret.
const KEY: &str = "key";
/// What decides where and how the secret is sent.
const BOUND: [&str; 3] = ["address", "kind", "place"];
pub const CODE: &str = "key_bound";

/// `ai.provider.<id>.<field>` -> (`<id>`, `<field>`).
fn field(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix(PROVIDERS)?.split_once('.')
}

/// The provider whose key is bound to the setting `key`.
pub(crate) fn provider_of(key: &str) -> Option<&str> {
    field(key).filter(|(_, f)| BOUND.contains(f)).map(|(id, _)| id)
}

fn key_of(id: &str) -> String {
    format!("{PROVIDERS}{id}.{KEY}")
}

impl Registry {
    /// Whether a key is stored for provider `id`, registered or not yet (settingsd forgets registrations
    /// when it restarts, the values stay).
    pub(crate) fn holds_key(&self, id: &str) -> bool {
        let k = key_of(id);
        self.values.get(&k).or_else(|| self.orphans.get(&k)).is_some_and(|v| !v.show().is_empty())
    }

    /// Refuses a change of `d` that would send a stored key somewhere else.
    pub(crate) fn check_redirect(&self, d: &KeyDef, op: &str, who: &Who) -> Result<(), Error> {
        let Some(id) = provider_of(&d.name).filter(|id| self.holds_key(id)) else { return Ok(()) };
        let asker = who.asker.map_or_else(String::new, |p| format!(" {}", p.show()));
        self.ev.emit(
            Level::Warn,
            "redirect.refused",
            &format!("key={} provider={} op={op} source={}{asker}", q(&d.name), q(id), q(who.source)),
        );
        Err(err(
            CODE,
            format!(
                "{}: Für den Anbieter {id} ist ein Schlüssel gespeichert; er gilt für Adresse, Art und Ort, wie sie jetzt \
                 sind. Erst den Schlüssel löschen (in den Einstellungen auf der Seite „KI“, oder als root an der \
                 Konsole: settings reset --confirm {}), dann das hier ändern, dann den Schlüssel neu eingeben (auf \
                 der Seite „KI“ oder mit assist --key {id})",
                d.name,
                key_of(id)
            ),
        ))
    }

    /// The fallback's pending changes that may reach the stick's values: without those that would move a key
    /// the stick holds (`self.values`/`orphans` are the stick's here) and that was not written on the
    /// fallback as well.
    pub(crate) fn without_redirects(&self, pending: &BTreeSet<String>) -> BTreeSet<String> {
        let moved = |k: &String| provider_of(k).is_some_and(|id| self.holds_key(id) && !pending.contains(&key_of(id)));
        for k in pending.iter().filter(|k| moved(k)) {
            self.ev.emit(Level::Warn, "redirect.dropped", &format!("key={} when=resync", q(k)));
        }
        pending.iter().filter(|k| !moved(k)).cloned().collect()
    }

    /// `set` only while other settings are what the asker saw: `{"key": value, ..}`, each compared with the
    /// value in effect. For the one who enters a provider's key after reading where it will go.
    pub(crate) fn check_while(&self, cond: &crate::json::Json) -> Result<(), Error> {
        let crate::json::Json::Obj(want) = cond else { return Err(err("bad_request", "while: want an object of key: value")) };
        for (k, v) in want {
            let now = self.effective(k).as_ref().map(Value::to_json);
            if now.as_ref() != Some(v) {
                return Err(err("changed_meanwhile", format!("{k} is not {} (any more): nothing changed", v.to_text())));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_settings_are_bound_to_a_key() {
        assert_eq!(provider_of("ai.provider.claude.address"), Some("claude"));
        assert_eq!(provider_of("ai.provider.claude.kind"), Some("claude"));
        assert_eq!(provider_of("ai.provider.l-1.place"), Some("l-1"));
        for free in ["ai.provider.claude.model", "ai.provider.claude.key", "ai.provider.claude", "ai.providers", "ai.role.chat", "ai.provider..address.x", "x.provider.claude.address"] {
            assert_eq!(provider_of(free), None, "{free}");
        }
        assert_eq!(key_of("claude"), "ai.provider.claude.key");
    }
}
