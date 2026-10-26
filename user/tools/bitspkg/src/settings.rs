//! PK-2: a package's settings keys in settingsd (CL-1). Install registers the current version's keys; removal
//! registers the keys of every version still on disk once more (settingsd only forgets values of keys it knows,
//! and after a reboot it knows none of a package's keys until they are registered: their values wait as
//! orphans, cl-CL-1 notes) and then unregisters `app.<name>.` with `forget`, which also drops the values, the
//! file lines and the journal entries. One JSON line per request on settingsd's socket (settingsd/src/proto.rs).

use crate::hooks::Hook;
use crate::manifest::{Manifest, Setting};
use settingsd::json::{self, Json};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// The owner of every package key. settingsd hands a namespace (`app`) to its first registrant, so per-package
/// owners would lock every second package out (change request to CL: groups `app.<name>`).
pub const OWNER: &str = "bitspkg";
pub const DEFAULT_SOCKET: &str = "/run/bitsos/settings.sock";
/// A CLI tool of class Normal: every answer is waited for at most this long (B4: no unbounded wait).
const TIMEOUT: Duration = Duration::from_secs(10);
const ANSWER_MAX: u64 = 1 << 20;
const SUN_PATH: usize = 108;

pub struct Settings {
    pub socket: PathBuf,
}

/// settingsd's answer: ok, or its error code and message.
pub type Answer = Result<Json, (String, String)>;

impl Settings {
    /// One request, one answer line. Err = settingsd not reachable or no sane answer.
    pub fn request(&self, req: &Json) -> Result<Answer, String> {
        // sun_path holds 108 bytes with its NUL (linux-6.6/net/unix/af_unix.c:292 unix_mkname_bsd): say so
        // instead of a bare "not reachable".
        if self.socket.as_os_str().len() >= SUN_PATH {
            return Err(format!("settingsd socket path {} is longer than a Unix socket address holds ({} bytes)", self.socket.display(), SUN_PATH - 1));
        }
        let s = UnixStream::connect(&self.socket).map_err(|e| format!("settingsd not reachable at {}: {e}", self.socket.display()))?;
        s.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
        s.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
        let mut w = s.try_clone().map_err(|e| e.to_string())?;
        writeln!(w, "{}", req.to_text()).map_err(|e| format!("settingsd: {e}"))?;
        let mut line = String::new();
        BufReader::new(s.take(ANSWER_MAX)).read_line(&mut line).map_err(|e| format!("settingsd: {e}"))?;
        let j = json::parse(line.trim()).map_err(|e| format!("settingsd answered {:?}: {e}", line.trim()))?;
        if j.get("ok").and_then(Json::as_bool) == Some(true) {
            return Ok(Ok(j));
        }
        let code = j.str("error").unwrap_or("?").to_string();
        Ok(Err((code, j.str("message").unwrap_or("").to_string())))
    }

    pub fn reachable(&self) -> bool {
        self.status().is_ok()
    }

    fn status(&self) -> Result<(), String> {
        self.request(&Json::obj().with("op", "status"))?.map(|_| ()).map_err(|(c, m)| format!("settingsd answered {c}: {m}"))
    }

    pub fn register(&self, settings: &[Setting]) -> Result<(), String> {
        if settings.is_empty() {
            return Ok(());
        }
        let keys = settings.iter().map(key_json).collect();
        let req = Json::obj().with("op", "register").with("owner", OWNER).with("keys", Json::Arr(keys));
        self.request(&req)?.map(|_| ()).map_err(|(c, m)| format!("settingsd refused the keys ({c}): {m}"))
    }

    /// Unregisters `app.<name>.`; "no keys there" (unknown_key) is no error: nothing is left.
    pub fn unregister(&self, name: &str, forget: bool) -> Result<usize, String> {
        self.unregister_below(&format!("app.{name}."), forget)
    }

    /// One key or a group: settingsd's prefix is a whole key name (registry.rs `under`), never part of one.
    fn unregister_below(&self, prefix: &str, forget: bool) -> Result<usize, String> {
        let req = Json::obj().with("op", "unregister").with("owner", OWNER).with("prefix", prefix).with("forget", forget);
        match self.request(&req)? {
            Ok(j) => Ok(j.get("removed").and_then(Json::as_i64).unwrap_or(0) as usize),
            Err((c, _)) if c == "unknown_key" => Ok(0),
            Err((c, m)) => Err(format!("settingsd refused to unregister {prefix} ({c}): {m}")),
        }
    }
}

/// A manifest key as settingsd's registration reads it (the reverse of manifest.rs setting_def).
fn key_json(s: &Setting) -> Json {
    let d = &s.def;
    Json::obj()
        .with("key", d.name.as_str())
        .with("schema", d.ty.describe())
        .with("default", d.default.to_json())
        .with("text", Json::obj().with("de", d.de.as_str()).with("en", d.en.as_str()))
        .with("restart", d.restart.name())
        .with("safety", d.safety.name())
}

impl Hook for Settings {
    fn what(&self) -> &'static str {
        "settings"
    }

    fn needed(&self, ms: &[Manifest]) -> bool {
        ms.iter().any(|m| !m.settings.is_empty())
    }

    fn ready(&self, ms: &[Manifest]) -> Result<(), String> {
        if self.needed(ms)
            && let Err(e) = self.status()
        {
            return Err(format!("{e}: its keys would stay behind; start settingsd (or remove with --settings none and its keys stay)"));
        }
        Ok(())
    }

    fn install(&self, m: &Manifest) -> Result<(), String> {
        self.register(&m.settings)
    }

    /// A key only another version on disk defines is unregistered with its value kept (a rollback brings it back);
    /// a key only a version this command removed defined is forgotten (registered first: after a boot settingsd
    /// would not know it). Then `cur`'s keys, re-adopting their values.
    fn current(&self, cur: &Manifest, others: &[Manifest], gone: &[Manifest]) -> Result<(), String> {
        let now: BTreeSet<&str> = cur.settings.iter().map(|s| s.def.name.as_str()).collect();
        let kept: BTreeSet<&str> = others.iter().flat_map(|m| m.settings.iter().map(|s| s.def.name.as_str())).collect();
        for k in kept.iter().filter(|k| !now.contains(*k)) {
            self.unregister_below(k, false)?;
        }
        for s in gone.iter().flat_map(|m| m.settings.iter()) {
            let k = s.def.name.as_str();
            if !now.contains(k) && !kept.contains(k) {
                self.register(std::slice::from_ref(s))?;
                self.unregister_below(k, true)?;
            }
        }
        self.register(&cur.settings)
    }

    /// Each version's keys on their own (two versions may define a key differently), then one unregister of the
    /// whole prefix. `keep` (`--keep-data`): the values stay as orphans for a later install.
    fn remove(&self, name: &str, ms: &[Manifest], keep: bool) -> Result<(), String> {
        if !self.needed(ms) && !self.reachable() {
            return Ok(());
        }
        for m in ms {
            if !keep && !m.settings.is_empty() {
                self.register(&m.settings)?;
                self.unregister(name, true)?;
            }
        }
        self.unregister(name, !keep).map(|_| ())
    }
}
