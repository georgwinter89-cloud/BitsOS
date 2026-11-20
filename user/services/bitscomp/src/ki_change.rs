//! A working provider is not broken by one click: a change of the kind or
//! the address of the provider that answers in the chat (`ai.role.chat`, or one on its fallback list) is asked
//! first in the page's dialog, in plain words, with how "Rückgängig" brings the old value back. Every change of a
//! kind or an address says so on the answer line once settingsd took it. The asked change goes only while the
//! value is still the one the dialog showed (settingsd's `while`).

use mcpreg::Json;

use crate::ki::{self, Facts, PROGRAM, kind_name};
use crate::ki_ask::What;
use crate::settings_write::{Done, Req};
use crate::state::App;

/// A value as the page names it: a kind by its German name, an empty address as such.
fn shown(field: &str, v: &str) -> String {
    match field {
        "kind" => kind_name(if v.is_empty() { ki::UNSET } else { v }),
        _ if v.trim().is_empty() => "leer".into(),
        _ => v.trim().to_string(),
    }
}

fn field_name(field: &str) -> &'static str {
    if field == "kind" { "Art" } else { "Adresse" }
}

/// Is `id` the chat's provider: "antwortet" (the role) or "Ersatz" (its fallback list); None: neither.
pub fn chat_role(f: &Facts, id: &str) -> Option<&'static str> {
    let has = |fallback: bool| f.roles.get(&ki::role_key("chat", fallback)).is_some_and(|v| ki::ids(v).iter().any(|i| i == id));
    if has(false) {
        Some("antwortet")
    } else if has(true) {
        Some("Ersatz")
    } else {
        None
    }
}

/// What the provider still needs after the change (one line), if anything.
fn needs(f: &Facts, id: &str, field: &str, to: &str) -> Option<String> {
    let p = f.provider(id)?;
    let (kind, address) = if field == "kind" { (to, p.address.as_str()) } else { (p.kind.as_str(), to) };
    if kind == PROGRAM {
        return (!address.trim().starts_with('/')).then(|| "Ein Hilfsprogramm braucht danach den ganzen Pfad des Programms statt einer Adresse.".to_string());
    }
    if kind == ki::UNSET {
        return None;
    }
    if crate::ki_key::host_of(address).is_err() {
        return Some("Danach braucht er eine Adresse im Netz und einen Schlüssel.".to_string());
    }
    (field == "kind" && !p.stored).then(|| "Danach braucht er einen Schlüssel für diese Art.".to_string())
}

/// The dialog's title, lines and yes for changing `key` from `from` to `to`.
pub fn lines(f: &Facts, key: &str, from: &str, to: &str) -> (String, Vec<String>, &'static str) {
    let Some((id, field)) = ki::provider_of(key) else { return (String::new(), Vec::new(), "Ändern") };
    let mut lines = vec![match chat_role(f, id) {
        Some("antwortet") => format!("„{id}“ antwortet gerade, wenn du mit dem Assistenten sprichst."),
        _ => format!("„{id}“ springt im Gespräch ein, wenn der erste Anbieter nicht antwortet."),
    }];
    lines.push(format!("Jetzt: {}", shown(field, from)));
    lines.push(format!("Neu: {}", shown(field, to)));
    lines.extend(needs(f, id, field, to));
    lines.push("Bis alle seine Angaben wieder zueinander passen, antwortet er nicht.".into());
    lines.push("„Rückgängig“ oben rechts auf dieser Seite holt den alten Wert zurück.".into());
    (format!("{} von „{id}“ ändern?", field_name(field)), lines, "Ändern")
}

/// The answer line after a helper became provider `id`.
pub fn adopted_line(helper: &str, id: &str, placed: bool, f: &Facts) -> String {
    let mut s = format!("„{helper}“ ist jetzt als Anbieter „{id}“ eingetragen.");
    if !placed {
        s.push_str(" Trag in seinem Abschnitt noch den Ort ein (Cloud oder Lokal).");
    }
    if chat_role(f, id) != Some("antwortet") {
        s.push_str(" „Fürs Gespräch nehmen“ lässt es im Gespräch antworten.");
    }
    s
}

impl App {
    /// The page wants `key` changed from `from` to `to`: a provider's kind or address of the chat's provider is
    /// asked first; everything else goes at once.
    pub(crate) fn ki_change(&mut self, pid: u32, key: &str, from: &str, to: &str) {
        let watched = ki::provider_of(key).filter(|(_, field)| matches!(*field, "kind" | "address"));
        let Some((id, field)) = watched else { return self.page_send(pid, Req::set(key, Json::str(to))) };
        let f = ki::facts(&self.settings);
        if chat_role(&f, id).is_some() {
            return self.ki_ask(pid, What::Change { key: key.into(), from: from.into(), to: to.into() });
        }
        let before = (shown(field, from), shown(field, to));
        if self.ki_send(pid, Req::set(key, Json::str(to))) {
            self.ki.before.insert(key.into(), before);
        }
    }

    /// Georg said "Ändern": it goes only while the value is still the one the dialog showed.
    pub(crate) fn ki_change_confirmed(&mut self, pid: u32, key: String, from: String, to: String) {
        let Some((_, field)) = ki::provider_of(&key) else { return };
        let before = (shown(field, &from), shown(field, &to));
        let r = Req::Guarded { key: key.clone(), value: Json::str(&to), whilst: vec![(key.clone(), Json::str(&from))] };
        eprintln!("bitscomp: ki: Georg confirmed the change of {key}");
        if self.ki_send(pid, r) {
            self.ki.before.insert(key, before);
        }
    }

    /// settingsd answered a change of a kind or an address: the page's line, with how to get the old one back.
    pub(crate) fn ki_changed(&mut self, key: &str, d: &Done) -> Option<String> {
        let (from, to) = self.ki.before.remove(key)?;
        let (id, field) = ki::provider_of(key)?;
        if !d.ok {
            return Some(format!("{} von „{id}“ nicht geändert: {}", field_name(field), d.text));
        }
        Some(format!("{} von „{id}“ ist jetzt „{to}“. „Rückgängig“ oben rechts holt „{from}“ zurück.", field_name(field)))
    }
}
