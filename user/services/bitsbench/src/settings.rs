//! The `leistung.*` settings, registered with settingsd by bitsbench as their owner (op `register`, as bitsjournal
//! does) and read before each look at the night. The nightly runs are on by default (DECISIONS 18.11.: everything
//! built is on); the switch is a `confirm` key: Georg's own yes on the page "Leistung" (settingsd takes `confirmed`
//! from the compositor only) turns them off and on again. Without settingsd the defaults hold.

use crate::rules::Settings;
use mcpreg::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SOCKET: &str = "/run/bitsos/settings.sock";
pub const OWNER: &str = "bitsbench";
pub const ON: &str = "leistung.nachts";
pub const FROM: &str = "leistung.von";
pub const TO: &str = "leistung.bis";

pub fn socket() -> String {
    std::env::var("SETTINGS_SOCKET").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| SOCKET.into())
}

fn text(de: &str, en: &str) -> Json {
    Json::obj(vec![("de", Json::str(de)), ("en", Json::str(en))])
}

fn hour(key: &str, default: u32, de: &str, en: &str) -> Json {
    Json::obj(vec![
        ("key", Json::str(key)),
        ("schema", Json::obj(vec![("type", Json::str("int")), ("min", Json::Num(0.0)), ("max", Json::Num(23.0)), ("unit", Json::str("Uhr"))])),
        ("default", Json::Num(default as f64)),
        ("text", text(de, en)),
    ])
}

pub fn register_request() -> String {
    let keys = vec![
        Json::obj(vec![
            ("key", Json::str(ON)),
            ("schema", Json::obj(vec![("type", Json::str("bool"))])),
            ("default", Json::Bool(true)),
            ("safety", Json::str("confirm")),
            (
                "text",
                text(
                    "Nachts messen, wie schnell BitsOS ist (am Netz, wenn niemand ihn nutzt; die Werte bleiben hier)",
                    "Measure at night how fast BitsOS is (on mains power, when nobody uses it; the values stay here)",
                ),
            ),
        ]),
        hour(FROM, 2, "Nachts messen ab", "Measure at night from"),
        hour(TO, 5, "Keine neue Messung mehr ab", "No new measurement from"),
    ];
    Json::obj(vec![("op", Json::str("register")), ("owner", Json::str(OWNER)), ("keys", Json::Arr(keys))]).to_string()
}

fn ask(sock: &str, req: &str) -> Result<Json, String> {
    let s = UnixStream::connect(sock).map_err(|e| format!("{sock}: {e}"))?;
    // B4: a registry that hangs never stops the runner.
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).take(1 << 20).read_line(&mut line).map_err(|e| e.to_string())?;
    Json::parse(line.trim()).map_err(|e| format!("settingsd answered {line:?}: {e}"))
}

fn get(sock: &str, key: &str) -> Result<Json, String> {
    ask(sock, &Json::obj(vec![("op", Json::str("get")), ("key", Json::str(key))]).to_string())
}

/// Applies one value to the settings (out-of-range hours are clamped; a wrong type keeps the old value).
pub fn set(s: &mut Settings, key: &str, v: &Json) {
    let h = |old: u32| v.as_i64().map_or(old, |n| n.clamp(0, 23) as u32);
    match key {
        ON => s.on = v.as_bool().unwrap_or(s.on),
        FROM => s.from_h = h(s.from_h),
        TO => s.to_h = h(s.to_h),
        _ => {}
    }
}

/// The current values, registered first when settingsd does not know them. The note says why defaults (or
/// part of them) are used.
pub fn load(sock: &str) -> (Settings, Option<String>) {
    let mut s = Settings::default();
    let first = match get(sock, ON) {
        Ok(j) if j.get("error").and_then(Json::as_str) == Some("unknown_key") => match ask(sock, &register_request()) {
            Ok(r) if r.get("ok").and_then(Json::as_bool) == Some(true) => get(sock, ON),
            Ok(r) => return (s, Some(format!("settingsd refused the leistung.* keys: {r}"))),
            Err(e) => return (s, Some(e)),
        },
        other => other,
    };
    let j = match first {
        Ok(j) => j,
        Err(e) => return (s, Some(e)),
    };
    let mut note = None;
    for key in [ON, FROM, TO] {
        let a = if key == ON { Ok(j.clone()) } else { get(sock, key) };
        match a {
            Ok(a) if a.get("ok").and_then(Json::as_bool) == Some(true) => {
                if let Some(v) = a.get("value") {
                    set(&mut s, key, v);
                }
            }
            Ok(a) => note = Some(format!("{key}: {a}")),
            Err(e) => note = Some(format!("{key}: {e}")),
        }
    }
    (s, note)
}
