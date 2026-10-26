//! The `snap.*` settings (B1, V2-PLAN §1a "all of it settings"). bitssnap registers them with settingsd as their
//! owner (op `register`, CL-1) and reads them before every round; without settingsd (a test boot, the registry
//! restarting) the defaults hold, so the snapshots never stop because the registry is away.

use crate::retention::{MONTH_S, Policy};
use settingsd::json::{self, Json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SOCKET: &str = "/run/bitsos/settings.sock";
pub const OWNER: &str = "bitssnap";

/// Free space (percent) the automatic snapshots leave (VS-6b): 25, so a pool of automatic snapshots alone settles
/// below 75 % full, where the space warning (above 80 %, quiet below 75 %, space.rs) is off. With the old 15 it
/// settled at 85 % and the warning never went away; only manual and before-restore snapshots should raise it.
pub const MIN_FREE_PCT: i64 = 25;
/// The default before VS-6b; a value stored as 15 is moved to the new default once (`migrate`).
const OLD_MIN_FREE_PCT: i64 = 15;
/// The `source` of that one `set` in settingsd's journal (and the mark that it happened).
pub const MIGRATION_SOURCE: &str = "bitssnap-migration";

/// (key, min, max, default, German, English); `snap.enabled` is the one bool.
const INTS: &[(&str, i64, i64, i64, &str, &str)] = &[
    ("snap.interval_min", 1, 1440, 5, "Alle wie viele Minuten /data gesichert wird", "Minutes between snapshots of /data"),
    ("snap.keep_all_hours", 1, 720, 24, "So viele Stunden bleibt jede Sicherung", "Hours in which every snapshot is kept"),
    ("snap.hourly_days", 0, 365, 7, "So viele Tage bleibt eine Sicherung pro Stunde", "Days in which one snapshot per hour is kept"),
    ("snap.daily_days", 0, 3650, 30, "So viele Tage bleibt eine Sicherung pro Tag", "Days in which one snapshot per day is kept"),
    ("snap.weekly_months", 0, 120, 6, "So viele Monate bleibt eine Sicherung pro Woche", "Months in which one snapshot per week is kept"),
    ("snap.min_free_pct", 0, 90, MIN_FREE_PCT, "Unter so viel Prozent freiem Platz gehen die ältesten zuerst, bis 5 Prozent mehr frei sind", "Below this much free space (percent) the oldest go first, until 5 percent more is free"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub interval_min: u64,
    pub keep_all_hours: u64,
    pub hourly_days: u64,
    pub daily_days: u64,
    pub weekly_months: u64,
    pub min_free_pct: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { enabled: true, interval_min: 5, keep_all_hours: 24, hourly_days: 7, daily_days: 30, weekly_months: 6, min_free_pct: MIN_FREE_PCT.unsigned_abs() }
    }
}

impl Settings {
    pub fn policy(&self, utc_offset_s: i64) -> Policy {
        Policy {
            keep_all_s: self.keep_all_hours * 3600,
            hourly_s: self.hourly_days * 86400,
            daily_s: self.daily_days * 86400,
            weekly_s: self.weekly_months * MONTH_S,
            min_free_permille: self.min_free_pct * 10,
            utc_offset_s,
        }
    }

    fn set(&mut self, key: &str, v: &Json) {
        // The schema's range (`INTS`) holds here too: the registry is another program, and an out-of-range
        // value must not overflow the seconds computed from it (VS-2b).
        let range = INTS.iter().find(|k| k.0 == key).map_or((0, i64::MAX), |k| (k.1, k.2));
        let n = || v.as_i64().map(|n| n.clamp(range.0, range.1) as u64);
        match key {
            "snap.enabled" => self.enabled = v.as_bool().unwrap_or(self.enabled),
            "snap.interval_min" => self.interval_min = n().unwrap_or(self.interval_min).max(1),
            "snap.keep_all_hours" => self.keep_all_hours = n().unwrap_or(self.keep_all_hours),
            "snap.hourly_days" => self.hourly_days = n().unwrap_or(self.hourly_days),
            "snap.daily_days" => self.daily_days = n().unwrap_or(self.daily_days),
            "snap.weekly_months" => self.weekly_months = n().unwrap_or(self.weekly_months),
            "snap.min_free_pct" => self.min_free_pct = n().unwrap_or(self.min_free_pct),
            _ => {}
        }
    }
}

/// The registration request (the keys with their schema, default and texts).
pub fn register_request() -> String {
    let text = |de: &str, en: &str| Json::obj().with("de", de).with("en", en);
    let mut keys = vec![
        Json::obj()
            .with("key", "snap.enabled")
            .with("schema", Json::obj().with("type", "bool"))
            .with("default", true)
            .with("text", text("Sicherungen von /data alle paar Minuten", "Snapshots of /data every few minutes")),
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
    // B4: a registry that hangs never stops the snapshots.
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    json::parse(line.trim()).map_err(|e| format!("settingsd answered {line:?}: {e}"))
}

/// The current values: registered first when settingsd does not know them yet. Err only says why the
/// defaults (or what was read) are used; the settings returned are always usable.
pub fn load(sock: &str) -> (Settings, Option<String>) {
    let mut s = Settings::default();
    let get = |key: &str| ask(sock, &Json::obj().with("op", "get").with("key", key).to_text());
    let first = match get("snap.enabled") {
        Ok(j) if j.str("error") == Some("unknown_key") => match ask(sock, &register_request()) {
            Ok(r) if r.get("ok").and_then(Json::as_bool) == Some(true) => get("snap.enabled"),
            Ok(r) => return (s, Some(format!("settingsd refused the snap.* keys: {}", r.to_text()))),
            Err(e) => return (s, Some(e)),
        },
        other => other,
    };
    let j = match first {
        Ok(j) => j,
        Err(e) => return (s, Some(e)),
    };
    let mut note = None;
    for key in std::iter::once("snap.enabled").chain(INTS.iter().map(|k| k.0)) {
        let a = if key == "snap.enabled" { Ok(j.clone()) } else { get(key) };
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

/// Whether the journal of `snap.min_free_pct` (newest first, as `history` answers) forbids the move to the new
/// default: it happened already (our `source`, also when undone since), or someone chose 15 after the default
/// became 25 (a `set` or `undo` whose new value is a stored 15, not the default). A stored 15 from before VS-6b
/// has no such entry: a `set` to the then default stored nothing, so only a hand-edited file (`edit`) held it.
pub fn migration_blocked(history: &[Json]) -> bool {
    history.iter().any(|e| {
        e.str("source") == Some(MIGRATION_SOURCE)
            || (e.get("new").and_then(Json::as_i64) == Some(OLD_MIN_FREE_PCT)
                && e.get("new_default").and_then(Json::as_bool) != Some(true)
                && !matches!(e.str("op"), Some("edit" | "resync")))
    })
}

/// VS-6b: a `snap.min_free_pct` stored as 15 (the old default) becomes the new default, once, journaled (Georg
/// can `undo` it). Ok(Some(line)) when it moved; Ok(None) when there is nothing to do; Err when settingsd did not
/// answer as expected (the next wake-up tries again).
pub fn migrate(sock: &str) -> Result<Option<String>, String> {
    let key = "snap.min_free_pct";
    let ok = |j: Json| if j.get("ok").and_then(Json::as_bool) == Some(true) { Ok(j) } else { Err(format!("{key}: {}", j.to_text())) };
    let cur = ok(ask(sock, &Json::obj().with("op", "get").with("key", key).to_text())?)?;
    let stored = cur.get("changed").and_then(Json::as_bool) == Some(true);
    if !stored || cur.get("value").and_then(Json::as_i64) != Some(OLD_MIN_FREE_PCT) {
        return Ok(None);
    }
    // The whole journal of the key (R1: no fixed count); settingsd keeps `settings.history_keep` entries.
    let h = ok(ask(sock, &Json::obj().with("op", "history").with("prefix", key).with("n", i64::MAX).to_text())?)?;
    let Some(Json::Arr(es)) = h.get("entries") else { return Err(format!("{key}: history without entries: {}", h.to_text())) };
    if migration_blocked(es) {
        return Ok(None);
    }
    let set = Json::obj().with("op", "set").with("key", key).with("value", MIN_FREE_PCT).with("source", MIGRATION_SOURCE);
    let r = ok(ask(sock, &set.to_text())?)?;
    let seq = r.get("seq").and_then(Json::as_i64).map_or_else(String::new, |s| format!(", journal entry {s}"));
    Ok(Some(format!(
        "settings: snap.min_free_pct {OLD_MIN_FREE_PCT} -> {MIN_FREE_PCT} (the new default: automatic snapshots alone now stay below 75 % full{seq}; settingsd undo takes it back)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use settingsd::events::Events;
    use settingsd::registry::Registry;
    use settingsd::server::{self, Shared};
    use settingsd::store::Paths;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    /// A settingsd of its own (in this process, on its own socket) with `snap_toml` as the group file on the
    /// stick, the way an older settingsd or a hand edit left it.
    fn settingsd(name: &str, snap_toml: Option<&str>) -> String {
        let root = std::env::temp_dir().join(format!("bitssnap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("stick/settings")).unwrap();
        std::fs::create_dir_all(root.join("sys")).unwrap();
        if let Some(t) = snap_toml {
            std::fs::write(root.join("stick/settings/snap.toml"), t).unwrap();
        }
        let paths = Paths { stick: root.join("stick"), fallback: root.join("fb"), sys: root.join("sys"), socket: root.join("s.sock") };
        let reg = Registry::open(paths.clone(), Events::capture().0, Duration::from_millis(500));
        let l = UnixListener::bind(&paths.socket).unwrap();
        std::thread::spawn(move || server::serve(l, Arc::new(Shared::new(reg))));
        paths.socket.to_string_lossy().into_owned()
    }

    fn req(sock: &str, j: Json) -> Json {
        let a = ask(sock, &j.to_text()).unwrap();
        assert_eq!(a.get("ok").and_then(Json::as_bool), Some(true), "{}", a.to_text());
        a
    }

    #[test]
    fn a_stored_old_default_moves_once() {
        let sock = settingsd("vs6b-once", Some("snap.min_free_pct = 15\n"));
        let (s, why) = load(&sock);
        assert_eq!((s.min_free_pct, why), (15, None), "the stored 15 survives the new default by itself");
        let line = migrate(&sock).unwrap().expect("moved");
        assert!(line.starts_with("settings: snap.min_free_pct 15 -> 25 (the new default") && line.contains("journal entry"), "{line}");
        assert_eq!(load(&sock).0.min_free_pct, 25);
        let g = req(&sock, Json::obj().with("op", "get").with("key", "snap.min_free_pct"));
        assert_eq!(g.get("changed").and_then(Json::as_bool), Some(false), "25 is the default: nothing stored");
        assert_eq!(migrate(&sock).unwrap(), None, "quiet afterwards");
        // Georg chooses 15 on purpose: it stays, at every later wake-up and every restart of bitssnap.
        req(&sock, Json::obj().with("op", "set").with("key", "snap.min_free_pct").with("value", 15i64).with("source", "mcp"));
        for _ in 0..3 {
            assert_eq!(migrate(&sock).unwrap(), None);
        }
        assert_eq!(load(&sock).0.min_free_pct, 15);
        let h = req(&sock, Json::obj().with("op", "history").with("prefix", "snap.min_free_pct").with("n", 9i64));
        let Some(Json::Arr(es)) = h.get("entries") else { panic!() };
        assert_eq!(es.iter().filter(|e| e.str("source") == Some(MIGRATION_SOURCE)).count(), 1, "one journaled move: {}", h.to_text());
    }

    #[test]
    fn an_undone_move_stays_undone() {
        let sock = settingsd("vs6b-undo", Some("snap.min_free_pct = 15\n"));
        load(&sock);
        assert!(migrate(&sock).unwrap().is_some());
        req(&sock, Json::obj().with("op", "undo").with("source", "mcp"));
        assert_eq!(load(&sock).0.min_free_pct, 15);
        assert_eq!(migrate(&sock).unwrap(), None, "Georg took it back: not again");
    }

    #[test]
    fn nothing_stored_or_another_value_is_left_alone() {
        let sock = settingsd("vs6b-none", None);
        assert_eq!(load(&sock).0.min_free_pct, 25, "registered with the new default");
        assert_eq!(migrate(&sock).unwrap(), None);
        let sock = settingsd("vs6b-30", Some("snap.min_free_pct = 30\n"));
        load(&sock);
        assert_eq!(migrate(&sock).unwrap(), None);
        assert_eq!(load(&sock).0.min_free_pct, 30);
        assert!(migrate("/nonexistent/settings.sock").is_err(), "settingsd away: tried again later");
    }

    #[test]
    fn journal_marks() {
        let e = |src: &str, op: &str, new: Json| Json::obj().with("source", src).with("op", op).with("new", new);
        assert!(!migration_blocked(&[]));
        assert!(migration_blocked(&[e(MIGRATION_SOURCE, "set", Json::Null)]));
        assert!(migration_blocked(&[e("mcp", "set", Json::Int(15))]), "chosen on purpose");
        assert!(migration_blocked(&[e("mcp", "undo", Json::Int(15))]), "a move taken back");
        assert!(!migration_blocked(&[e("file", "edit", Json::Int(15))]), "the old file is what moves");
        assert!(!migration_blocked(&[e("mcp", "set", Json::Int(15)).with("new_default", true)]), "a default shown as its value");
        assert!(!migration_blocked(&[e("mcp", "set", Json::Int(30))]));
    }

    #[test]
    fn registration_parses_as_settingsd_reads_it() {
        let j = json::parse(&register_request()).unwrap();
        let Some(Json::Arr(keys)) = j.get("keys") else { panic!("no keys") };
        assert_eq!(keys.len(), 7);
        for k in keys {
            let d = settingsd::key::KeyDef::from_json(k, OWNER).unwrap_or_else(|e| panic!("{e}: {}", k.to_text()));
            assert!(d.name.starts_with("snap."));
        }
    }

    #[test]
    fn values_map_onto_the_policy() {
        let mut s = Settings::default();
        s.set("snap.weekly_months", &Json::Int(2));
        s.set("snap.interval_min", &Json::Int(0));
        s.set("snap.enabled", &Json::Bool(false));
        s.set("snap.min_free_pct", &Json::Str("x".into()));
        assert_eq!((s.weekly_months, s.interval_min, s.enabled, s.min_free_pct), (2, 1, false, 25));
        s.set("snap.interval_min", &Json::Int(i64::MAX));
        s.set("snap.daily_days", &Json::Int(-5));
        assert_eq!((s.interval_min, s.daily_days), (1440, 0), "values outside the schema are clamped");
        let p = s.policy(3600);
        assert_eq!((p.keep_all_s, p.weekly_s, p.min_free_permille, p.utc_offset_s), (86400, 2 * MONTH_S, 250, 3600));
    }

    /// The edges of `snap.min_free_pct` through the whole chain (registry value -> policy -> space rule): every
    /// round ends, keeps the newest automatic one and deletes each snapshot at most once.
    #[test]
    fn min_free_edges_end_every_space_round() {
        use crate::retention::{self, Freed, Item};
        let its: Vec<Item> = (1..=20).map(|id| Item { id, created_s: id * 60, auto: true }).collect();
        // `frees` per mille per delete; returns (removed, free after, delete calls).
        let run = |min_free: u64, free: u64, dry: bool, frees: u64, busy: bool| {
            let (mut f, mut calls) = (free, 0);
            let (removed, after) = retention::prune_for_space(&its, &mut Vec::new(), min_free, free, dry, |_| {
                calls += 1;
                f += frees;
                Ok::<_, ()>(if busy { Freed::Busy } else { Freed::Now(f) })
            })
            .unwrap();
            (removed, after, calls)
        };
        for pct in [90, 1000, i64::MAX] {
            let mut s = Settings::default();
            s.set("snap.min_free_pct", &Json::Int(pct));
            let m = s.policy(0).min_free_permille;
            assert_eq!(m, 900, "{pct} is clamped to the schema's 90");
            // Target 950, never reached at 7 per delete: all but the newest, once each.
            let (removed, after, calls) = run(m, 100, false, 7, false);
            assert_eq!((removed, after, calls), ((1..=19).collect(), 100 + 19 * 7, 19));
        }
        // Past the schema (a hand-built policy): target capped at 1000, a delete that frees nothing still ends.
        let (removed, after, calls) = run(1000, 999, false, 0, false);
        assert_eq!((removed.len(), after, calls), (19, 999, 19));
        // Every candidate busy: each tried once, nothing removed.
        let (removed, after, calls) = run(1000, 10, false, 0, true);
        assert_eq!((removed, after, calls), (vec![], 10, 19));
        // Dry run with the target above the pool: one name, no delete.
        assert_eq!(run(u64::MAX, 0, true, 7, false), (vec![1], 0, 0));
        // The migrated default and the old one: both stop at their own band.
        for (pct, start, n) in [(25, 249, 8), (15, 149, 8)] {
            let mut s = Settings::default();
            s.set("snap.min_free_pct", &Json::Int(pct));
            let (removed, after, _) = run(s.policy(0).min_free_permille, start, false, 7, false);
            assert_eq!((removed.len(), after), (n, start + n as u64 * 7));
        }
    }

    #[test]
    fn no_registry_means_defaults() {
        let (s, why) = load("/nonexistent/settings.sock");
        assert_eq!(s, Settings::default());
        assert!(why.is_some());
    }
}
