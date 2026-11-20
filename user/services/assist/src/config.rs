//! What the settings say about AI: the providers Georg added, which one the chat uses, "Nur lokal", the limits
//! and the one level (`ai.level`). Read anew for every question, so a change takes effect at once. This file
//! also holds the rules that decide whether a provider may be contacted at all, before any connection exists.

use crate::net::url::{Url, is_private};
use crate::provider::{Kind, Place, Provider, program};
use crate::secret::Secret;
use crate::settings::{self, Refused, Settings};
use crate::tools::permit::Level;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub kind: String,
    pub address: String,
    pub model: String,
    pub place: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The ids of `ai.providers`, in Georg's order.
    pub ids: Vec<String>,
    pub entries: BTreeMap<String, Entry>,
    pub chat: String,
    /// `ai.role.chat_fallback`: who is asked when the one before is away, in Georg's order.
    pub fallback: Vec<String>,
    pub local_only: bool,
    pub timeout: Duration,
    pub retries: u32,
    pub tool_rounds: u32,
    pub max_tokens: u64,
    /// `ai.level`: how much the assistant may do unasked (K2). A value that is no level is `ask`.
    pub level: Level,
    /// `ai.mcp.ask_tools`: a tool the chat would ask about runs for a program at `assist --mcp`.
    pub mcp_ask_tools: bool,
    /// The assistant's name in the chat (`ai.assistant.name`). Bounded ([`short_name`]): it goes into the
    /// system prompt of every question.
    pub name: String,
    /// `ai.cloud_notice`: Georg is told when a question goes to a cloud provider ("geht an <Anbieter>").
    /// settingsd brings the key and the name's (CL-5s), so no other program chooses their defaults.
    pub cloud_notice: bool,
}

/// What a provider may be called and how the list `ai.providers` is read: settingsd's rules, since it makes
/// the provider keys from that list (CL-5s).
pub use settingsd::providers::{ids as parse_ids, valid_id};

/// The most a name may take: what settingsd allows `ai.assistant.name`.
const NAME_MAX: usize = 64;

/// A name as it goes into the system prompt and onto the sidebar: its first line, without control
/// characters, at most [`NAME_MAX`] bytes. Wherever the name comes from, it cannot bring a text of its own
/// into every question.
fn short_name(raw: &str) -> String {
    let line: String = raw.trim().lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).collect();
    let mut end = line.len().min(NAME_MAX);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].trim().to_string()
}

pub fn load(s: &dyn Settings) -> Result<Config, Refused> {
    let ai = settings::listing(s, "ai")?;
    let ids = parse_ids(ai.get("ai.providers").and_then(|(v, _)| v.as_str()).unwrap_or(""));
    let text = |k: &str| ai.get(k).and_then(|(v, _)| v.as_str()).unwrap_or("").trim().to_string();
    let int = |k: &str, default: i64| ai.get(k).and_then(|(v, _)| v.as_i64()).unwrap_or(default);
    let entries = ids
        .iter()
        .map(|id| {
            let f = |name: &str| text(&format!("ai.provider.{id}.{name}"));
            (id.clone(), Entry { kind: f("kind"), address: f("address"), model: f("model"), place: f("place") })
        })
        .collect();
    let name = ai.get("ai.assistant.name").and_then(|(v, _)| v.as_str()).map(short_name).filter(|n| !n.is_empty());
    Ok(Config {
        ids,
        entries,
        chat: text("ai.role.chat"),
        fallback: parse_ids(&text("ai.role.chat_fallback")),
        local_only: ai.get("ai.local_only").and_then(|(v, _)| v.as_bool()).unwrap_or(false),
        timeout: Duration::from_secs(u64::try_from(int("ai.timeout_s", 600)).unwrap_or(600)),
        retries: u32::try_from(int("ai.retries", 2)).unwrap_or(2),
        tool_rounds: u32::try_from(int("ai.tool_rounds", 20)).unwrap_or(20),
        max_tokens: u64::try_from(int("ai.max_tokens", 64_000)).unwrap_or(64_000),
        level: Level::parse(&text(Level::KEY)).unwrap_or(Level::Ask),
        mcp_ask_tools: ai.get("ai.mcp.ask_tools").and_then(|(v, _)| v.as_bool()).unwrap_or(false),
        name: name.unwrap_or_else(|| "Assistent".into()),
        cloud_notice: ai.get("ai.cloud_notice").and_then(|(v, _)| v.as_bool()).unwrap_or(true),
    })
}

impl Config {
    /// The providers the chat may use, first choice first; Err explains in German what to set.
    pub fn candidates(&self) -> Result<Vec<String>, String> {
        if self.ids.is_empty() {
            return Err(crate::de::howto());
        }
        if self.chat.is_empty() {
            return Err(format!(
                "Noch ist kein Anbieter für das Gespräch gewählt. Eingerichtet sind: {}.\nWähle einen: settings set ai.role.chat NAME",
                self.ids.join(", ")
            ));
        }
        let mut out = vec![self.chat.clone()];
        for id in &self.fallback {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        Ok(out)
    }

    /// The provider `id` without its key, or what is still missing (German).
    pub fn provider(&self, id: &str) -> Result<Provider, String> {
        let Some(e) = self.entries.get(id) else {
            return Err(format!("Der Anbieter „{id}“ ist nicht eingerichtet (ai.providers nennt ihn nicht). Anlegen: assist --add {id}"));
        };
        let kind = Kind::parse(&e.kind).ok_or_else(|| format!("Beim Anbieter „{id}“ fehlt die Art: settings set ai.provider.{id}.kind {}", Kind::NAMES.join("|")))?;
        let place = match e.place.as_str() {
            "cloud" => Place::Cloud,
            "local" => Place::Local,
            _ => return Err(format!("Beim Anbieter „{id}“ fehlt, ob er in der Cloud oder lokal läuft: settings set ai.provider.{id}.place cloud|local (das legst du fest, BitsOS rät es nicht)")),
        };
        if e.address.is_empty() {
            return Err(format!("Beim Anbieter „{id}“ fehlt die Adresse: settings set ai.provider.{id}.address ADRESSE ({})", crate::de::address_hint(kind)));
        }
        if kind == Kind::Program {
            // A helper needs no model name (it may take one) and has no key.
            program::check(&e.address).map_err(|why| format!("Das Hilfsprogramm des Anbieters „{id}“ ist nicht brauchbar ({}): {why} (ai.provider.{id}.address)", e.address))?;
            return Ok(Provider { id: id.to_string(), kind, address: e.address.clone(), model: e.model.clone(), key: Secret::default(), place, max_tokens: self.max_tokens });
        }
        Url::parse(&e.address)
            .map_err(|why| format!("Die Adresse des Anbieters „{id}“ ist nicht brauchbar: {why} (ai.provider.{id}.address).{}", crate::de::not_an_address(&e.address)))?;
        if e.model.is_empty() {
            return Err(format!("Beim Anbieter „{id}“ fehlt das Modell: settings set ai.provider.{id}.model MODELL"));
        }
        Ok(Provider { id: id.to_string(), kind, address: e.address.clone(), model: e.model.clone(), key: Secret::default(), place, max_tokens: self.max_tokens })
    }

    /// May `p` be contacted at all? Checked before a name is looked up or a socket opened.
    pub fn admit(&self, p: &Provider) -> Result<(), String> {
        if self.local_only && p.place == Place::Cloud {
            return Err(format!("„Nur lokal“ ist eingeschaltet (ai.local_only): „{}“ läuft in der Cloud und wird nicht angesprochen.", p.id));
        }
        // A helper program has no address on the network; where it sends the text is its own doing, and
        // `place` is Georg's word for that.
        let https = p.kind == Kind::Program || Url::parse(&p.address).is_ok_and(|u| u.https);
        if !https && p.place == Place::Cloud {
            return Err(format!("Der Anbieter „{}“ läuft in der Cloud, seine Adresse beginnt aber mit http:// (unverschlüsselt). Das geht nur bei lokalen Anbietern im Heimnetz.", p.id));
        }
        Ok(())
    }
}

/// What the addresses a provider's name resolves to must satisfy, every one of them, before a socket is
/// opened: plain http goes only to the home network or this machine, and with "Nur lokal" so does https,
/// whatever the provider's `place` says (the label is Georg's word, the address is where the text goes).
pub fn admit_addrs(p: &Provider, https: bool, local_only: bool, addrs: &[SocketAddr]) -> Result<(), String> {
    let Some(a) = addrs.iter().find(|a| !is_private(a.ip())) else { return Ok(()) };
    if local_only {
        return Err(format!(
            "„Nur lokal“ ist eingeschaltet (ai.local_only): „{}“ ist als lokal eingetragen, seine Adresse führt aber aus dem Heimnetz hinaus ({}). Er wird nicht angesprochen.",
            p.id,
            a.ip()
        ));
    }
    if https {
        return Ok(());
    }
    Err(format!("Der Anbieter „{}“ hat eine Adresse mit http:// (unverschlüsselt), die aus dem Heimnetz hinausführt ({}). Das wird nicht angesprochen; nimm https://.", p.id, a.ip()))
}

pub fn key_missing(id: &str) -> String {
    format!("Beim Anbieter „{id}“ fehlt der Schlüssel. Trag ihn ein: assist --key {id}")
}

/// Puts the key into `p`, read from settingsd at the last moment. A cloud provider without one is not asked.
pub fn with_key(s: &dyn Settings, mut p: Provider) -> Result<Provider, String> {
    // A helper program gets no key: none is read for it.
    if p.kind == Kind::Program {
        return Ok(p);
    }
    let key = settings::get_secret(s, &format!("ai.provider.{}.key", p.id)).map_err(|e| format!("Der Schlüssel von „{}“ ließ sich nicht lesen ({}).", p.id, e.code))?;
    if key.is_empty() && p.place == Place::Cloud {
        return Err(key_missing(&p.id));
    }
    p.key = Secret::new(key);
    Ok(p)
}
