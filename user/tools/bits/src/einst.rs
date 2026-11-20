//! `bits einstellungen liste|lesen|setzen`: settingsd's `list`, `get` and `set` with settingsd's own rules (types,
//! locked and confirm keys, secrets shown as `***`). From the assistant's tree a change is judged as the
//! assistant's own settings tool: refused at "Nur antworten", asked on the desktop at "Fragt vorher", and never
//! with `confirmed` (a key that asks first stays Georg's).

use std::time::Duration;

use serde_json::{Value, json};

use crate::frage;
use crate::out::{Done, Fail, R};
use crate::sock;

const WHO: &str = "Der Einstellungsdienst";
/// PLAN K2: the level's key and its default.
pub const LEVEL_KEY: &str = "ai.level";
pub const LEVEL_DEFAULT: &str = "ask";

pub fn run(args: &[String]) -> R {
    match args.first().map(String::as_str) {
        Some("liste") | None => liste(args.get(1).map(String::as_str).unwrap_or("")),
        Some("lesen") => lesen(args.get(1).ok_or_else(|| Fail::usage("Welche Einstellung? (einstellungen lesen <schlüssel>)"))?),
        Some("setzen") => setzen(&args[1..]),
        Some(v) => Err(Fail::usage(format!("„einstellungen {v}“ gibt es nicht; möglich: liste, lesen, setzen."))),
    }
}

fn call(req: Value) -> R<Value> {
    sock::ok_or_fail(sock::json_line(&sock::settings_path(), WHO, &req, Duration::from_secs(15), |_| {})?)
}

fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        o => o.to_string(),
    }
}

fn liste(prefix: &str) -> R {
    let v = call(json!({"op": "list", "prefix": prefix, "source": "bits"}))?;
    let keys = v.get("keys").cloned().unwrap_or(json!([]));
    let text = keys
        .as_array()
        .map(|a| a.iter().map(|k| format!("{}{} = {}", k["key"].as_str().unwrap_or(""), if k["changed"] == true { " *" } else { "" }, shown(&k["value"]))).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    Ok(Done::new(text).with("keys", keys))
}

fn lesen(key: &str) -> R {
    let v = call(json!({"op": "get", "key": key, "source": "bits"}))?;
    Ok(Done::new(shown(&v["value"])).with("key", key).with("value", v["value"].clone()).with("changed", v["changed"].clone()))
}

/// The level now (settingsd's `ai.level`; the plan's default while nobody registered it).
pub fn level() -> R<String> {
    match call(json!({"op": "get", "key": LEVEL_KEY, "source": "bits"})) {
        Ok(v) => Ok(v["value"].as_str().unwrap_or(LEVEL_DEFAULT).to_string()),
        Err(f) if f.code == "not_found" => Ok(LEVEL_DEFAULT.into()),
        Err(f) => Err(f),
    }
}

pub const ANSWER_ONLY: &str = "Auf der Stufe „Nur antworten“ ändert der Assistent nichts. Es wurde nichts getan.";
const NOT_YOURS: &str = "Eine Einstellung, die vorher fragt, ändert nur Georg selbst. Es wurde nichts getan.";

fn setzen(a: &[String]) -> R {
    let confirm = a.iter().any(|x| x == "--bestaetigt");
    let rest: Vec<&String> = a.iter().filter(|x| *x != "--bestaetigt").collect();
    let (Some(key), Some(_)) = (rest.first(), rest.get(1)) else {
        return Err(Fail::usage("einstellungen setzen <schlüssel> <wert> [--bestaetigt]"));
    };
    let value = rest[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
    let tree = bitstree::of_self().inside();
    if tree {
        if confirm {
            return Err(Fail::refused("Aus dem Assistenten heraus zählt --bestaetigt nicht: das sagt nur Georg selbst."));
        }
        match level()?.as_str() {
            "auto" => {}
            "answer" => return Err(Fail::refused(ANSWER_ONLY)),
            _ => frage::ask(&format!("bits-frage einstellung {key} {value}"))?,
        }
    }
    let r = call(json!({"op": "set", "key": key, "value": value, "confirmed": confirm && !tree, "source": if tree { "assistent" } else { "bits" }}));
    match r {
        Ok(v) => Ok(Done::new(format!("{key} = {}", shown(&v["new"]))).with("key", key.as_str()).with("old", v["old"].clone()).with("value", v["new"].clone())),
        Err(f) if tree && f.code == "refused" => Err(Fail::refused(format!("{NOT_YOURS} ({})", f.message))),
        Err(f) => Err(f),
    }
}
