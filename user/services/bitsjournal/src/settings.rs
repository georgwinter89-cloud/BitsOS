//! The `journal.*` settings (V2-PLAN §1a "all of it settings"), registered with settingsd by bitsjournal as
//! their owner (op `register`, CL-1) and read every minute; without settingsd the defaults hold, so the journal
//! never stops because the registry is away (bitssnap's pattern, user/tools/bitssnap/src/settings.rs).

use settingsd::json::{self, Json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SOCKET: &str = "/run/bitsos/settings.sock";
pub const OWNER: &str = "bitsjournal";

/// (key, min, max, default, German, English); `journal.enabled` is the one bool.
const INTS: &[(&str, i64, i64, i64, &str, &str)] = &[
    ("journal.keep_days", 1, 3650, 60, "So viele Tage bleiben Ereignisse und Messwerte im Journal", "Days the journal keeps events and metrics"),
    ("journal.max_mb", 8, 1_048_576, 256, "So viele MB darf das Journal auf /data höchstens belegen (die ältesten gehen zuerst)", "Most MB the journal may take on /data (the oldest go first)"),
    ("journal.metrics_s", 2, 3600, 60, "Messwerte im Journal: ein Mittelwert alle so viele Sekunden", "Metrics in the journal: one mean every this many seconds"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub keep_days: u64,
    pub max_mb: u64,
    pub metrics_s: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { enabled: true, keep_days: 60, max_mb: 256, metrics_s: 60 }
    }
}

impl Settings {
    pub fn keep_s(&self) -> u64 {
        self.keep_days * 86_400
    }
    pub fn max_bytes(&self) -> u64 {
        self.max_mb << 20
    }
    /// Segment size: an eighth of the budget (so retention by size frees in small steps), 64 KiB to 8 MiB.
    pub fn seg_max(&self) -> u64 {
        (self.max_bytes() / 8).clamp(64 << 10, 8 << 20)
    }

    pub fn set(&mut self, key: &str, v: &Json) {
        let range = INTS.iter().find(|k| k.0 == key).map_or((0, i64::MAX), |k| (k.1, k.2));
        let n = || v.as_i64().map(|n| n.clamp(range.0, range.1) as u64);
        match key {
            "journal.enabled" => self.enabled = v.as_bool().unwrap_or(self.enabled),
            "journal.keep_days" => self.keep_days = n().unwrap_or(self.keep_days),
            "journal.max_mb" => self.max_mb = n().unwrap_or(self.max_mb),
            "journal.metrics_s" => self.metrics_s = n().unwrap_or(self.metrics_s),
            _ => {}
        }
    }
}

pub fn register_request() -> String {
    let text = |de: &str, en: &str| Json::obj().with("de", de).with("en", en);
    let mut keys = vec![
        Json::obj()
            .with("key", "journal.enabled")
            .with("schema", Json::obj().with("type", "bool"))
            .with("default", true)
            .with("text", text("Das Journal hebt Ereignisse und Messwerte auf /data auf (Wochen zurück)", "The journal keeps events and metrics on /data (weeks back)")),
    ];
    for (k, min, max, d, de, en) in INTS {
        keys.push(
            Json::obj()
                .with("key", *k)
                .with("schema", Json::obj().with("type", "int").with("min", *min).with("max", *max))
                .with("default", *d)
                .with("text", text(de, en)),
        );
    }
    Json::obj().with("op", "register").with("owner", OWNER).with("keys", keys).to_text()
}

fn ask(sock: &str, req: &str) -> Result<Json, String> {
    let s = UnixStream::connect(sock).map_err(|e| format!("{sock}: {e}"))?;
    // B4: a registry that hangs never stops the journal.
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    json::parse(line.trim()).map_err(|e| format!("settingsd answered {line:?}: {e}"))
}

/// The current values (registered first when settingsd does not know them). Err only says why the defaults
/// (or what was read) are used.
pub fn load(sock: &str) -> (Settings, Option<String>) {
    let mut s = Settings::default();
    let get = |key: &str| ask(sock, &Json::obj().with("op", "get").with("key", key).to_text());
    let first = match get("journal.enabled") {
        Ok(j) if j.str("error") == Some("unknown_key") => match ask(sock, &register_request()) {
            Ok(r) if r.get("ok").and_then(Json::as_bool) == Some(true) => get("journal.enabled"),
            Ok(r) => return (s, Some(format!("settingsd refused the journal.* keys: {}", r.to_text()))),
            Err(e) => return (s, Some(e)),
        },
        other => other,
    };
    let j = match first {
        Ok(j) => j,
        Err(e) => return (s, Some(e)),
    };
    let mut note = None;
    for key in std::iter::once("journal.enabled").chain(INTS.iter().map(|k| k.0)) {
        let a = if key == "journal.enabled" { Ok(j.clone()) } else { get(key) };
        match a {
            Ok(a) if a.get("ok").and_then(Json::as_bool) == Some(true) => {
                if let Some(v) = a.get("value") {
                    s.set(key, v);
                }
            }
            Ok(a) => note = Some(format!("{key}: {}", a.to_text())),
            Err(e) => note = Some(format!("{key}: {e}")),
        }
    }
    (s, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_parses_as_settingsd_reads_it() {
        let j = json::parse(&register_request()).unwrap();
        let Some(Json::Arr(keys)) = j.get("keys") else { panic!("no keys") };
        assert_eq!(keys.len(), 4);
        for k in keys {
            let d = settingsd::key::KeyDef::from_json(k, OWNER).unwrap_or_else(|e| panic!("{e}: {}", k.to_text()));
            assert!(d.name.starts_with("journal."));
        }
    }

    #[test]
    fn values_are_clamped_and_defaults_hold() {
        let mut s = Settings::default();
        assert_eq!((s.keep_s(), s.max_bytes(), s.seg_max()), (60 * 86_400, 256 << 20, 8 << 20));
        s.set("journal.max_mb", &Json::Int(1));
        s.set("journal.keep_days", &Json::Int(i64::MAX));
        s.set("journal.enabled", &Json::Bool(false));
        s.set("journal.metrics_s", &Json::Str("x".into()));
        assert_eq!((s.max_mb, s.keep_days, s.enabled, s.metrics_s), (8, 3650, false, 60));
        assert_eq!(s.seg_max(), 1 << 20);
        let (d, why) = load("/nonexistent/settings.sock");
        assert_eq!(d, Settings::default());
        assert!(why.is_some());
    }
}
