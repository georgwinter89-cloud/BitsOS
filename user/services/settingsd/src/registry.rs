//! The registry: key definitions, current values, the journal, namespace owners and watchers. One instance
//! behind a mutex serves every client. Changes are in `change.rs`, files and the fallback in `sync.rs`.

use crate::builtin;
use crate::events::{Events, Level, q};
use crate::journal::Journal;
use crate::json::Json;
use crate::key::{self, Applier, KeyDef, Safety};
use crate::store::{Paths, Place, Writer};
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub msg: String,
}

pub fn err(code: &'static str, msg: impl Into<String>) -> Error {
    Error { code, msg: msg.into() }
}

/// A client that asked to hear about changes below `prefix`. Its queue is bounded so a client that stops
/// reading cannot grow settingsd; on overflow it is marked lagged (it gets `{"event":"lagged"}` and re-reads).
pub struct Watch {
    pub prefix: String,
    pub tx: SyncSender<String>,
    pub lagged: Arc<AtomicBool>,
}

/// Queue length per watcher: a burst of changes (an undo of many keys, a resync) fits.
const WATCH_QUEUE: usize = 1024;

pub struct Registry {
    pub(crate) paths: Paths,
    pub(crate) place: Place,
    pub(crate) writer: Writer,
    pub(crate) defs: BTreeMap<String, KeyDef>,
    /// Values that differ from the default, of registered keys.
    pub(crate) values: BTreeMap<String, Value>,
    /// Values found in files for keys nobody registered (yet): kept and written back unchanged.
    pub(crate) orphans: BTreeMap<String, Value>,
    pub(crate) journal: Journal,
    /// Namespace (first segment) -> the owner that defines keys in it (shared namespaces are not listed).
    pub(crate) owners: BTreeMap<String, String>,
    /// Keys changed while on the fallback, carried to the stick at the resync.
    pub(crate) pending: BTreeSet<String>,
    /// Shared with every snapshot, so a watcher can be added without the registry lock.
    pub(crate) watches: Arc<Mutex<Vec<Watch>>>,
    pub(crate) ev: Events,
    /// Busy flags of abandoned stick workers: while one is still stuck, the stick is not touched again.
    pub(crate) stuck: Vec<Arc<AtomicBool>>,
    /// A separate worker for `/sys/bitsos` writes, so a hung stick never delays a kernel key (and back).
    pub(crate) sys: Writer,
    pub(crate) sys_stuck: Option<Arc<AtomicBool>>,
    pub(crate) retry_at: Instant,
    pub(crate) retry_every: Duration,
    /// A migration marker not written yet (the stick was still read-only at the start): path, content, next try.
    pub(crate) marker_due: Option<crate::migrate::Marker>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub(crate) fn under(key: &str, prefix: &str) -> bool {
    let p = prefix.trim_end_matches('.');
    p.is_empty() || key == p || (key.starts_with(p) && key.as_bytes().get(p.len()) == Some(&b'.'))
}

impl Registry {
    /// Builds the registry with the built-in keys and loads the files (stick, else fallback).
    pub fn open(paths: Paths, ev: Events, deadline: Duration) -> Registry {
        let mut r = Registry {
            paths,
            place: Place::Stick,
            writer: Writer::new(deadline),
            defs: BTreeMap::new(),
            values: BTreeMap::new(),
            orphans: BTreeMap::new(),
            journal: Journal::default(),
            owners: BTreeMap::new(),
            pending: BTreeSet::new(),
            watches: Arc::new(Mutex::new(Vec::new())),
            ev,
            stuck: Vec::new(),
            sys: Writer::new(deadline),
            sys_stuck: None,
            retry_at: Instant::now(),
            retry_every: Duration::from_secs(10),
            marker_due: None,
        };
        for d in builtin::all() {
            match d.validate() {
                Ok(()) => {
                    r.defs.insert(d.name.clone(), d);
                }
                Err(e) => r.ev.emit(Level::Error, "builtin.bad", &format!("key={} reason={}", d.name, q(&e))),
            }
        }
        r.load();
        r
    }

    /// A copy for readers (`list`, `get`, `describe`, `history`, `status`, `watch`): the server answers them
    /// from the last snapshot, so they never wait behind a change that waits for a hung stick.
    pub fn snapshot(&self) -> Registry {
        Registry {
            paths: self.paths.clone(),
            place: self.place.clone(),
            writer: self.writer.clone(),
            defs: self.defs.clone(),
            values: self.values.clone(),
            orphans: self.orphans.clone(),
            journal: self.journal.clone(),
            owners: self.owners.clone(),
            pending: self.pending.clone(),
            watches: self.watches.clone(),
            ev: self.ev.clone(),
            stuck: Vec::new(),
            sys: self.sys.clone(),
            sys_stuck: None,
            retry_at: self.retry_at,
            retry_every: self.retry_every,
            marker_due: None,
        }
    }

    /// Gives up on the stick worker after a timeout and starts a fresh one.
    pub(crate) fn replace_writer(&mut self) {
        self.stuck.push(self.writer.abandon());
        self.writer = Writer::new(self.writer.deadline);
    }

    /// Whether an abandoned stick worker is still stuck in the kernel.
    pub(crate) fn still_hung(&mut self) -> bool {
        self.stuck.retain(|b| b.load(std::sync::atomic::Ordering::Acquire));
        !self.stuck.is_empty()
    }

    pub fn effective(&self, key: &str) -> Option<Value> {
        let d = self.defs.get(key)?;
        Some(self.values.get(key).cloned().unwrap_or_else(|| d.default.clone()))
    }

    pub(crate) fn language(&self) -> String {
        self.effective("settings.language").map(|v| v.show()).unwrap_or_else(|| "de".into())
    }

    fn shown(&self, d: &KeyDef, v: &Value) -> Json {
        if d.safety == Safety::Secret { Json::from("***") } else { v.to_json() }
    }

    pub fn list(&self, prefix: &str) -> Json {
        let lang = self.language();
        let keys: Vec<Json> = self
            .defs
            .values()
            .filter(|d| under(&d.name, prefix))
            .map(|d| {
                let v = self.effective(&d.name).unwrap_or_else(|| d.default.clone());
                Json::obj()
                    .with("key", d.name.as_str())
                    .with("type", d.ty.name())
                    .with("value", self.shown(d, &v))
                    .with("changed", self.values.contains_key(&d.name))
                    .with("text", if lang == "en" { d.en.as_str() } else { d.de.as_str() })
            })
            .collect();
        Json::obj().with("ok", true).with("keys", keys)
    }

    pub fn describe(&self, key: &str) -> Result<Json, Error> {
        let d = self.def(key)?;
        Ok(Json::obj().with("ok", true).with("key", d.describe(self.values.get(key))))
    }

    pub(crate) fn def(&self, key: &str) -> Result<&KeyDef, Error> {
        crate::guard::refuse_key(key).map_err(|e| err("refused_r8", e))?;
        self.defs.get(key).ok_or_else(|| err("unknown_key", format!("{key}: no such setting")))
    }

    /// `as_owner`: a secret value is shown only to the key's owner.
    pub fn get(&self, key: &str, as_owner: Option<&str>) -> Result<Json, Error> {
        let d = self.def(key)?;
        let v = self.effective(key).unwrap_or_else(|| d.default.clone());
        let value = if d.safety == Safety::Secret && as_owner == Some(d.owner.as_str()) {
            v.to_json()
        } else {
            self.shown(d, &v)
        };
        Ok(Json::obj()
            .with("ok", true)
            .with("key", key)
            .with("value", value)
            .with("changed", self.values.contains_key(key)))
    }

    /// Newest first. A `null` old/new value (the default at the time) is shown as today's default, marked
    /// `old_default`/`new_default`, so a reader never has to know the defaults.
    pub fn history(&self, n: usize, prefix: Option<&str>) -> Json {
        let mut es = self.journal.history(n, prefix);
        for e in &mut es {
            let Some(d) = e.str("key").and_then(|k| self.defs.get(k)).filter(|d| d.safety != Safety::Secret) else {
                continue;
            };
            let Json::Obj(fields) = e else { continue };
            let mut marks = Vec::new();
            for (k, v) in fields.iter_mut() {
                if (k == "old" || k == "new") && *v == Json::Null {
                    *v = d.default.to_json();
                    marks.push((format!("{k}_default"), Json::Bool(true)));
                }
            }
            fields.extend(marks);
        }
        Json::obj().with("ok", true).with("entries", es)
    }

    pub fn status(&self) -> Json {
        let (place, reason) = match &self.place {
            Place::Stick => ("stick", None),
            Place::Fallback(r) => ("fallback", Some(r.clone())),
        };
        Json::obj()
            .with("ok", true)
            .with("place", place)
            .with("reason", reason)
            .with("dir", self.paths.dir(&self.place).display().to_string())
            .with("keys", self.defs.len())
            .with("changed", self.values.len())
            .with("journal", self.journal.entries.len())
            .with("pending", self.pending.len())
            .with("watchers", self.watches.lock().unwrap_or_else(|p| p.into_inner()).len())
    }

    pub fn watch(&self, prefix: &str) -> (Receiver<String>, Arc<AtomicBool>) {
        let (tx, rx) = sync_channel(WATCH_QUEUE);
        let lagged = Arc::new(AtomicBool::new(false));
        self.watches.lock().unwrap_or_else(|p| p.into_inner()).push(Watch {
            prefix: prefix.to_string(),
            tx,
            lagged: lagged.clone(),
        });
        (rx, lagged)
    }

    /// Tells every watcher of `key`; drops watchers that went away. Returns how many were told.
    pub(crate) fn notify(&self, key: &str, line: &str) -> usize {
        let mut told = 0;
        self.watches.lock().unwrap_or_else(|p| p.into_inner()).retain(|w| {
            if !under(key, &w.prefix) {
                return true;
            }
            match w.tx.try_send(line.to_string()) {
                Ok(()) => {
                    told += 1;
                    true
                }
                Err(TrySendError::Full(_)) => {
                    w.lagged.store(true, std::sync::atomic::Ordering::Release);
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
        told
    }

    /// Tells every watcher whose prefix covers one of `keys` that `owner`'s keys came or went (WY-10/1), so a
    /// page reads its rows again: `{"event":"registered"|"unregistered","owner":..,"keys":[..]}`.
    fn notify_keys(&self, event: &str, owner: &str, keys: &BTreeSet<String>) {
        let line = Json::obj()
            .with("event", event)
            .with("owner", owner)
            .with("keys", keys.iter().map(|k| Json::from(k.as_str())).collect::<Vec<_>>())
            .to_text();
        self.watches.lock().unwrap_or_else(|p| p.into_inner()).retain(|w| {
            if !keys.iter().any(|k| under(k, &w.prefix)) {
                return true;
            }
            match w.tx.try_send(line.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    w.lagged.store(true, std::sync::atomic::Ordering::Release);
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// Registers keys of `owner` (a service at its start). All or nothing.
    pub fn register(&mut self, owner: &str, defs: Vec<KeyDef>) -> Result<Json, Error> {
        if owner.is_empty()
            || !owner.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(err("bad_request", "owner: want a lower-case service name"));
        }
        let names: BTreeSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        if names.len() != defs.len() {
            return Err(err("bad_request", "a key is listed twice"));
        }
        for d in &defs {
            d.validate().map_err(|e| {
                err(if crate::guard::refuse_key(&d.name).is_err() { "refused_r8" } else { "bad_request" }, e)
            })?;
            if !matches!(&d.applier, Applier::Service(s) if s == owner) || d.owner != owner {
                return Err(err("bad_request", format!("{}: a service registers keys it applies itself", d.name)));
            }
            let ns = key::group(&d.name);
            if owner != "settingsd" && builtin::RESERVED.contains(&ns) {
                return Err(err("not_owner", format!("{}: namespace {ns} belongs to settingsd", d.name)));
            }
            if !builtin::SHARED.contains(&ns)
                && let Some(o) = self.owners.get(ns)
                && o != owner
            {
                return Err(err("not_owner", format!("{}: namespace {ns} belongs to {o}", d.name)));
            }
            if let Some(old) = self.defs.get(&d.name)
                && old.owner != owner
            {
                return Err(err("not_owner", format!("{}: key belongs to {}", d.name, old.owner)));
            }
            // A key may not also be a table of other keys: the TOML files could not hold both.
            let mut others = self.defs.keys().map(String::as_str).chain(names.iter().copied());
            if let Some(c) = others.find(|k| *k != d.name && (under(k, &d.name) || under(&d.name, k))) {
                return Err(err(
                    "bad_request",
                    format!("{} conflicts with {c} (a key cannot be a group of keys)", d.name),
                ));
            }
        }
        let n = defs.len();
        let all: BTreeSet<String> = defs.iter().map(|d| d.name.clone()).collect();
        let mut touched = BTreeSet::new();
        for d in defs {
            let ns = key::group(&d.name).to_string();
            if !builtin::SHARED.contains(&ns.as_str()) {
                self.owners.insert(ns, owner.to_string());
            }
            let raw = self.orphans.remove(&d.name).or_else(|| self.values.remove(&d.name));
            if let Some(raw) = raw {
                match self.coerce(&d, &raw) {
                    Ok(Some(v)) => {
                        self.values.insert(d.name.clone(), v);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.ev.emit(Level::Warn, "value.dropped", &format!("key={} reason={}", d.name, q(&e)));
                        touched.insert(d.name.clone());
                    }
                }
            }
            self.defs.insert(d.name.clone(), d);
        }
        self.ev.emit(Level::Info, "registered", &format!("owner={owner} keys={n}"));
        self.rewrite_groups(&touched);
        self.notify_keys("registered", owner, &all);
        Ok(Json::obj().with("ok", true).with("registered", n))
    }

    /// Removes `owner`'s keys below `prefix`. `forget`: their values and journal entries go too (a package
    /// removed without leftovers, PK-2); otherwise the values stay for a later registration.
    pub fn unregister(&mut self, owner: &str, prefix: &str, forget: bool) -> Result<Json, Error> {
        if owner == "settingsd" {
            return Err(err("not_owner", "built-in keys stay"));
        }
        let keys: BTreeSet<String> =
            self.defs.values().filter(|d| d.owner == owner && under(&d.name, prefix)).map(|d| d.name.clone()).collect();
        if keys.is_empty() {
            return Err(err("unknown_key", format!("{owner} has no keys below {prefix:?}")));
        }
        for k in &keys {
            self.defs.remove(k);
            let v = self.values.remove(k);
            if forget {
                self.orphans.remove(k);
                self.pending.remove(k);
            } else if let Some(v) = v {
                self.orphans.insert(k.clone(), v);
            }
        }
        let still: BTreeSet<&str> =
            self.defs.values().filter(|d| d.owner == owner).map(|d| key::group(&d.name)).collect();
        self.owners.retain(|ns, o| o != owner || still.contains(ns.as_str()));
        if forget {
            let hs = keys.iter().cloned().collect();
            if self.journal.forget(&hs) {
                self.rewrite_journal();
            }
            self.rewrite_groups(&keys);
        }
        self.ev.emit(
            Level::Info,
            "unregistered",
            &format!("owner={owner} prefix={} keys={} forget={forget}", q(prefix), keys.len()),
        );
        self.notify_keys("unregistered", owner, &keys);
        Ok(Json::obj().with("ok", true).with("removed", keys.len()))
    }

    /// A value from a file in the key's normal form; None = the default (not stored).
    pub(crate) fn coerce(&self, d: &KeyDef, raw: &Value) -> Result<Option<Value>, String> {
        let v = d.ty.check(raw.clone()).or_else(|_| d.ty.parse_text(&raw.show()))?;
        Ok(if v == d.default { None } else { Some(v) })
    }
}
