//! The switch in settingsd (owner `atspid`): whether Durchblick reads foreign programs at all. In the shared
//! `desktop` namespace because `durchblick.*` is another program's (a namespace has one owner). Without
//! settingsd the default holds: on.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub const SOCKET: &str = "/run/bitsos/settings.sock";
pub const OWNER: &str = "atspid";
pub const KEY: &str = "desktop.durchblick.programs";

pub fn register_request() -> Value {
    json!({"op": "register", "owner": OWNER, "keys": [{
        "key": KEY,
        "schema": {"type": "bool"},
        "default": true,
        "text": {
            "de": "Durchblick liest auch die Knöpfe und Felder fremder Programme (Brave, LibreOffice, Steam), nur auf diesem Rechner; gilt für Programme, die danach starten",
            "en": "Durchblick also reads the buttons and fields of foreign programs (Brave, LibreOffice, Steam), on this machine only; applies to programs started afterwards"
        }
    }]})
}

fn ask(sock: &Path, req: &Value) -> Result<Value, String> {
    let s = UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(std::io::Read::take(s, 1 << 20)).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(line.trim()).map_err(|e| format!("settingsd answered {line:?}: {e}"))
}

/// The switch's value: Ok(None) when settingsd has no such key even after registering it.
fn value(sock: &Path) -> Result<Option<bool>, String> {
    let get = || ask(sock, &json!({"op": "get", "key": KEY}));
    let mut v = get()?;
    if v["ok"] != json!(true) {
        let r = ask(sock, &register_request())?;
        if r["ok"] != json!(true) {
            return Err(format!("settingsd refused {KEY}: {r}"));
        }
        v = get()?;
    }
    Ok(v["value"].as_bool())
}

/// Whether Durchblick may read foreign programs now. An unreachable settingsd counts as the default (on).
pub fn enabled(sock: &Path) -> bool {
    if !sock.exists() {
        return true;
    }
    match value(sock) {
        Ok(v) => v.unwrap_or(true),
        Err(e) => {
            eprintln!("atspid: settings: {e}; going by the default (on)");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_a_bool_in_the_shared_desktop_namespace_with_german_text() {
        let r = register_request();
        assert_eq!(r["owner"], "atspid");
        let k = &r["keys"][0];
        assert!(k["key"].as_str().unwrap().starts_with("desktop."));
        assert_eq!(k["schema"]["type"], "bool");
        assert_eq!(k["default"], true);
        assert!(k["text"]["de"].as_str().unwrap().contains("Durchblick"));
    }

    #[test]
    fn no_settingsd_means_on() {
        assert!(enabled(Path::new("/nonexistent/settings.sock")));
    }
}
