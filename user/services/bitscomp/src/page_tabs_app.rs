//! WY-SET: the settings window's tabs in the app: each settings page keeps its view (`page_view`), rebuilt when
//! the registry's rows, a value, the tab, the search or a "Mehr" change; the rail's and the head's clicks; the
//! consolidated rows' writes (several keys, one question first when one of them asks: a Confirm key, or more
//! freedom for the assistant, settingsd's leash); "Zurücksetzen".

use std::collections::HashSet;

use mcpreg::Json;

use crate::layout::Rect;
use crate::page::{self, Inputs, Kind, Layout, Part, Row};
use crate::page_app::value_of;
use crate::page_join::{self, Provider};
use crate::page_kind::PageKind;
use crate::page_tabs::{self, FOLD};
use crate::settings_write::{Ask, Req};
use crate::state::App;

fn place_name(p: &str) -> &'static str {
    match p {
        "cloud" => "in der Cloud",
        "local" => "auf diesem Rechner",
        _ => "Ort nicht gesetzt",
    }
}

impl App {
    /// The programs the never-see list offers: the running windows' and those already on it (board 4: "+ Programm
    /// hinzufügen" picks from them).
    pub(crate) fn page_programs(&self) -> Vec<String> {
        let mut v: Vec<String> = self.ui_drawn().iter().map(|p| crate::ctl::names(&p.win).0).filter(|a| !a.is_empty() && !a.starts_with("bitscomp")).collect();
        v.extend(self.ws.minimized.iter().map(|m| crate::ctl::names(&m.win).0));
        v.sort();
        v.dedup();
        v
    }

    /// The providers as their cards show them.
    pub(crate) fn page_providers(&self) -> Vec<Provider> {
        crate::ki::facts(&self.settings)
            .providers
            .iter()
            .map(|p| {
                let abo = p.kind == crate::ki::PROGRAM && (p.id == "abo" || p.address.rsplit('/').next().is_some_and(|f| f.contains("claude-abo")));
                let key = if p.stored { "Schlüssel gespeichert" } else if p.kind == crate::ki::PROGRAM { "ohne Schlüssel" } else { "Schlüssel fehlt" };
                let mut line = if abo { format!("{} · nimmt Bilder an", place_name(&p.place)) } else { format!("{} · {} · {key}", crate::ki::kind_name(&p.kind), place_name(&p.place)) };
                // Whether it answers: the assistant's own status, for the provider it answers with.
                match self.assist.status.as_ref().filter(|s| s.provider.as_deref() == Some(p.id.as_str())) {
                    Some(s) if s.ready => line.push_str(" · ● erreichbar"),
                    Some(_) => line.push_str(" · nicht erreichbar"),
                    None => {}
                }
                Provider { id: p.id.clone(), name: if abo { page_join::ABO.into() } else { p.id.clone() }, line }
            })
            .collect()
    }

    fn page_known(&self) -> HashSet<String> {
        self.setting_keys.iter().map(|(k, _)| k.clone()).collect()
    }

    /// Every settings page's view anew (the registry's rows or a value changed); an open field follows its key,
    /// a field whose key is gone ends without writing.
    pub fn page_views_refresh(&mut self) {
        let pids: Vec<u32> = self.pages.by_pid.iter().filter(|(_, s)| s.kind == PageKind::Settings).map(|(p, _)| *p).collect();
        for pid in pids {
            self.page_view_refresh(pid);
        }
    }

    pub fn page_view_refresh(&mut self, pid: u32) {
        // An address that names a key, not a tab, finds its tab once that key's row is there (the registry or
        // ctxd's learned rows came after the window): while the jump is armed, the tab follows and jumps anew.
        if let Some(s) = self.pages.by_pid.get_mut(&pid).filter(|s| s.kind == PageKind::Settings && s.jump && !s.tab_given)
            && let Some(t) = crate::page_view::tab_of_prefix(&self.page_model.rows, &s.prefix)
            && t != s.tab
        {
            eprintln!("bitscomp: settings page {pid}: {} found on tab {}", s.prefix, page_tabs::address(t));
            s.tab = t;
            s.scroll = 0;
            s.jumped = None;
        }
        let Some(s) = self.pages.by_pid.get(&pid).filter(|s| s.kind == PageKind::Settings) else { return };
        let known = self.page_known();
        let providers = self.page_providers();
        let programs = self.page_programs();
        let value = |r: &Row| value_of(self, r);
        let has = |k: &str| known.contains(k);
        let cx = crate::page_view::Ctx {
            tab: s.tab,
            query: &s.query,
            changed_only: s.changed_only,
            open: &self.pages.open,
            target: &s.prefix,
            value: &value,
            known: &has,
            providers: &providers,
            programs: &programs,
        };
        let view = crate::page_view::build(&self.page_model, &cx);
        let Some(s) = self.pages.by_pid.get_mut(&pid) else { return };
        if s.shown == view {
            return;
        }
        let old = std::mem::replace(&mut s.shown, view);
        let mut ended = false;
        if let Some((part, _)) = s.edit.as_mut() {
            let key_at = |m: &page::Model, r: usize| m.rows.get(r).map(|x| x.key.clone());
            let typed = |k: &Kind| matches!(k, Kind::Number { .. } | Kind::Field { .. } | Kind::Subs(_));
            let follow = |r: usize| {
                let key = key_at(&old.model, r)?;
                s.shown.model.rows.iter().position(|n| n.key == key && typed(&n.kind))
            };
            match *part {
                Part::Field(r) => match follow(r) {
                    Some(n) => *part = Part::Field(n),
                    None => ended = true,
                },
                Part::Sub(r, k) => match follow(r) {
                    Some(n) => *part = Part::Sub(n, k),
                    None => ended = true,
                },
                _ => {}
            }
        }
        if ended {
            s.edit = None;
            eprintln!("bitscomp: page: settings list changed under an open field; edit ended, nothing written");
            self.desk.held = None;
        }
        self.needs_frame = true;
    }

    /// The tab of a page opened at a key or namespace no tab word names (`einstellungen/kernel/snd_output`).
    pub(crate) fn page_tab_resolve(&mut self, pid: u32) {
        let Some(s) = self.pages.by_pid.get(&pid).filter(|s| s.kind == PageKind::Settings && !s.tab_given) else { return };
        let tab = crate::page_view::tab_of_prefix(&self.page_model.rows, &s.prefix).unwrap_or(0);
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.tab = tab;
        }
    }

    /// The settings page's layout: the rail on the left, the page right of it.
    pub(crate) fn page_layout_tabs(&self, pid: u32, content: Rect) -> Option<Layout> {
        let s = self.pages.by_pid.get(&pid).filter(|s| s.kind == PageKind::Settings)?;
        let rw = crate::page_rail::width(content);
        let main = Rect { x: content.x + rw, w: (content.w - rw).max(1), ..content };
        let mut lay = page::layout(main, &s.shown.model, &Inputs { favourites: self.desk.favourites.len(), scroll: s.scroll });
        let listing = !s.query.trim().is_empty() || s.changed_only;
        // The filter stands right of the line under the title (the title keeps its width).
        let head = Rect { x: lay.sub.x, y: lay.sub.y - 6, w: lay.sub.w, h: 30 };
        if listing {
            lay.sub.w = (lay.sub.w - 330).max(100);
        }
        let facts = crate::page_rail::Facts {
            tab: (!listing).then_some(s.tab),
            counts: s.shown.counts,
            hits: s.shown.hits,
            changed: s.shown.changed,
            changed_only: s.changed_only,
            total: self.setting_keys.iter().filter(|(k, _)| !page_tabs::gone(k)).count(),
            query: s.query.clone(),
        };
        let rail = crate::page_rail::layout(content, facts, Some(head));
        lay.parts.extend(rail.parts());
        lay.content = content;
        lay.rail = Some(rail);
        Some(lay)
    }

    /// A click on the rail, the head's filter, a "Mehr", a "Zurücksetzen" or a key of a Subs row. False: not one.
    pub(crate) fn page_tabs_act(&mut self, pid: u32, part: Part) -> bool {
        let row = |app: &App, r: usize| app.pages.by_pid.get(&pid).and_then(|s| s.shown.model.rows.get(r).cloned());
        match part {
            Part::Tab(i) => {
                self.page_edit_end();
                if let Some(s) = self.pages.by_pid.get_mut(&pid) {
                    s.tab = i.min(page_tabs::TABS.len() - 1);
                    s.tab_given = true;
                    s.query.clear();
                    s.changed_only = false;
                    s.scroll = 0;
                    s.prefix.clear();
                    s.address = page_tabs::address(s.tab);
                }
                eprintln!("bitscomp: settings page {pid}: tab {}", page_tabs::address(i));
                self.chrome_dirty();
            }
            Part::Search => {
                let q = self.pages.by_pid.get(&pid).map(|s| s.query.clone()).unwrap_or_default();
                self.page_edit_begin(pid, Part::Search, q);
            }
            Part::SearchClear => {
                self.page_edit_end();
                if let Some(s) = self.pages.by_pid.get_mut(&pid) {
                    s.query.clear();
                    s.scroll = 0;
                }
            }
            Part::Changed | Part::Filter(_) => {
                if let Some(s) = self.pages.by_pid.get_mut(&pid) {
                    s.changed_only = !matches!(part, Part::Filter(false));
                    s.scroll = 0;
                }
            }
            Part::Fold(r) => {
                let Some(id) = row(self, r).and_then(|x| x.key.strip_prefix(FOLD).map(String::from)) else { return true };
                if !self.pages.open.remove(&id) {
                    self.pages.open.insert(id);
                }
            }
            Part::Reset(r) => {
                let Some(x) = row(self, r) else { return true };
                // EINS-B: back to the default can give the assistant more freedom (Nur antworten -> Fragt vorher).
                let loosens = page_join::loosens(&x.key, &self.page_sub_value(&x.key), &x.default);
                if x.confirm || loosens {
                    let req = Req::Reset { key: x.key.clone(), confirmed: true };
                    self.page_ask(pid, Ask { req, key: x.key.clone(), to: Json::str(&x.default) });
                } else {
                    self.page_send(pid, Req::Reset { key: x.key, confirmed: false });
                }
            }
            Part::Sub(r, k) => {
                let Some(Row { kind: Kind::Subs(subs), .. }) = row(self, r) else { return true };
                let Some(sub) = subs.get(k) else { return true };
                let now = self.page_sub_value(&sub.key);
                if sub.switch {
                    self.page_set(pid, &sub.key, Json::Bool(now != "true"));
                } else {
                    self.page_edit_begin(pid, part, now);
                }
            }
            _ => return false,
        }
        self.page_view_refresh(pid);
        self.needs_frame = true;
        true
    }

    /// A key's value as the page shows it: settingsd's, else its row's default.
    pub(crate) fn page_sub_value(&self, key: &str) -> String {
        match self.settings.text(key) {
            Some(v) => v.to_string(),
            None => self.page_model.rows.iter().find(|r| r.key == key).map(|r| r.default.clone()).unwrap_or_default(),
        }
    }

    /// Enter in a Subs row's number: the key's own range decides.
    pub(crate) fn page_commit_sub(&mut self, pid: u32, r: usize, k: usize, text: &str) {
        let sub = self.pages.by_pid.get(&pid).and_then(|s| match s.shown.model.rows.get(r).map(|x| &x.kind) {
            Some(Kind::Subs(v)) => v.get(k).cloned(),
            _ => None,
        });
        let Some(sub) = sub else { return };
        let real = self.page_model.rows.iter().find(|x| x.key == sub.key).cloned();
        match real.map(|x| x.kind) {
            Some(Kind::Number { min, max, unit, .. }) => match page::number(text, min, max, &unit) {
                Ok(n) => self.page_set(pid, &sub.key, Json::Num(n as f64)),
                Err(why) => self.page_say(pid, format!("{}: {why}", sub.label), true),
            },
            _ => self.page_say(pid, format!("{}: hier nicht änderbar.", sub.label), true),
        }
    }

    /// Search typed: the list follows every key.
    pub(crate) fn page_search_typed(&mut self, pid: u32) {
        let Some(s) = self.pages.by_pid.get_mut(&pid) else { return };
        if let Some((Part::Search, e)) = s.edit.as_ref() {
            s.query = e.text.clone();
            s.scroll = 0;
            self.page_view_refresh(pid);
        }
    }

    /// A choice of a consolidated row: every key under it; one question first when one of them asks.
    pub(crate) fn page_set_many(&mut self, pid: u32, name: &str, choice: &str, writes: Vec<(String, Json)>) {
        let now = |app: &App, k: &str| app.page_sub_value(k);
        let shown = |j: &Json| match j {
            Json::Str(s) => s.clone(),
            Json::Bool(b) => b.to_string(),
            other => other.as_i64().map_or_else(String::new, |n| n.to_string()),
        };
        let todo: Vec<(String, Json)> = writes.into_iter().filter(|(k, v)| now(self, k) != shown(v)).collect();
        if todo.is_empty() {
            return self.page_say(pid, format!("{name}: unverändert"), false);
        }
        let confirm = todo.iter().any(|(k, _)| self.page_model.rows.iter().any(|r| r.key == *k && r.confirm));
        let loosens = todo.iter().any(|(k, v)| page_join::loosens(k, &now(self, k), &shown(v)));
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.answers_as = Some((name.to_string(), todo.len()));
        }
        if !confirm && !loosens {
            for (k, v) in todo {
                self.page_send(pid, Req::set(&k, v));
            }
            return;
        }
        let n = todo.len();
        let mut lines = vec![format!("„{name}“ wird „{choice}“.")];
        if loosens {
            let sees = |k: &str| todo.iter().any(|(x, v)| x == k && page_join::loosens(k, &now(self, k), &shown(v)));
            lines.push(if sees(crate::assist_level::NEVER_SEE) {
                "Der Assistent sieht dann wieder, was dieses Programm zeigt. Das bestätigst nur du selbst.".into()
            } else if sees(crate::assist_level::SIDEBAR_SEES) {
                "Der Assistent sieht dann bei jeder Frage in der Seitenleiste das Fenster vorne. Das bestätigst nur du selbst.".into()
            } else {
                "Der Assistent darf das dann ohne zu fragen. Das bestätigst nur du selbst.".into()
            });
        }
        if confirm {
            lines.push("Eine der Einstellungen darunter ändert sich nur mit Rückfrage.".into());
        }
        // A row of its own key (the level) changes just that; a consolidated row says how many keys it writes.
        if !(n == 1 && self.page_model.rows.iter().any(|r| r.key == todo[0].0 && r.label.starts_with(name))) && !todo.iter().all(|(k, _)| k == crate::assist_level::LEVEL) {
            lines.push(if n == 1 { "Es ändert sich eine Einstellung (einzeln unter „Mehr“).".into() } else { format!("Es ändern sich {n} Einstellungen (einzeln unter „Mehr“).") });
        }
        self.address_end();
        self.page_edit_end();
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.confirm = Some(crate::page_confirm::plain(format!("{name} ändern?"), lines, "Ändern"));
            s.many = Some((name.to_string(), todo));
            s.msg = None;
        }
        self.needs_frame = true;
    }

    pub(crate) fn page_many_asks(&self, pid: u32) -> bool {
        self.pages.by_pid.get(&pid).is_some_and(|s| s.many.is_some())
    }

    /// The answer to a consolidated row's question: "Ändern" sends every key confirmed.
    pub(crate) fn page_many_confirmed(&mut self, pid: u32, yes: bool) {
        let Some((name, todo)) = self.pages.by_pid.get_mut(&pid).and_then(|s| s.many.take()) else { return };
        if !yes {
            eprintln!("bitscomp: settings page {pid}: {name} cancelled, nothing sent");
            if let Some(s) = self.pages.by_pid.get_mut(&pid) {
                s.answers_as = None;
            }
            return self.page_say(pid, format!("{name}: nicht geändert (abgebrochen)."), false);
        }
        // Its question says only Georg gives this yes: a loosened leash key never goes by injected input.
        if !self.pages.real && todo.iter().any(|(k, _)| page_join::leashed(k)) {
            eprintln!("bitscomp: settings page {pid}: the confirmation of {name} by injected input refused, nothing sent");
            if let Some(s) = self.pages.by_pid.get_mut(&pid) {
                s.answers_as = None;
            }
            return self.page_say(pid, crate::ki_ask::REAL_INPUT, true);
        }
        eprintln!("bitscomp: settings page {pid}: Georg confirmed {name} ({} keys)", todo.len());
        for (k, v) in todo {
            self.page_send(pid, Req::Set { key: k, value: v, confirmed: true });
        }
    }

    /// The settings window's line in `ctl page`.
    pub(crate) fn page_tabs_report(&self, pid: u32) -> String {
        let Some(s) = self.pages.by_pid.get(&pid).filter(|s| s.kind == PageKind::Settings) else { return String::new() };
        format!(" tab={} query={:?} only_changed={} changed={}", page_tabs::TABS[s.tab.min(8)].word, s.query, s.changed_only, s.shown.changed)
    }
}

#[cfg(test)]
#[path = "page_tabs_app_tests.rs"]
mod tests;
