//! A settings key: everything V2-PLAN §8a.3 lists (name, type, range, default, German and English text, who
//! applies it, restart flag, safety class), plus its owner.

use crate::guard;
use crate::json::Json;
use crate::value::{Type, Value};

/// Who makes a new value take effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applier {
    /// settingsd itself (its own `settings.*` keys).
    Settingsd,
    /// A service that watches the key (bitscomp for `theme.*`); settingsd tells every watcher.
    Service(String),
    /// The kernel: a line in `\BITSOS2\CONFIG.TXT` (read at boot) and, when the kernel has it, a runtime file
    /// `/sys/bitsos/<sys>` (applied at once).
    Kernel { config: String, sys: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    None,
    /// The applying service must restart (its watchers are told, it decides when).
    Service,
    /// Only the next start of BitsOS reads it.
    Reboot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Safety {
    /// Anyone may change it (Claude included).
    Normal,
    /// A change needs `"confirmed": true` in the request (the caller asked Georg first; B3's `ask`).
    Confirm,
    /// The value never leaves settingsd except to its owner, and is never written to the journal.
    Secret,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyDef {
    pub name: String,
    pub ty: Type,
    pub default: Value,
    pub de: String,
    pub en: String,
    pub applier: Applier,
    pub restart: Restart,
    pub safety: Safety,
    pub owner: String,
}

impl Restart {
    pub fn name(self) -> &'static str {
        match self {
            Restart::None => "none",
            Restart::Service => "service",
            Restart::Reboot => "reboot",
        }
    }
    fn parse(s: &str) -> Option<Restart> {
        Some(match s {
            "none" => Restart::None,
            "service" => Restart::Service,
            "reboot" => Restart::Reboot,
            _ => return None,
        })
    }
}

impl Safety {
    pub fn name(self) -> &'static str {
        match self {
            Safety::Normal => "normal",
            Safety::Confirm => "confirm",
            Safety::Secret => "secret",
        }
    }
    fn parse(s: &str) -> Option<Safety> {
        Some(match s {
            "normal" => Safety::Normal,
            "confirm" => Safety::Confirm,
            "secret" => Safety::Secret,
            _ => return None,
        })
    }
}

/// `a.b_c.d`: lower-case ASCII segments of letters, digits, `_` and `-`, at least two segments (a namespace and
/// a name), so a key is also a valid file group and TOML bare key.
pub fn valid_name(name: &str) -> bool {
    let segs: Vec<&str> = name.split('.').collect();
    segs.len() >= 2
        && segs.iter().all(|s| {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        })
}

pub fn group(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

impl KeyDef {
    /// The checks every definition passes, built-in or registered: a sane name, a default of its own type, no
    /// R8 switch and no kernel secret.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err(format!("bad key name {:?} (want namespace.name, lower case)", self.name));
        }
        guard::refuse_key(&self.name)?;
        if let Applier::Kernel { config, sys } = &self.applier {
            guard::refuse_config_name(config)?;
            if config.is_empty() || !config.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(format!("bad CONFIG.TXT name {config:?}"));
            }
            if let Some(s) = sys
                && (s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            {
                return Err(format!("bad /sys/bitsos name {s:?}"));
            }
            if self.safety == Safety::Secret {
                return Err("a kernel key is never secret here (kernel secrets stay out of the registry)".into());
            }
        }
        if self.de.trim().is_empty() || self.en.trim().is_empty() {
            return Err("a key needs a German and an English text".into());
        }
        let d = self.ty.check(self.default.clone()).map_err(|e| format!("default: {e}"))?;
        if d != self.default {
            return Err("default is not in normal form".into());
        }
        Ok(())
    }

    pub fn describe(&self, value: Option<&Value>) -> Json {
        let shown = |v: &Value| if self.safety == Safety::Secret { Json::from("***") } else { v.to_json() };
        let applier = match &self.applier {
            Applier::Settingsd => Json::obj().with("by", "settingsd"),
            Applier::Service(s) => Json::obj().with("by", "service").with("service", s.as_str()),
            Applier::Kernel { config, sys } => Json::obj()
                .with("by", "kernel")
                .with("config", config.as_str())
                .with("sys", sys.as_ref().map(|s| format!("/sys/bitsos/{s}"))),
        };
        Json::obj()
            .with("key", self.name.as_str())
            .with("schema", self.ty.describe())
            .with("default", shown(&self.default))
            .with("value", value.map_or_else(|| shown(&self.default), shown))
            .with("changed", value.is_some())
            .with("text", Json::obj().with("de", self.de.as_str()).with("en", self.en.as_str()))
            .with("applier", applier)
            .with("restart", self.restart.name())
            .with("safety", self.safety.name())
            .with("owner", self.owner.as_str())
    }

    /// A definition from a registration request. Services may only name themselves as applier; kernel keys are
    /// settingsd's own (the kernel cannot register).
    pub fn from_json(j: &Json, owner: &str) -> Result<KeyDef, String> {
        let name = j.str("key").ok_or("key missing")?.to_string();
        let ty = Type::from_json(j.get("schema").ok_or("schema missing")?)?;
        let default = ty.parse_json(j.get("default").ok_or("default missing")?).map_err(|e| format!("default: {e}"))?;
        let text = j.get("text").ok_or("text missing")?;
        let restart = match j.str("restart") {
            None => Restart::None,
            Some(s) => Restart::parse(s).ok_or("restart: want none|service|reboot")?,
        };
        let safety = match j.str("safety") {
            None => Safety::Normal,
            Some(s) => Safety::parse(s).ok_or("safety: want normal|confirm|secret")?,
        };
        let def = KeyDef {
            name,
            ty,
            default,
            de: text.str("de").unwrap_or("").to_string(),
            en: text.str("en").unwrap_or("").to_string(),
            applier: Applier::Service(owner.to_string()),
            restart,
            safety,
            owner: owner.to_string(),
        };
        def.validate()?;
        Ok(def)
    }
}
