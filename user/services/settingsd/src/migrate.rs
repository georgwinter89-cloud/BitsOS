//! One-time changes of stored values when a default changes meaning. The marker file `settings/MIGRATED` on
//! the stick lists the migrations done (one name per line), so a value Georg sets afterwards stays as he set it.
//!
//! Renamed keys (CL-5s) need no marker: the stored old name is the mark. Its value moves to the new key, the
//! old name leaves the files and the journal, and the next start finds nothing to do.

use crate::change::Who;
use crate::events::{Level, q};
use crate::registry::Registry;
use crate::store::{self, Job, Place};
use crate::value::Value;
use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

/// Old name -> new name (V2-LANES row CL-5: `claude.*` became `ai.*`). The other names of that row never were
/// settings here: `claude_api_key` is a line of CONFIG.TXT that R8 keeps settingsd away from (`guard.rs`), and
/// ctxd's `context.claude.*`/`context.strata.*` are options of its own file.
const RENAMED: &[(&str, &str)] = &[("claude.name", "ai.assistant.name")];

/// How often a refused marker write is tried again.
const MARKER_RETRY: Duration = Duration::from_secs(2);

/// FS-13b: kernel.preload.max_mb's default became `all`; a stored 1024 was the old default, so it goes.
const PRELOAD_ALL: &str = "preload_all";
/// EINS: the one level of the assistant (`one_level`).
const EINS: &str = "eins_level";
/// DECISIONS 18.11. (everything built is on): the defaults that became the switched-on value (`defaults_on`).
const DEFAULTS_ON: &str = "defaults_on";

/// The default a key had before 18.11., for the keys whose default became the switched-on value. A stored value
/// equal to it goes once, so the key takes the new default; any other stored value is Georg's and stays (an
/// `ai.level` of `answer` is a tightening he chose).
fn old_default(key: &str) -> Option<Value> {
    match key {
        crate::leash::LEVEL => Some(Value::Text("ask".into())),
        "ai.mcp.ask_tools" | "assistant.sidebar_sees" | "leistung.nachts" => Some(Value::Bool(false)),
        _ if key.strip_prefix("claude.actions.").is_some_and(|t| !t.is_empty() && !t.contains('.')) => Some(Value::Text("deny".into())),
        _ => None,
    }
}

/// The keys 18.11. removed: the effect level (the effects are always the full ones) and ctxd's fixed file cap
/// (no fixed limits). Their stored values leave the files and the journal.
const REMOVED_ON: &[&str] = &["theme.effects", "context.index.max_files"];

/// The keys EINS removed (plan/spec/assistent-eins K8): their stored values go once the level is migrated.
pub fn removed_by_eins(key: &str) -> bool {
    let level_key = |prefix: &str| key.strip_prefix(prefix).and_then(|k| k.strip_suffix(".allow")).is_some_and(|k| !k.is_empty());
    level_key("ai.tools.")
        || level_key("ai.helper.")
        || matches!(
            key,
            "ai.role.explain"
                | "ai.role.explain_fallback"
                | "ai.role.search"
                | "ai.role.search_fallback"
                | "ai.role.jobs"
                | "ai.role.jobs_fallback"
                | "durchblick.handeln.unumkehrbar_fragen"
                | "durchblick.antwort"
        )
        || ["durchblick.vorschau.", "durchblick.bild.auto.", "durchblick.handeln.auto."].iter().any(|p| key.starts_with(p))
}

impl Registry {
    /// Moves what is stored under an old key name to the new key and drops the old name. A value Georg
    /// already gave the new key stays. Runs wherever the values came from (the old name then leaves the
    /// stick at the resync); a move that fails is tried again at the next load.
    pub(crate) fn rename_old_keys(&mut self) {
        for (old, new) in RENAMED {
            let (Some(raw), Some(d)) = (self.orphans.get(*old).cloned(), self.defs.get(*new).cloned()) else { continue };
            let kept = self.values.contains_key(*new);
            if !kept {
                let who = Who { source: "settingsd-migrate", confirmed: true, asker: None };
                let moved = self.coerce(&d, &raw).map_err(|e| e.to_string()).and_then(|v| self.change(&d, v, &who, "migrate", None).map_err(|e| e.msg));
                if let Err(e) = moved {
                    self.ev.emit(Level::Warn, "migrate.failed", &format!("key={old} to={new} error={}", q(&e)));
                    continue;
                }
            }
            self.orphans.remove(*old);
            let gone = BTreeSet::from([old.to_string()]);
            if matches!(self.place, Place::Fallback(_)) {
                self.pending.insert(old.to_string());
                if let Err(e) = self.writer.run(vec![self.pending_job()]) {
                    self.ev.emit(Level::Warn, "store.failed", &format!("what=pending error={}", q(&e)));
                }
            }
            if self.journal.forget(&HashSet::from([old.to_string()])) {
                self.rewrite_journal();
            }
            self.rewrite_groups(&gone);
            self.ev.emit(Level::Info, "migrated", &format!("key={old} to={new} new_key_was_set={kept}"));
        }
    }

    /// Runs the migrations not done yet. Only on the stick (the marker lives there); on the fallback they wait
    /// for a later start.
    pub(crate) fn migrate(&mut self) {
        if !matches!(self.place, Place::Stick) {
            return;
        }
        let marker = self.paths.dir(&Place::Stick).join("MIGRATED");
        let m2 = marker.clone();
        let done = match self.writer.call(move || store::read_text(&m2)) {
            Ok(Ok(t)) => t,
            Ok(Err(_)) => String::new(),
            Err(e) => {
                self.ev.emit(Level::Warn, "migrate.unreadable", &format!("error={}", q(&e)));
                return;
            }
        };
        let is_done = |name: &str| done.lines().any(|l| l.trim() == name);
        let mut now_done: Vec<&str> = Vec::new();
        if !is_done(PRELOAD_ALL) && self.preload_all() {
            now_done.push(PRELOAD_ALL);
        }
        if !is_done(EINS) && self.one_level() {
            now_done.push(EINS);
        }
        if !is_done(DEFAULTS_ON) && self.defaults_on() {
            now_done.push(DEFAULTS_ON);
        }
        if now_done.is_empty() {
            return;
        }
        let mut text = done;
        for name in now_done {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(name);
            text.push('\n');
        }
        self.marker_due = Some(Marker { path: marker, text: text.into_bytes(), at: Instant::now(), tries: 0 });
        self.write_marker();
    }

    /// FS-13b. False: it failed and is tried again at the next start.
    fn preload_all(&mut self) -> bool {
        let key = "kernel.preload.max_mb";
        if let Some(d) = self.defs.get(key).cloned()
            && self.values.get(key) == Some(&Value::Int(1024))
        {
            let who = Who { source: "settingsd-migrate", confirmed: true, asker: None };
            match self.change(&d, None, &who, "migrate", None) {
                Ok(_) => self.ev.emit(Level::Info, "migrated", &format!("key={key} from=1024 to={}", d.default.show())),
                Err(e) => {
                    self.ev.emit(Level::Warn, "migrate.failed", &format!("key={key} error={}", q(&e.msg)));
                    return false;
                }
            }
        }
        true
    }

    /// EINS (plan/spec/assistent-eins K8): the one level `ai.level` took the place of the per-tool and per-kind
    /// levels and of Durchblick's per-program Automodus. Its default is `auto` since 18.11., which is what the old
    /// levels at their most open gave, so only the values of the keys EINS removed are left to go, from the files
    /// and the journal; a level already set stays. Always true: nothing here can fail.
    fn one_level(&mut self) -> bool {
        let all: Vec<String> = self.values.keys().chain(self.orphans.keys()).cloned().collect();
        let gone: BTreeSet<String> = all.into_iter().filter(|k| removed_by_eins(k) && !self.defs.contains_key(k)).collect();
        self.drop_stored(&gone);
        let kept = self.values.contains_key(crate::leash::LEVEL);
        self.ev.emit(Level::Info, "level.migrated", &format!("level_set={kept} removed_keys={}", gone.len()));
        true
    }

    /// DECISIONS 18.11. (everything built is on): a stored old default (`old_default`) goes, so the key takes its
    /// new default; the values of the keys 18.11. removed go too. A key registered by its service later (ctxd,
    /// bitsbench, bitscomp, the action servers) is still an orphan here: its line leaves the group file. False:
    /// a change failed and the whole step is tried again at the next start.
    fn defaults_on(&mut self) -> bool {
        let who = Who { source: "settingsd-migrate", confirmed: true, asker: None };
        let all: Vec<String> = self.values.keys().chain(self.orphans.keys()).cloned().collect();
        let same = |v: Option<&Value>, old: &Value| v.is_some_and(|v| v == old || v.show() == old.show());
        let mut ok = true;
        let mut orphans_gone = BTreeSet::new();
        for k in &all {
            let Some(old) = old_default(k) else { continue };
            let to = |r: &Registry| r.defs.get(k).map_or_else(|| "its-new-default".to_string(), |d| d.default.show());
            if let Some(d) = self.defs.get(k).cloned()
                && same(self.values.get(k), &old)
            {
                match self.change(&d, None, &who, "migrate", None) {
                    Ok(_) => self.ev.emit(Level::Info, "migrated", &format!("key={k} from={} to={}", old.show(), to(self))),
                    Err(e) => {
                        self.ev.emit(Level::Warn, "migrate.failed", &format!("key={k} error={}", q(&e.msg)));
                        ok = false;
                    }
                }
            } else if same(self.orphans.get(k), &old) {
                self.orphans.remove(k);
                orphans_gone.insert(k.clone());
                self.ev.emit(Level::Info, "migrated", &format!("key={k} from={} to={}", old.show(), to(self)));
            }
        }
        if !orphans_gone.is_empty() {
            self.rewrite_groups(&orphans_gone);
        }
        let gone: BTreeSet<String> = all.into_iter().filter(|k| REMOVED_ON.contains(&k.as_str()) && !self.defs.contains_key(k)).collect();
        for k in &gone {
            self.ev.emit(Level::Info, "migrated", &format!("key={k} removed=yes"));
        }
        self.drop_stored(&gone);
        ok
    }

    /// Drops the stored values and the journal entries of keys that are no longer defined.
    fn drop_stored(&mut self, gone: &BTreeSet<String>) {
        if gone.is_empty() {
            return;
        }
        for k in gone {
            self.orphans.remove(k);
            self.values.remove(k);
        }
        if self.journal.forget(&gone.iter().cloned().collect()) {
            self.rewrite_journal();
        }
        self.rewrite_groups(gone);
    }

    /// Writes a due migration marker. At boot settingsd starts before the stick is writable (`fat stick.decided`
    /// about a second later): a refused write is tried again from `tick`, 2 s later, doubling up to a minute.
    pub(crate) fn write_marker(&mut self) {
        let Some(mut m) = self.marker_due.take() else { return };
        if Instant::now() < m.at {
            self.marker_due = Some(m);
            return;
        }
        match self.writer.run(vec![Job::Write(m.path.clone(), m.text.clone())]) {
            Ok(()) => self.ev.emit(Level::Info, "migrate.marked", &format!("file={} tries={}", m.path.display(), m.tries + 1)),
            Err(e) => {
                if e.starts_with(store::SLOW) {
                    self.replace_writer();
                }
                if m.tries == 0 {
                    self.ev.emit(Level::Info, "migrate.marker_later", &format!("error={}", q(&e)));
                }
                let gap = MARKER_RETRY.saturating_mul(1 << m.tries.min(5)).min(Duration::from_secs(60));
                m.tries += 1;
                m.at = Instant::now() + gap;
                self.marker_due = Some(m);
            }
        }
    }
}

/// A migration marker still to be written.
pub(crate) struct Marker {
    pub(crate) path: std::path::PathBuf,
    pub(crate) text: Vec<u8>,
    pub(crate) at: Instant,
    pub(crate) tries: u32,
}
