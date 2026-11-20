//! The `access.*` keys (registered with settingsd by bitsaccess as their owner, read every 10 s; without
//! settingsd the defaults hold) and ctxd's `context.private_roots`, which the page marks.

use settingsd::json::{self, Json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SOCKET: &str = "/run/bitsos/settings.sock";

/// settingsd's socket: `SETTINGS_SOCKET` (host scenarios), else the system's.
pub fn socket() -> String {
    std::env::var("SETTINGS_SOCKET").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| SOCKET.into())
}
pub const OWNER: &str = "bitsaccess";
pub const PRIVATE_ROOTS: &str = "context.private_roots";

/// (key, min, max, default, German, English).
const INTS: &[(&str, i64, i64, i64, &str, &str)] = &[
    ("access.keep_days", 1, 365, 30, "So viele Tage hebt „Wer hat was gemacht“ auf", "Days \"Wer hat was gemacht\" keeps"),
    ("access.max_mb", 4, 4096, 64, "So viele MB darf „Wer hat was gemacht“ auf /data höchstens belegen (die ältesten Tage gehen zuerst)", "Most MB the access log may take on /data (the oldest days go first)"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub keep_days: u64,
    pub max_mb: u64,
    /// Programs left out: paths or file names.
    pub ignore: Vec<String>,
    pub private_roots: Vec<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { enabled: true, keep_days: 30, max_mb: 64, ignore: Vec::new(), private_roots: Vec::new() }
    }
}

pub fn split_list(t: &str) -> Vec<String> {
    t.split(';').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

impl Settings {
    pub fn max_bytes(&self) -> u64 {
        self.max_mb << 20
    }

    /// Whether a program is left out (`access.ignore` names its path or its file name).
    pub fn ignored(&self, exe: &str) -> bool {
        self.ignore.iter().any(|i| i == exe || i == crate::model::program_name(exe))
    }

    pub fn set(&mut self, key: &str, v: &Json) {
        let range = INTS.iter().find(|k| k.0 == key).map_or((0, i64::MAX), |k| (k.1, k.2));
        let n = || v.as_i64().map(|n| n.clamp(range.0, range.1) as u64);
        match key {
            "access.enabled" => self.enabled = v.as_bool().unwrap_or(self.enabled),
            "access.keep_days" => self.keep_days = n().unwrap_or(self.keep_days),
            "access.max_mb" => self.max_mb = n().unwrap_or(self.max_mb),
            "access.ignore" => self.ignore = v.as_str().map(split_list).unwrap_or_default(),
            PRIVATE_ROOTS => self.private_roots = v.as_str().map(split_list).unwrap_or_default(),
            _ => {}
        }
    }
}

pub fn register_request() -> String {
    let text = |de: &str, en: &str| Json::obj().with("de", de).with("en", en);
    let mut keys = vec![
        Json::obj()
            .with("key", "access.enabled")
            .with("schema", Json::obj().with("type", "bool"))
            .with("default", true)
            .with("text", text("BitsOS merkt sich, welches Programm welche Ordner, Geräte und Netzadressen benutzt hat (bleibt auf diesem Rechner)", "BitsOS notes which program used which folders, devices and network addresses (stays on this machine)")),
    ];
    for (k, min, max, d, de, en) in INTS {
        keys.push(Json::obj().with("key", *k).with("schema", Json::obj().with("type", "int").with("min", *min).with("max", *max)).with("default", *d).with("text", text(de, en)));
    }
    keys.push(
        Json::obj()
            .with("key", "access.ignore")
            .with("schema", Json::obj().with("type", "text").with("max_bytes", 4096i64))
            .with("default", "")
            .with("text", text("Programme, die „Wer hat was gemacht“ nicht aufschreibt (Name oder Pfad, mehrere mit ; trennen)", "Programs the access log leaves out (name or path, separate several with ;)")),
    );
    Json::obj().with("op", "register").with("owner", OWNER).with("keys", keys).to_text()
}

fn ask(sock: &str, req: &str) -> Result<Json, String> {
    let s = UnixStream::connect(sock).map_err(|e| format!("{sock}: {e}"))?;
    // A registry that hangs never stops the log.
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    json::parse(line.trim()).map_err(|e| format!("settingsd answered {line:?}: {e}"))
}

/// The current values (registered first when settingsd does not know them). The note says why defaults (or what
/// was read) are used.
pub fn load(sock: &str) -> (Settings, Option<String>) {
    let mut s = Settings::default();
    let get = |key: &str| ask(sock, &Json::obj().with("op", "get").with("key", key).to_text());
    let first = match get("access.enabled") {
        Ok(j) if j.str("error") == Some("unknown_key") => match ask(sock, &register_request()) {
            Ok(r) if r.get("ok").and_then(Json::as_bool) == Some(true) => get("access.enabled"),
            Ok(r) => return (s, Some(format!("settingsd refused the access.* keys: {}", r.to_text()))),
            Err(e) => return (s, Some(e)),
        },
        other => other,
    };
    let j = match first {
        Ok(j) => j,
        Err(e) => return (s, Some(e)),
    };
    let mut note = None;
    let keys = std::iter::once("access.enabled").chain(INTS.iter().map(|k| k.0)).chain(["access.ignore", PRIVATE_ROOTS]);
    for key in keys {
        let a = if key == "access.enabled" { Ok(j.clone()) } else { get(key) };
        match a {
            Ok(a) if a.get("ok").and_then(Json::as_bool) == Some(true) => {
                if let Some(v) = a.get("value") {
                    s.set(key, v);
                }
            }
            // ctxd's key is missing while ctxd never ran: no private folders known yet.
            Ok(_) if key == PRIVATE_ROOTS => {}
            Ok(a) => note = Some(format!("{key}: {}", a.to_text())),
            Err(e) => note = Some(format!("{key}: {e}")),
        }
    }
    (s, note)
}
