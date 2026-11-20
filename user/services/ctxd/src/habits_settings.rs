//! DB4: the switches of the one memory in settingsd, `gewohnheiten.enabled` and `gewohnheiten.<part>` (all on by
//! default, all local), registered by ctxd as their owner beside `context.*`. Unlike those, a change needs no
//! restart: a thread of its own looks every few seconds and applies it at once.

use crate::engine::Engine;
use crate::habits::PARTS;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const PREFIX: &str = "gewohnheiten";
const LOOK: Duration = Duration::from_secs(3);
static SOCKET: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// (part, German text of its switch).
fn texts() -> Vec<(String, String)> {
    let mut v = vec![("enabled".to_string(), "BitsOS merkt sich, wie du es benutzt, und macht Kleinigkeiten nach (alles bleibt auf diesem Rechner)".to_string())];
    for (p, n) in PARTS {
        v.push((p.to_string(), format!("Lernen: {n}")));
    }
    v
}

pub fn register_request() -> Value {
    let keys: Vec<Value> = texts()
        .into_iter()
        .map(|(p, de)| json!({"key": format!("{PREFIX}.{p}"), "schema": {"type": "bool"}, "default": true, "text": {"de": de, "en": format!("Learn: {p}")}}))
        .collect();
    json!({"op": "register", "owner": crate::settings::OWNER, "keys": keys})
}

/// (part, on) as settingsd holds them; registers the keys first when some are missing.
pub fn fetch(sock: &Path) -> Result<Vec<(String, bool)>, String> {
    let list = |s: &Path| crate::settings::ask(s, &json!({"op": "list", "prefix": PREFIX}));
    let mut v = list(sock)?;
    if v["keys"].as_array().map_or(0, Vec::len) < PARTS.len() + 1 {
        let r = crate::settings::ask(sock, &register_request())?;
        if r["ok"] != json!(true) {
            return Err(format!("settingsd refused the {PREFIX}.* keys: {r}"));
        }
        v = list(sock)?;
    }
    Ok(v["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| Some((k["key"].as_str()?.strip_prefix(&format!("{PREFIX}."))?.to_string(), k["value"].as_bool()?)))
        .collect())
}

/// The watcher of the system service: the switches follow settingsd within `LOOK`.
pub fn watch(e: Arc<Engine>, sock: PathBuf) {
    if let Ok(mut s) = SOCKET.lock() {
        *s = Some(sock.clone());
    }
    let mut said = false;
    loop {
        match fetch(&sock) {
            Ok(v) => {
                said = false;
                for (p, on) in v {
                    if e.habits.switch(&p) != on {
                        crate::event!("info", "habits.switch", part = p.clone(), on = on);
                    }
                    crate::habits_ops::apply_switch(&e, &p, on);
                }
            }
            Err(x) if !said => {
                crate::event!("warn", "habits.settings_unavailable", reason = x);
                said = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(LOOK);
    }
}

/// `learned_switch`: kept in settingsd when the service watches it; false when only this run knows it.
pub fn store(part: &str, on: bool) -> bool {
    let Some(sock) = SOCKET.lock().ok().and_then(|s| s.clone()) else { return false };
    crate::settings::ask(&sock, &json!({"op": "set", "key": format!("{PREFIX}.{part}"), "value": on, "source": "ctxd"}))
        .is_ok_and(|r| r["ok"] == json!(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_switch_per_part_and_the_master_all_on_in_plain_german() {
        let r = register_request();
        let keys = r["keys"].as_array().unwrap();
        assert_eq!(keys.len(), PARTS.len() + 1);
        assert!(keys.iter().all(|k| k["default"] == json!(true) && k["key"].as_str().unwrap().starts_with("gewohnheiten.")));
        for k in keys {
            let de = k["text"]["de"].as_str().unwrap();
            assert!(!de.contains('_') && !de.contains("ctxd"), "{de}");
        }
    }
}
