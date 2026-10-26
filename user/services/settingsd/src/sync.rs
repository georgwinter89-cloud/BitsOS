//! Loading the files, moving to the fallback when the stick fails, and carrying the fallback's changes back
//! when the stick is usable again. While on the fallback, `pending` lists the keys changed there; only those
//! overwrite the stick at the resync, so an edit Georg made to the stick on the Gaming-PC meanwhile survives.

use crate::events::{Level, q};
use crate::guard;
use crate::journal::Journal;
use crate::key::Applier;
use crate::registry::Registry;
use crate::store::{self, Job, Place, RawPlace};
use crate::tomlish;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub(crate) struct Loaded {
    groups: Vec<(String, Value)>,
    config: String,
    journal: Journal,
}

/// How long the resync waits at most between tries (doubling from 10 s).
const RETRY_MAX: Duration = Duration::from_secs(300);
/// The steady gap between tries while the stick answers but refuses (read-only, missing).
const RETRY_STEADY: Duration = Duration::from_secs(10);
/// The first gap after a start on the fallback (doubling up to `RETRY_STEADY`).
const START_RETRY: Duration = Duration::from_secs(1);

impl Registry {
    /// Parses what [`store::read_raw`] read on the worker; bad lines and unreadable files become events.
    pub(crate) fn parse_place(&self, raw: RawPlace) -> Loaded {
        for p in &raw.cleaned {
            self.ev.emit(Level::Info, "file.leftover_removed", &format!("file={}", p.display()));
        }
        let mut groups = Vec::new();
        for (p, t) in raw.files {
            match t {
                Ok(t) => {
                    let parsed = tomlish::read(&t);
                    for (line, why) in parsed.bad_lines {
                        self.ev.emit(
                            Level::Warn,
                            "file.bad_line",
                            &format!("file={} line={line} reason={}", p.display(), q(&why)),
                        );
                    }
                    groups.extend(parsed.entries);
                }
                Err(e) => {
                    self.ev.emit(Level::Warn, "file.unreadable", &format!("file={} error={}", p.display(), q(&e)))
                }
            }
        }
        let config = raw.config.unwrap_or_else(|e| {
            self.ev.emit(Level::Warn, "file.unreadable", &format!("file=CONFIG.TXT error={}", q(&e)));
            String::new()
        });
        let (journal, bad) = Journal::load(&raw.journal);
        if bad > 0 {
            self.ev.emit(Level::Warn, "journal.bad_lines", &format!("count={bad}"));
        }
        Loaded { groups, config, journal }
    }

    /// Probes the stick and reads it (plus the fallback when a resync is due) on the worker, within the
    /// deadline. Err(reason): the stick is not usable; a timeout also abandons the worker.
    fn read_stick(&mut self, with_fallback: bool) -> Result<(RawPlace, Option<RawPlace>), String> {
        let paths = self.paths.clone();
        let r = self.writer.call(move || -> Result<(RawPlace, Option<RawPlace>), String> {
            paths.probe_stick()?;
            let fb = (with_fallback && paths.pending().exists())
                .then(|| store::read_raw(&paths, &Place::Fallback(String::new())));
            Ok((store::read_raw(&paths, &Place::Stick), fb))
        });
        match r {
            Ok(r) => r,
            Err(e) => {
                self.replace_writer();
                Err(e)
            }
        }
    }

    /// Reads the fallback directory (RAM) on the worker; an empty place when even that fails.
    fn read_fallback(&mut self) -> Loaded {
        let paths = self.paths.clone();
        let raw = self.writer.call(move || {
            let _ = std::fs::create_dir_all(&paths.fallback);
            let pending = store::read_text(&paths.pending()).unwrap_or_default();
            (store::read_raw(&paths, &Place::Fallback(String::new())), pending)
        });
        match raw {
            Ok((raw, pending)) => {
                self.pending = pending.lines().map(str::to_string).filter(|k| !k.is_empty()).collect();
                self.parse_place(raw)
            }
            Err(e) => {
                self.ev.emit(Level::Error, "fallback.unreadable", &format!("error={}", q(&e)));
                self.replace_writer();
                Loaded { groups: Vec::new(), config: String::new(), journal: Journal::default() }
            }
        }
    }

    /// Takes the values of `l` (kernel keys from its CONFIG.TXT, the rest from the group files).
    fn take(&mut self, l: &Loaded) {
        self.values.clear();
        self.orphans.clear();
        for (k, raw) in &l.groups {
            match self.defs.get(k) {
                Some(d) if matches!(d.applier, Applier::Kernel { .. }) => {}
                Some(d) => match self.coerce(d, raw) {
                    Ok(Some(v)) => {
                        self.values.insert(k.clone(), v);
                    }
                    Ok(None) => {}
                    Err(e) => self.ev.emit(Level::Warn, "value.bad", &format!("key={k} reason={}", q(&e))),
                },
                None if crate::key::valid_name(k) && guard::refuse_key(k).is_ok() => {
                    self.orphans.insert(k.clone(), raw.clone());
                }
                None => self.ev.emit(Level::Warn, "value.ignored", &format!("key={}", q(k))),
            }
        }
        for d in self.defs.values() {
            if let Applier::Kernel { config, .. } = &d.applier
                && let Some(t) = guard::config_value(&l.config, config)
            {
                match d.ty.parse_text(&t) {
                    Ok(v) if v != d.default => {
                        self.values.insert(d.name.clone(), v);
                    }
                    Ok(_) => {}
                    Err(e) => self.ev.emit(
                        Level::Warn,
                        "value.bad",
                        &format!("key={} config={config} reason={}", d.name, q(&e)),
                    ),
                }
            }
        }
    }

    pub(crate) fn load(&mut self) {
        match self.read_stick(true) {
            Ok((raw, fb)) => {
                self.place = Place::Stick;
                let base = self.parse_place(raw);
                self.take(&base);
                self.journal = base.journal;
                if let Some(fb) = fb {
                    // settingsd ran on the fallback earlier in this boot: carry those changes over.
                    let fb = self.parse_place(fb);
                    let keys: BTreeSet<String> = store::read_text(&self.paths.pending())
                        .unwrap_or_default()
                        .lines()
                        .map(str::to_string)
                        .filter(|k| !k.is_empty())
                        .collect();
                    let stick_state = (self.values.clone(), self.orphans.clone());
                    self.take(&fb);
                    let fb_state = (self.values.clone(), self.orphans.clone());
                    (self.values, self.orphans) = stick_state;
                    self.overlay(&keys, &fb_state.0, &fb_state.1);
                    let last = self.journal.entries.last().map_or(0, |e| e.seq);
                    for e in fb.journal.entries.into_iter().filter(|e| e.seq > last) {
                        self.journal.push_loaded(e);
                    }
                    self.pending = keys;
                    self.push_pending("start");
                }
                self.ev.emit(
                    Level::Info,
                    "loaded",
                    &format!("place=stick keys={} changed={}", self.defs.len(), self.values.len()),
                );
                self.migrate();
            }
            Err(why) => {
                self.place = Place::Fallback(why.clone());
                // At boot the stick may not be decided yet (read-only until `fat stick.decided`, about 1 s after
                // settingsd starts): try again soon, 1, 2, 4, 8 s apart, then every 10 s (MM-9 CR6).
                self.retry_every = START_RETRY;
                self.retry_at = Instant::now() + START_RETRY;
                let l = self.read_fallback();
                self.take(&l);
                self.journal = l.journal;
                self.ev.emit(
                    Level::Warn,
                    "fallback",
                    &format!("reason={} dir={}", q(&why), self.paths.fallback.display()),
                );
            }
        }
    }

    /// Sets `keys` to the values in `vals`/`orph` (absent there = default).
    fn overlay(&mut self, keys: &BTreeSet<String>, vals: &BTreeMap<String, Value>, orph: &BTreeMap<String, Value>) {
        for k in keys {
            match vals.get(k).or_else(|| orph.get(k)) {
                Some(v) if self.defs.contains_key(k) => {
                    self.values.insert(k.clone(), v.clone());
                }
                Some(v) => {
                    self.orphans.insert(k.clone(), v.clone());
                }
                None => {
                    self.values.remove(k);
                    self.orphans.remove(k);
                }
            }
        }
    }

    /// Moves to the fallback after a failed or hung stick write: the whole state is written there.
    pub(crate) fn move_to_fallback(&mut self, why: &str) {
        self.replace_writer();
        self.place = Place::Fallback(why.to_string());
        let mut jobs = Vec::new();
        let groups: BTreeSet<String> = self
            .values
            .keys()
            .chain(self.orphans.keys())
            .filter(|k| !self.is_kernel(k))
            .map(|k| crate::key::group(k).to_string())
            .collect();
        for g in &groups {
            jobs.push(self.group_job(g));
        }
        let mut cfg = String::new();
        for d in self.defs.values() {
            if let Applier::Kernel { config, .. } = &d.applier
                && let Some(v) = self.values.get(&d.name)
                && let Ok(t) = guard::edit_config(&cfg, config, Some(&v.show()))
            {
                cfg = t;
            }
        }
        jobs.push(Job::Write(self.paths.config(&self.place), cfg.into_bytes()));
        jobs.push(Job::Write(self.paths.dir(&self.place).join("journal.log"), self.journal.text().into_bytes()));
        jobs.push(self.pending_job());
        self.retry_every = RETRY_STEADY;
        self.retry_at = Instant::now() + self.retry_every;
        match self.writer.run(jobs) {
            Ok(()) => self.ev.emit(
                Level::Warn,
                "fallback",
                &format!("reason={} dir={} pending={}", q(why), self.paths.fallback.display(), self.pending.len()),
            ),
            Err(e) => self.ev.emit(Level::Error, "fallback.failed", &format!("reason={} error={}", q(why), q(&e))),
        }
    }

    /// Writes the pending keys and the whole journal to the stick; on success the fallback is cleared.
    fn push_pending(&mut self, why: &str) -> bool {
        let mut jobs: Vec<Job> = Vec::new();
        let groups: BTreeSet<String> =
            self.pending.iter().filter(|k| !self.is_kernel(k)).map(|k| crate::key::group(k).to_string()).collect();
        let saved = std::mem::replace(&mut self.place, Place::Stick);
        for g in &groups {
            jobs.push(self.group_job(g));
        }
        for k in self.pending.iter().filter(|k| self.is_kernel(k)) {
            jobs.push(self.job_for(k));
        }
        jobs.push(Job::Write(self.paths.dir(&Place::Stick).join("journal.log"), self.journal.text().into_bytes()));
        match self.writer.run(jobs) {
            Ok(()) => {
                let n = self.pending.len();
                self.pending.clear();
                for f in ["PENDING", "CONFIG.TXT", "journal.log"] {
                    let _ = std::fs::remove_file(self.paths.fallback.join(f));
                }
                for g in &groups {
                    let _ = std::fs::remove_file(self.paths.fallback.join(format!("{g}.toml")));
                }
                self.ev.emit(Level::Info, "resync", &format!("when={why} keys={n}"));
                true
            }
            Err(e) => {
                self.place = saved;
                if matches!(self.place, Place::Stick) {
                    self.move_to_fallback(&e);
                } else {
                    // The stick write may still hang in the old worker; the fallback gets a fresh one.
                    self.replace_writer();
                }
                self.ev.emit(Level::Warn, "resync.failed", &format!("when={why} error={}", q(&e)));
                false
            }
        }
    }

    /// Tries the stick at once (tests, and a later "stick plugged in" hint).
    pub fn tick_now(&mut self) -> bool {
        self.retry_at = Instant::now();
        self.tick()
    }

    fn back_off(&mut self) {
        self.retry_every = (self.retry_every * 2).min(RETRY_MAX);
        self.retry_at = Instant::now() + self.retry_every;
    }

    /// Called about every second: on the fallback, tries the stick again (backing off up to 5 minutes).
    /// Every stick access is bounded by the deadline. Returns whether it tried (the state may have changed).
    pub fn tick(&mut self) -> bool {
        if self.marker_due.is_some() && !self.still_hung() {
            self.write_marker();
        }
        if !matches!(self.place, Place::Fallback(_)) || Instant::now() < self.retry_at {
            return false;
        }
        if self.still_hung() {
            // The abandoned write has not returned yet: touching the stick now could only hang another
            // thread, and its late write must land before the resync reads the stick.
            self.ev.emit(Level::Warn, "resync.waiting", &format!("stuck={}", self.stuck.len()));
            self.back_off();
            return false;
        }
        let raw = match self.read_stick(false) {
            Ok((raw, _)) => raw,
            Err(e) => {
                if e.starts_with(store::SLOW) {
                    self.ev.emit(Level::Warn, "resync.failed", &format!("when=probe error={}", q(&e)));
                    self.back_off();
                } else {
                    if self.retry_every < RETRY_STEADY {
                        self.retry_every = (self.retry_every * 2).min(RETRY_STEADY);
                    }
                    self.retry_at = Instant::now() + self.retry_every;
                }
                return true;
            }
        };
        let before: BTreeMap<String, Value> =
            self.defs.keys().filter_map(|k| Some((k.clone(), self.effective(k)?))).collect();
        let mem = (self.values.clone(), self.orphans.clone(), std::mem::take(&mut self.journal));
        let base = self.parse_place(raw);
        self.take(&base);
        self.overlay(&self.pending.clone(), &mem.0, &mem.1);
        let last = base.journal.entries.last().map_or(0, |e| e.seq);
        self.journal = base.journal;
        for e in mem.2.entries.iter().filter(|e| e.seq > last) {
            self.journal.push_loaded(e.clone());
        }
        if self.push_pending("stick back") {
            self.retry_every = RETRY_STEADY;
            // Values Georg changed on the stick meanwhile reach the watchers too.
            for (k, old) in before {
                if let Some(v) = self.effective(&k)
                    && v != old
                {
                    let line = crate::json::Json::obj()
                        .with("event", "changed")
                        .with("key", k.as_str())
                        .with("value", v.to_json())
                        .with("op", "resync")
                        .to_text();
                    self.notify(&k, &line);
                }
            }
        } else {
            (self.values, self.orphans, self.journal) = mem;
            self.back_off();
        }
        true
    }
}
