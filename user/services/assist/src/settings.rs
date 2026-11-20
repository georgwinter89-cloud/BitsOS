//! The client side of settingsd's socket (`/run/bitsos/settings.sock`, one JSON line each way). One connection
//! per request: settingsd may restart between two questions of a long chat.

use crate::json::{self, Json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The name this program registers its keys under; settingsd shows a secret only to the key's owner.
pub const OWNER: &str = "assist";
pub const SOCKET: &str = "/run/bitsos/settings.sock";
const PATIENCE: Duration = Duration::from_secs(60);

/// settingsd's refusal (`code` from its protocol table) or a failure to reach it (`code` "unreachable").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub trait Settings {
    /// One request, settingsd's answer line as JSON (also when it says `"ok": false`).
    fn request(&self, req: &Json) -> Result<Json, Refused>;
}

pub struct Socket {
    pub path: PathBuf,
    /// The most one answer may hold (the caller's memory budget).
    pub budget: usize,
    /// When the caller's own wait ends (`assist --test`): no answer is waited for beyond it.
    pub until: Option<Instant>,
}

impl Socket {
    pub fn new(path: Option<&str>, budget: usize) -> Socket {
        let env = std::env::var("SETTINGS_SOCKET").ok().filter(|s| !s.is_empty());
        Socket { path: PathBuf::from(path.map(str::to_string).or(env).unwrap_or_else(|| SOCKET.to_string())), budget, until: None }
    }

    /// The same socket for a caller that gives up at `until`.
    pub fn until(self, until: Instant) -> Socket {
        Socket { until: Some(until), ..self }
    }

    /// How long one answer may take. A change may wait for the stick; longer than [`PATIENCE`] and settingsd
    /// is stuck. Err: the caller's own wait is already over.
    fn wait(&self) -> Result<Duration, String> {
        match self.until.map(|u| u.saturating_duration_since(Instant::now())) {
            None => Ok(PATIENCE),
            Some(left) if left.is_zero() => Err("no answer in the time the caller has".into()),
            Some(left) => Ok(left.min(PATIENCE)),
        }
    }
}

impl Settings for Socket {
    fn request(&self, req: &Json) -> Result<Json, Refused> {
        let away = |e: String| Refused { code: "unreachable".into(), message: format!("{}: {e}", self.path.display()) };
        let wait = self.wait().map_err(away)?;
        let s = UnixStream::connect(&self.path).map_err(|e| away(e.to_string()))?;
        s.set_read_timeout(Some(wait)).map_err(|e| away(e.to_string()))?;
        s.set_write_timeout(Some(wait)).map_err(|e| away(e.to_string()))?;
        let mut w = s.try_clone().map_err(|e| away(e.to_string()))?;
        writeln!(w, "{}", req.to_text()).map_err(|e| away(e.to_string()))?;
        let mut line = String::new();
        let limit = u64::try_from(self.budget).unwrap_or(u64::MAX);
        BufReader::new(s.take(limit)).read_line(&mut line).map_err(|e| away(e.to_string()))?;
        if !line.ends_with('\n') {
            return Err(away(if line.is_empty() { "closed without an answer".into() } else { "answer longer than the free memory allows".into() }));
        }
        json::parse(line.trim()).map_err(|e| away(format!("answer is not JSON: {e}")))
    }
}

/// The answer when it says ok, settingsd's refusal otherwise.
pub fn ok(answer: Json) -> Result<Json, Refused> {
    if answer.get("ok").and_then(Json::as_bool) == Some(true) {
        return Ok(answer);
    }
    Err(Refused { code: answer.str("error").unwrap_or("error").to_string(), message: answer.str("message").unwrap_or("").to_string() })
}

fn op(name: &str) -> Json {
    Json::obj().with("op", name)
}

/// Every key under `prefix` with its value as settingsd shows it (secrets as `***`), and whether that value
/// was set (true) or is the default the key was registered with (false).
pub fn listing(s: &dyn Settings, prefix: &str) -> Result<BTreeMap<String, (Json, bool)>, Refused> {
    let a = ok(s.request(&op("list").with("prefix", prefix))?)?;
    let mut out = BTreeMap::new();
    if let Some(Json::Arr(keys)) = a.get("keys") {
        for k in keys {
            if let (Some(name), Some(v)) = (k.str("key"), k.get("value")) {
                out.insert(name.to_string(), (v.clone(), k.get("changed").and_then(Json::as_bool) == Some(true)));
            }
        }
    }
    Ok(out)
}

/// The values alone.
pub fn list(s: &dyn Settings, prefix: &str) -> Result<BTreeMap<String, Json>, Refused> {
    Ok(listing(s, prefix)?.into_iter().map(|(k, (v, _))| (k, v)).collect())
}

pub fn get(s: &dyn Settings, key: &str) -> Result<Json, Refused> {
    let a = ok(s.request(&op("get").with("key", key))?)?;
    Ok(a.get("value").cloned().unwrap_or(Json::Null))
}

/// A secret of this program, in clear. The only caller is the one that puts it into a request header.
pub fn get_secret(s: &dyn Settings, key: &str) -> Result<String, Refused> {
    let a = ok(s.request(&op("get").with("key", key).with("owner", OWNER))?)?;
    Ok(a.str("value").unwrap_or("").to_string())
}

pub fn set(s: &dyn Settings, key: &str, value: impl Into<Json>, source: &str) -> Result<Json, Refused> {
    ok(s.request(&op("set").with("key", key).with("value", value).with("source", source))?)
}

/// `set`, but only while the settings in `seen` still have these values (settingsd checks and stores in one
/// step; `changed_meanwhile` otherwise).
pub fn set_while(s: &dyn Settings, key: &str, value: impl Into<Json>, source: &str, seen: &BTreeMap<String, Json>) -> Result<Json, Refused> {
    let cond = seen.iter().fold(Json::obj(), |j, (k, v)| j.with(k.as_str(), v.clone()));
    ok(s.request(&op("set").with("key", key).with("value", value).with("source", source).with("while", cond))?)
}

pub fn register(s: &dyn Settings, keys: Vec<Json>) -> Result<(), Refused> {
    if keys.is_empty() {
        return Ok(());
    }
    ok(s.request(&op("register").with("owner", OWNER).with("keys", keys))?).map(|_| ())
}
