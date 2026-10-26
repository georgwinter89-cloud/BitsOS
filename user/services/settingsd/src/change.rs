//! Changing a value: check, apply (kernel file, watchers, settingsd itself), store, journal. `set`, `reset` and
//! `undo` all end in `change`, so every path journals and every path can be undone.

use crate::events::{Level, q};
use crate::journal::{Entry, Journal};
use crate::json::Json;
use crate::key::{Applier, KeyDef, Restart, Safety};
use crate::registry::{Error, Registry, err, now_ms};
use crate::store::{self, Job, Place, Writer};
use crate::value::Value;
use std::collections::BTreeSet;

pub struct Who<'a> {
    pub source: &'a str,
    pub confirmed: bool,
}

impl Registry {
    pub fn set(&mut self, key: &str, value: &Json, who: &Who) -> Result<Json, Error> {
        let d = self.def(key)?.clone();
        let v = d.ty.parse_json(value).map_err(|e| err("bad_value", format!("{key}: {e}")))?;
        self.change(&d, if v == d.default { None } else { Some(v) }, who, "set", None)
    }

    pub fn reset(&mut self, key: &str, who: &Who) -> Result<Json, Error> {
        let d = self.def(key)?.clone();
        self.change(&d, None, who, "reset", None)
    }

    /// Reverts the newest `n` changes not undone yet, newest first. Stops at the first one that cannot be
    /// undone (a secret, a key no longer registered, a `confirm` key without `confirmed`) and says so; what
    /// was undone before stays undone. An undo is a change like any other: a `confirm` key needs `confirmed`.
    pub fn undo(&mut self, n: usize, who: &Who) -> Result<Json, Error> {
        self.undo_expect(n, None, who)
    }

    /// `undo` that names the change it may revert (WY-10/2): with `expect` and n == 1, a newer change made
    /// meanwhile (another program, a second page) is `changed_meanwhile` and nothing changes.
    pub fn undo_expect(&mut self, n: usize, expect: Option<u64>, who: &Who) -> Result<Json, Error> {
        let todo = self.journal.undo_candidates(n.max(1));
        if todo.is_empty() {
            return Err(err("nothing_to_undo", "the journal has no change left to undo"));
        }
        if let Some(want) = expect
            && n <= 1
            && todo[0].seq != want
        {
            return Err(err(
                "changed_meanwhile",
                format!("the newest change is {} ({}), not {want}: nothing undone", todo[0].seq, todo[0].key),
            ));
        }
        let mut done = Vec::new();
        let mut stop: Option<Error> = None;
        for e in todo {
            if e.secret {
                stop = Some(err("not_undoable", format!("{}: a secret's old value is not kept", e.key)));
                break;
            }
            let Some(d) = self.defs.get(&e.key).cloned() else {
                stop = Some(err("not_undoable", format!("{}: no longer registered", e.key)));
                break;
            };
            match self.change(&d, e.old.clone(), who, "undo", Some(e.seq)) {
                Ok(j) => done.push(j),
                Err(x) => {
                    stop = Some(Error { code: x.code, msg: format!("{}: {}", e.key, x.msg) });
                    break;
                }
            }
        }
        if done.is_empty() {
            return Err(stop.unwrap_or_else(|| err("not_undoable", "")));
        }
        Ok(Json::obj().with("ok", true).with("undone", done).with("stopped", stop.map(|e| e.msg)))
    }

    pub(crate) fn change(
        &mut self,
        d: &KeyDef,
        new: Option<Value>,
        who: &Who,
        op: &str,
        undoes: Option<u64>,
    ) -> Result<Json, Error> {
        if d.safety == Safety::Confirm && !who.confirmed {
            return Err(err("needs_confirm", format!("{}: ask Georg first, then send \"confirmed\": true", d.name)));
        }
        let old = self.values.get(&d.name).cloned();
        let eff = |v: &Option<Value>| v.clone().unwrap_or_else(|| d.default.clone());
        if old == new && undoes.is_none() {
            return Ok(self.answer(d, &old, &new, "unchanged", None, "-").with("changed", false));
        }
        if let Applier::Kernel { config, .. } = &d.applier {
            let check = Job::ConfigCheck(self.paths.config(&self.place), config.clone(), new.as_ref().map(Value::show));
            if let Err(e) = self.writer.run(vec![check]) {
                if let Some(why) = e.strip_prefix(crate::store::CONTENT) {
                    return Err(err("config_refused", format!("{}: CONFIG.TXT left alone: {why}", d.name)));
                }
                if matches!(self.place, Place::Stick) {
                    // The stick failed or hangs: move now, so the write below does not wait a second time.
                    self.move_to_fallback(&e);
                }
            }
        }
        let applied = self.apply(d, &eff(&new))?;
        match &new {
            Some(v) => self.values.insert(d.name.clone(), v.clone()),
            None => self.values.remove(&d.name),
        };
        let secret = d.safety == Safety::Secret;
        let entry = self.journal.add(Entry {
            seq: 0,
            t_ms: now_ms(),
            key: d.name.clone(),
            old: if secret { None } else { old.clone() },
            new: if secret { None } else { new.clone() },
            source: who.source.to_string(),
            op: op.to_string(),
            undoes,
            secret,
        });
        let keep = self
            .effective("settings.history_keep")
            .and_then(|v| if let Value::Int(n) = v { usize::try_from(n).ok() } else { None });
        let trimmed = keep.is_some_and(|k| self.journal.trim(k));
        let stored = self.persist(d, &entry, trimmed);
        let show = |v: &Option<Value>| if secret { "***".to_string() } else { eff(v).show() };
        self.ev.emit(
            Level::Info,
            op,
            &format!(
                "key={} old={} new={} source={} applied={applied} stored={stored} seq={}",
                d.name,
                q(&show(&old)),
                q(&show(&new)),
                q(who.source),
                entry.seq
            ),
        );
        let line = Json::obj()
            .with("event", "changed")
            .with("key", d.name.as_str())
            .with("value", if secret { Json::Null } else { eff(&new).to_json() })
            .with("default", new.is_none())
            .with("seq", entry.seq)
            .with("op", op)
            .with("source", who.source)
            .to_text();
        let told = self.notify(&d.name, &line);
        let applied = if applied == "notified" { format!("notified {told} watcher(s)") } else { applied.to_string() };
        Ok(self.answer(d, &old, &new, &applied, Some(entry.seq), stored).with("changed", true))
    }

    fn answer(
        &self,
        d: &KeyDef,
        old: &Option<Value>,
        new: &Option<Value>,
        applied: &str,
        seq: Option<u64>,
        stored: &str,
    ) -> Json {
        let v = |x: &Option<Value>| {
            if d.safety == Safety::Secret {
                Json::from("***")
            } else {
                x.clone().unwrap_or_else(|| d.default.clone()).to_json()
            }
        };
        Json::obj()
            .with("ok", true)
            .with("key", d.name.as_str())
            .with("old", v(old))
            .with("new", v(new))
            .with("applied", applied)
            .with("restart", d.restart.name())
            .with("seq", seq)
            .with("stored", stored)
    }

    /// Makes `v` take effect. Err only when the kernel refused it or its file hung (then nothing changed).
    fn apply(&mut self, d: &KeyDef, v: &Value) -> Result<&'static str, Error> {
        Ok(match &d.applier {
            Applier::Kernel { sys: Some(s), .. } => {
                let p = self.paths.sys.join(s);
                if self.sys_stuck.as_ref().is_some_and(|b| b.load(std::sync::atomic::Ordering::Acquire)) {
                    return Err(err(
                        "apply_failed",
                        format!("{}: {} still hangs since an earlier write", d.name, p.display()),
                    ));
                }
                // sysfs takes the whole buffer (linux-6.6/fs/sysfs/file.c sysfs_kf_write); a newline ends it.
                let (p2, text) = (p.clone(), format!("{}\n", v.show()));
                let r = self.sys.call(move || store::write_attr(&p2, &text).map_err(|e| e.to_string()));
                let fail = |this: &Self, e: String| {
                    this.ev.emit(
                        Level::Warn,
                        "apply.failed",
                        &format!("key={} file={} error={}", d.name, p.display(), q(&e)),
                    );
                    err("apply_failed", format!("{}: the kernel refused {:?}: {e}", d.name, v.show()))
                };
                match r {
                    Ok(Ok(true)) => "now",
                    Ok(Ok(false)) => "next_start",
                    Ok(Err(e)) => return Err(fail(self, e)),
                    Err(e) => {
                        self.sys_stuck = Some(self.sys.abandon());
                        self.sys = Writer::new(self.sys.deadline);
                        return Err(fail(self, e));
                    }
                }
            }
            Applier::Kernel { sys: None, .. } => "next_start",
            Applier::Service(_) if d.restart == Restart::Service => "restart_service",
            Applier::Service(_) => "notified",
            Applier::Settingsd => "now",
        })
    }

    /// Writes the key's file (or CONFIG.TXT line) and the journal. A failure on the stick moves everything to
    /// the fallback; the answer's `stored` says where the value is now.
    fn persist(&mut self, d: &KeyDef, e: &Entry, rewrite_journal: bool) -> &'static str {
        if matches!(self.place, Place::Fallback(_)) {
            self.pending.insert(d.name.clone());
        }
        let mut jobs = vec![self.job_for(&d.name)];
        let jpath = self.paths.dir(&self.place).join("journal.log");
        jobs.push(if rewrite_journal {
            Job::Write(jpath, self.journal.text().into_bytes())
        } else {
            Job::Append(jpath, Journal::line(e).into_bytes())
        });
        if matches!(self.place, Place::Fallback(_)) {
            jobs.push(self.pending_job());
        }
        match (self.writer.run(jobs), self.place.clone()) {
            (Ok(()), Place::Stick) => "stick",
            (Ok(()), Place::Fallback(_)) => "fallback",
            (Err(why), Place::Stick) => {
                self.pending.insert(d.name.clone());
                self.move_to_fallback(&why);
                if matches!(self.place, Place::Fallback(_)) { "fallback" } else { "not_stored" }
            }
            (Err(why), Place::Fallback(_)) => {
                self.ev.emit(Level::Error, "store.failed", &format!("key={} place=fallback error={}", d.name, q(&why)));
                "not_stored"
            }
        }
    }

    /// The write that stores `key` where it lives: its CONFIG.TXT line, or its group file.
    pub(crate) fn job_for(&self, key: &str) -> Job {
        if let Some(Applier::Kernel { config, .. }) = self.defs.get(key).map(|d| &d.applier) {
            return Job::Config(self.paths.config(&self.place), config.clone(), self.values.get(key).map(Value::show));
        }
        self.group_job(crate::key::group(key))
    }

    pub(crate) fn group_job(&self, group: &str) -> Job {
        let entries: Vec<(String, Value)> = self
            .values
            .iter()
            .chain(self.orphans.iter())
            .filter(|(k, _)| crate::key::group(k) == group && !self.is_kernel(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let path = self.paths.dir(&self.place).join(format!("{group}.toml"));
        if entries.is_empty() {
            Job::Remove(path)
        } else {
            Job::Write(path, crate::tomlish::write(group, &entries).into_bytes())
        }
    }

    pub(crate) fn is_kernel(&self, key: &str) -> bool {
        matches!(self.defs.get(key).map(|d| &d.applier), Some(Applier::Kernel { .. }))
    }

    pub(crate) fn pending_job(&self) -> Job {
        let text: String = self.pending.iter().map(|k| format!("{k}\n")).collect();
        Job::Write(self.paths.pending(), text.into_bytes())
    }

    pub(crate) fn rewrite_groups(&mut self, keys: &BTreeSet<String>) {
        let groups: BTreeSet<&str> = keys.iter().map(|k| crate::key::group(k)).collect();
        let jobs: Vec<Job> = groups.iter().map(|g| self.group_job(g)).collect();
        if let Err(e) = self.writer.run(jobs) {
            self.ev.emit(Level::Warn, "store.failed", &format!("what=groups error={}", q(&e)));
        }
    }

    pub(crate) fn rewrite_journal(&mut self) {
        let p = self.paths.dir(&self.place).join("journal.log");
        if let Err(e) = self.writer.run(vec![Job::Write(p, self.journal.text().into_bytes())]) {
            self.ev.emit(Level::Warn, "store.failed", &format!("what=journal error={}", q(&e)));
        }
    }
}
