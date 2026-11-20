//! CL-5: the settings page "KI" (`einstellungen/ki`) in bitscomp: a page window of kind `Ki` whose generic part
//! (window, scroll, field, dialog, image) is page_app's and whose rows are `ki`'s, drawn by the settings page's
//! own code. Every change goes to settingsd through the page's writer (its journal keeps undo and history) and
//! shows only once settingsd's watch reports it; "Testen" runs `assist --test` on a thread (`ki_probe`).
//! A provider's key: typed into a field that draws nothing of it, sent as a confirmed change with settingsd's
//! `while` on address, kind and place as the page showed them, never logged, never in `ctl page`. Typing or
//! deleting a key and removing a provider need Georg's real keyboard or mouse (`ki_ask`), and while he types a
//! key injected input reaches nothing (`ki_guard`). This holds on bitscomp's side only: settingsd still takes
//! `"confirmed": true` from any program of the session, so "confirmed" means
//! that he confirmed only once settingsd counts bitscomp's `confirmed` alone.

use std::collections::BTreeMap;

use mcpreg::Json;

use crate::ki::{self, Built, Form, Test};
use crate::ki_ask::{Asking, What};
use crate::ki_guard::Typing;
use crate::ki_probe::{End, Msg};
use crate::page::{Kind, Part};
use crate::page_draw::Tone;
use crate::page_kind::PageKind;
use crate::settings_write::{DONE_RESET, Done, Hidden, Req, wipe};
use crate::state::App;

pub use crate::ki_ask::REAL_INPUT;

/// An add whose answer is this late is given up (the writer answers each request within 5 s): the form is free
/// again.
const ADD_STALE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    List,
    Kind,
    Path,
    /// A helper that says where it sends the conversation.
    Place,
}

/// A provider on its way in: the name goes on the list first (settingsd then makes its keys), then its kind,
/// then a helper's path; each only after settingsd took the one before.
#[derive(Clone, Debug, PartialEq)]
struct Adding {
    pid: u32,
    name: String,
    kind: String,
    path: String,
    /// From a helper's description: its place, and its name for the page's line.
    place: Option<String>,
    helper: Option<String>,
    step: Step,
    /// When its newest step went to settingsd (`pacer` time).
    since: std::time::Duration,
}

impl Adding {
    fn key(&self) -> String {
        match self.step {
            Step::List => ki::LIST.into(),
            Step::Kind => ki::key_of(&self.name, "kind"),
            Step::Path => ki::key_of(&self.name, "address"),
            Step::Place => ki::key_of(&self.name, "place"),
        }
    }
}

#[derive(Default)]
pub struct Ki {
    pub built: Built,
    pub form: Form,
    pub tests: BTreeMap<String, Test>,
    /// The newest test of each provider: an answer of an older one is dropped.
    runs: BTreeMap<String, u64>,
    next_run: u64,
    adding: Option<Adding>,
    /// The open question of each page "KI" (each page has its own dialog).
    pub(crate) asking: BTreeMap<u32, Asking>,
    /// The removal on its way: the page that asked and the provider.
    pub(crate) removing: Option<(u32, String)>,
    /// The key being typed (`ki_guard`).
    pub(crate) typing: Option<Typing>,
    /// A key's field lost the keyboard without Georg ending it: his keys go nowhere until his next click.
    pub(crate) swallow: bool,
    /// Host tests have no window: the page whose field has the keyboard.
    #[cfg(test)]
    pub(crate) keys_at: Option<u32>,
    pub prober: Option<crate::ki_probe::Prober>,
    /// The helper programs described on this machine (`ki_helpers`).
    pub helpers: Vec<crate::helpers::Helper>,
    /// A kind or address on its way to settingsd: (shown before, shown after), for the line that says how
    /// "Rückgängig" brings the old one back.
    pub(crate) before: BTreeMap<String, (String, String)>,
}

impl App {
    fn ki_pages(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.pages.by_pid.iter().filter(|(_, s)| s.kind == PageKind::Ki).map(|(p, _)| *p).collect();
        v.sort();
        v
    }

    /// Builds the page anew from what settingsd said last. An open field follows its row; a row that is gone
    /// ends the edit (nothing is written).
    pub(crate) fn ki_refresh(&mut self) {
        let f = ki::facts(&self.settings);
        self.ki.tests.retain(|id, _| f.provider(id).is_some());
        let new = ki::build(&f, &self.ki.form, &self.ki.tests, &self.page_model, &self.ki.helpers);
        if new == self.ki.built {
            return;
        }
        let mut ended = false;
        for s in self.pages.by_pid.values_mut().filter(|s| s.kind == PageKind::Ki) {
            let Some((Part::Field(r), _)) = s.edit.as_ref() else { continue };
            let key = self.ki.built.model.rows.get(*r).map(|r| r.key.as_str());
            let to = key.and_then(|k| new.row_of(k)).filter(|n| matches!(new.model.rows[*n].kind, Kind::Field { .. } | Kind::Secret { .. }));
            match (to, s.edit.as_mut()) {
                (Some(n), Some((part, _))) => *part = Part::Field(n),
                _ => {
                    if let Some((_, e)) = s.edit.take() {
                        wipe(e.text);
                        ended = true;
                    }
                }
            }
        }
        if ended {
            eprintln!("bitscomp: ki: a row went away under an open field; edit ended, nothing written");
            self.desk.held = None;
        }
        self.ki.built = new;
        self.needs_frame = true;
    }

    /// A value of settingsd changed (or whether one is stored).
    pub fn ki_setting(&mut self, key: &str) {
        if !key.starts_with("ai.") {
            return;
        }
        // A test's line was about the provider as it was.
        if let Some((id, _)) = ki::provider_of(key)
            && self.ki.tests.get(id).is_some_and(|t| *t != Test::Running)
        {
            self.ki.tests.remove(id);
        }
        self.ki_refresh();
    }

    /// Sends `r`; true when it is on its way.
    pub(crate) fn ki_send(&mut self, pid: u32, r: Req) -> bool {
        let before = self.pages.busy;
        self.page_send(pid, r);
        self.pages.busy > before
    }

    pub(crate) fn ki_act(&mut self, pid: u32, part: Part) {
        let (r, o) = match part {
            Part::Undo => return self.page_send(pid, Req::UNDO),
            Part::DialogCancel => return self.page_dialog_close(pid, false),
            Part::DialogOk => return self.page_dialog_close(pid, true),
            Part::Pill(r, o) | Part::Button(r, o) => (r, o),
            Part::Field(r) | Part::Switch(r) => (r, 0),
            _ => return,
        };
        // A part of a row that went away with a new list does nothing.
        let Some(row) = self.ki.built.model.rows.get(r).cloned() else { return };
        let now = self.ki.built.values.get(r).cloned().unwrap_or_default();
        let id = ki::provider_of(&row.key).map(|(id, _)| id.to_string());
        match (part, &row.kind) {
            (Part::Pill(..), Kind::Pills { opts, multi, .. }) => {
                let Some((value, _)) = opts.get(o) else { return };
                if row.key == ki::ADD_KIND {
                    self.ki.form.kind = value.clone();
                    return self.ki_refresh();
                }
                if *multi {
                    self.page_send(pid, Req::set(&row.key, Json::str(&ki::toggled(&now, value))));
                } else if *value != now {
                    self.ki_change(pid, &row.key, &now, value);
                }
            }
            (Part::Switch(_), Kind::Switch) => self.page_send(pid, Req::set(&row.key, Json::Bool(now != "true"))),
            (Part::Field(_), Kind::Field { .. }) => self.page_edit_begin(pid, part, now),
            (Part::Field(_), Kind::Secret { .. }) => self.ki_key_begin(pid, &row.key),
            (Part::Button(..), Kind::Secret { clear: true }) => {
                if let Some(id) = id {
                    self.ki_ask(pid, What::Clear(id));
                }
            }
            (Part::Button(..), Kind::Buttons(b)) => match (b.get(o).map(|b| b.0.as_str()), id) {
                (Some("add"), _) => self.ki_add(pid),
                (Some(h @ ("adopt" | "chat" | "rescan")), _) => self.ki_helper_act(pid, &row.key, h),
                (Some("test"), Some(id)) => self.ki_test(pid, &id),
                (Some("remove"), Some(id)) => {
                    let list = self.settings.text(ki::LIST).unwrap_or("").to_string();
                    self.ki_ask(pid, What::Remove { id, list });
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// The field being edited on `pid` is a key's.
    pub(crate) fn ki_secret_edit(&self, pid: u32) -> bool {
        let Some((Part::Field(r), _)) = self.pages.by_pid.get(&pid).and_then(|s| s.edit.as_ref()) else { return false };
        matches!(self.ki.built.model.rows.get(*r).map(|r| &r.kind), Some(Kind::Secret { .. }))
    }

    /// A key for the page's field: true when it must not reach a key's field (injected input types no key);
    /// the edit ends.
    pub(crate) fn ki_key_guard(&mut self, pid: u32) -> bool {
        if self.pages.real || !self.ki_secret_edit(pid) {
            return false;
        }
        self.ki.typing = None;
        eprintln!("bitscomp: ki: an injected key reached a key's field: edit ended, nothing stored");
        self.page_edit_end();
        self.page_say(pid, REAL_INPUT, true);
        true
    }

    /// A field's text changed: the form's own fields keep what is typed (a click on "Hinzufügen" ends the edit).
    pub(crate) fn ki_typed(&mut self, pid: u32) {
        let Some((Part::Field(r), e)) = self.pages.by_pid.get(&pid).and_then(|s| s.edit.as_ref()) else { return };
        match self.ki.built.model.rows.get(*r).map(|r| r.key.as_str()) {
            Some(ki::ADD_NAME) => self.ki.form.name = e.text.clone(),
            Some(ki::ADD_PATH) => self.ki.form.path = e.text.clone(),
            _ => return,
        }
        self.ki_refresh();
    }

    /// Enter in row `r`'s field.
    pub(crate) fn ki_commit(&mut self, pid: u32, r: usize, text: String) {
        let Some(row) = self.ki.built.model.rows.get(r).cloned() else { return wipe(text) };
        match &row.kind {
            Kind::Secret { .. } => self.ki_key_commit(pid, &row.key, text),
            Kind::Field { max, .. } => {
                let t = text.trim();
                match row.key.as_str() {
                    ki::ADD_NAME => self.ki.form.name = t.into(),
                    ki::ADD_PATH => self.ki.form.path = t.into(),
                    _ if t.len() > *max => {
                        return self.page_say(pid, format!("{}: zu lang, höchstens {max} Zeichen (ä, ö, ü und ß zählen doppelt). Bitte kürzen.", row.short), true);
                    }
                    _ => {
                        let now = self.ki.built.values.get(r).cloned().unwrap_or_default();
                        return self.ki_change(pid, &row.key, &now, t);
                    }
                }
                self.ki_refresh();
            }
            _ => {}
        }
    }

    fn ki_key_commit(&mut self, pid: u32, key: &str, text: String) {
        let typing = self.ki.typing.take();
        let value = Hidden::new(text.trim().to_string());
        wipe(text);
        let (Some((id, _)), Some(Typing { id: typed_for, whilst, .. })) = (ki::provider_of(key), typing) else { return };
        if typed_for != id {
            return;
        }
        if value.is_empty() {
            return self.page_say(pid, format!("Nichts eingetippt: am Schlüssel von „{id}“ wurde nichts geändert."), false);
        }
        if !self.ki_real(pid, "storing a key") {
            return;
        }
        eprintln!("bitscomp: ki: a key for provider {id} goes to settingsd (confirmed, bound to address, kind and place)");
        self.page_send(pid, Req::Secret { key: key.into(), value, whilst });
    }

    fn ki_add(&mut self, pid: u32) {
        let f = ki::facts(&self.settings);
        let (name, kind, path) = (self.ki.form.name.trim().to_string(), self.ki.form.kind.clone(), self.ki.form.path.trim().to_string());
        self.ki_add_stale();
        let no = if self.ki.adding.is_some() {
            Some("Der Anbieter wird gerade angelegt …".to_string())
        } else if let Err(why) = ki::check_name(&name, &f) {
            Some(why)
        } else if kind.is_empty() {
            Some("Erst die Art des Anbieters wählen.".into())
        } else if kind == ki::PROGRAM && path.is_empty() {
            Some("Erst den Pfad des Hilfsprogramms eintippen.".into())
        } else {
            None
        };
        if let Some(why) = no {
            return self.page_say(pid, why, true);
        }
        self.ki_add_start(pid, name, kind, path, None, None);
    }

    /// An add whose answer is overdue is given up: the form is free again.
    fn ki_add_stale(&mut self) {
        let now = crate::pacer::now();
        if self.ki.adding.as_ref().is_some_and(|a| now.saturating_sub(a.since) >= ADD_STALE) {
            eprintln!("bitscomp: ki: an add got no answer from settingsd for {} s: given up", ADD_STALE.as_secs());
            self.ki.adding = None;
        }
    }

    /// Puts provider `name` on the list; its kind, path and place follow step by step (`ki_added`).
    pub(crate) fn ki_add_start(&mut self, pid: u32, name: String, kind: String, path: String, place: Option<String>, helper: Option<String>) {
        self.ki_add_stale();
        if self.ki.adding.is_some() {
            return self.page_say(pid, "Der Anbieter wird gerade angelegt …", true);
        }
        let list = ki::list_with(self.settings.text(ki::LIST).unwrap_or(""), &name);
        if self.ki_send(pid, Req::set(ki::LIST, Json::str(&list))) {
            self.ki.adding = Some(Adding { pid, name, kind, path, place, helper, step: Step::List, since: crate::pacer::now() });
        }
    }

    /// settingsd answered a step of an add: the next one, or the page's line.
    fn ki_added(&mut self, mut a: Adding, d: &Done) {
        self.ki.adding = None;
        if !d.ok {
            let text = match a.step {
                Step::List => format!("Anbieter „{}“ nicht hinzugefügt: {}", a.name, d.text),
                _ => format!("Anbieter „{}“ ist angelegt, aber noch nicht ganz eingetragen: {}", a.name, d.text),
            };
            return self.page_say(d.pid, text, true);
        }
        let place = a.place.clone().map(|p| (Step::Place, p));
        let next = match a.step {
            Step::List => Some((Step::Kind, a.kind.clone())),
            Step::Kind if a.kind == ki::PROGRAM => Some((Step::Path, a.path.clone())),
            Step::Path => place,
            _ => None,
        };
        if let Some((step, value)) = next {
            a.step = step;
            a.since = crate::pacer::now();
            if self.ki_send(a.pid, Req::set(&a.key(), Json::str(&value))) {
                self.ki.adding = Some(a);
            }
            return;
        }
        if let Some(helper) = a.helper.clone() {
            self.ki_refresh();
            return self.page_say(d.pid, crate::ki_change::adopted_line(&helper, &a.name, a.place.is_some(), &ki::facts(&self.settings)), false);
        }
        self.ki.form = Form::default();
        self.ki_refresh();
        let rest = if a.kind == ki::PROGRAM { "den Ort" } else { "Ort, Adresse und Modell" };
        self.page_say(d.pid, format!("Anbieter „{}“ hinzugefügt. Trag jetzt in seinem Abschnitt {rest} ein.", a.name), false);
    }

    /// A page "KI" that is gone still waits for settingsd's answer to `pid`'s add or removal: the answer must
    /// reach `ki_done`, or the form stays "wird gerade angelegt" for good.
    pub(crate) fn ki_waits(&self, pid: u32) -> bool {
        self.ki.adding.as_ref().is_some_and(|a| a.pid == pid) || self.ki.removing.as_ref().is_some_and(|(p, _)| *p == pid)
    }

    /// settingsd answered a change of a page "KI" (the page may have closed meanwhile: an add still goes on to
    /// its end, a line for a page that is gone is said to nobody).
    pub(crate) fn ki_done(&mut self, d: Done) {
        let key = d.key.clone().unwrap_or_default();
        if let Some(a) = self.ki.adding.clone().filter(|a| a.pid == d.pid && a.key() == key) {
            return self.ki_added(a, &d);
        }
        if let Some(text) = self.ki_changed(&key, &d) {
            return self.page_say(d.pid, text, !d.ok);
        }
        let removed = if key == ki::LIST { self.ki.removing.take_if(|(p, _)| *p == d.pid) } else { None };
        let text = if let Some((_, id)) = removed {
            if d.ok { format!("Anbieter „{id}“ entfernt.") } else { format!("Anbieter „{id}“ nicht entfernt: {}", d.text) }
        } else if let Some((id, "key")) = ki::provider_of(&key) {
            match (d.ok, d.text.as_str()) {
                (true, DONE_RESET) => format!("Schlüssel von „{id}“ gelöscht."),
                (true, _) => format!("Schlüssel für „{id}“ gespeichert."),
                (false, _) => format!("Schlüssel von „{id}“: {}", d.text),
            }
        } else if d.key.is_none() || (!d.ok && d.text.starts_with(&key)) {
            // An undo names its own key; a refusal that begins with the key is settingsd's own sentence (a
            // stored key binds address, kind and place) and is shown as it is.
            d.text.clone()
        } else {
            let short = self.ki.built.model.rows.iter().find(|r| r.key == key).map_or(key.clone(), |r| r.short.clone());
            format!("{short}: {}", d.text)
        };
        self.page_say(d.pid, text, !d.ok);
    }

    fn ki_test(&mut self, pid: u32, id: &str) {
        if self.ki.tests.get(id) == Some(&Test::Running) {
            return self.page_say(pid, format!("Der Test von „{id}“ läuft schon."), false);
        }
        self.ki.next_run += 1;
        let run = self.ki.next_run;
        let started = match self.ki.prober.as_ref() {
            Some(p) => p.send(id, run),
            None => Err("Hier lässt sich kein Test starten.".to_string()),
        };
        let t = match started {
            Ok(()) => {
                self.ki.runs.insert(id.to_string(), run);
                Test::Running
            }
            Err(why) => Test::Failed(why),
        };
        self.ki.tests.insert(id.to_string(), t);
        self.page_say(pid, ki::test_line(self.ki.tests.get(id)).0, false);
        self.ki_refresh();
    }

    /// A test ended (its thread), or its deadline passed (the main loop's timer).
    pub fn ki_probe_msg(&mut self, m: Msg) {
        // An older run's answer, or the deadline of a test that answered in time.
        if self.ki.runs.get(&m.id) != Some(&m.run) || self.ki.tests.get(&m.id) != Some(&Test::Running) {
            return;
        }
        let t = match m.end {
            End::Exit(code, line) => Test::Done { code, line },
            End::Late => Test::Late,
            End::Failed(why) => Test::Failed(why),
        };
        let (line, tone) = ki::test_line(Some(&t));
        self.ki.tests.insert(m.id.clone(), t);
        self.ki_refresh();
        for pid in self.ki_pages() {
            self.page_say(pid, format!("Test von „{}“: {line}", m.id), tone == Tone::Bad);
        }
    }

    /// What `ctl page` adds for a page "KI": the providers, whether each has a key (never the key), the tests.
    pub(crate) fn ki_report(&self) -> String {
        let f = ki::facts(&self.settings);
        let list = |v: Vec<String>| if v.is_empty() { "-".to_string() } else { v.join(",") };
        let providers = list(f.providers.iter().map(|p| p.id.clone()).collect());
        let keys = list(f.providers.iter().map(|p| format!("{}:{}", p.id, if p.stored { ki::STORED } else { ki::MISSING })).collect());
        let tests = list(
            self.ki
                .tests
                .iter()
                .map(|(id, t)| match t {
                    Test::Running => format!("{id}:running"),
                    Test::Done { code, line } => format!("{id}:exit={}:{line:?}", code.map_or("signal".to_string(), |c| c.to_string())),
                    Test::Late => format!("{id}:late"),
                    Test::Failed(why) => format!("{id}:failed:{why:?}"),
                })
                .collect(),
        );
        let adding = self.ki.adding.as_ref().map_or("-".to_string(), |a| a.name.clone());
        let helpers = list(self.ki.helpers.iter().map(|h| h.id.clone()).collect());
        format!(
            " providers={providers} keys={keys} tests={tests} form={:?},{:?},{:?} adding={adding} helpers={helpers}",
            self.ki.form.name, self.ki.form.kind, self.ki.form.path
        )
    }
}

#[cfg(test)]
#[path = "ki_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ki_guard_tests.rs"]
mod guard_tests;

#[cfg(test)]
#[path = "ki_page_tests.rs"]
mod page_tests;

#[cfg(test)]
#[path = "ki_paste_tests.rs"]
mod paste_tests;
