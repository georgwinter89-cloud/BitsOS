//! PK-7a: the "Programme" page in bitscomp (plan/spec/program-library.md section 4): a page window of kind `Programme`
//! (page_kind) whose generic part (window, scroll, field, dialog, image) is page_app's. Its clicks become
//! requests to bitspkgd through the client thread (pkg_client), its answers update the model (`programme`),
//! and the page shows the model. D6: the page is Georg's own; what changes something (run a plan, roll back,
//! remove, update, clear a cache) needs real input, injected input (MCP, `ctl click`) only reads. "Real" is
//! the sidebar's rule (`assist_card::Hand`, kept for every page in `Pages::real`): a click whose press, release
//! and last pointer move all came from a real device, a key from a real keyboard. The release alone proves
//! nothing: injected input can put the pointer on the button or press for him.

use std::time::Instant;

use crate::address::Where;
use crate::layout::Rect;
use crate::page::Part;
use crate::page_kind::PageKind;
use crate::pkg_client::{Job, Msg};
use crate::programme::{Action, Phase, SUGGESTIONS};
use crate::programme_draw::ProgLook;
use crate::programme_layout::Layout;
use crate::programme_view::{self as view, Item, Lib, ProgPart, TABS, Tab};
use crate::state::App;

/// What injected input gets instead of a change (D6).
pub const REAL_INPUT: &str = "Nur mit deiner echten Maus oder Tastatur: Claude fragt dafür über den Installationsdialog.";

impl App {
    fn prog_pages(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.pages.by_pid.iter().filter(|(_, s)| s.kind == PageKind::Programme).map(|(p, _)| *p).collect();
        v.sort();
        v
    }

    pub(crate) fn prog_send(&mut self, jobs: Vec<Job>) {
        if jobs.is_empty() && self.prog.events.is_empty() {
            return;
        }
        for j in jobs {
            let sent = self.prog_client.as_ref().is_some_and(|c| c.send(j.clone()));
            if !sent {
                let why = if self.prog_client.is_none() { "Programmverwaltung nicht verbunden" } else { "kein Thread für die Anfrage" };
                let more = self.prog.on(Msg::Failed(j.id, why.into()), Instant::now());
                // Without a client nothing more can go out: what a failure starts is dropped.
                drop(more);
            }
        }
        self.prog_events();
        self.needs_frame = true;
    }

    fn prog_events(&mut self) {
        for e in std::mem::take(&mut self.prog.events) {
            eprintln!("bitscomp: {e}");
        }
    }

    /// A programme page was started (page_spawned): the lists, and what the address names (a program, or the
    /// search the library hands on).
    pub(crate) fn prog_opened(&mut self, pid: u32, address: &str) {
        let now = Instant::now();
        let mut jobs = self.prog.refresh(now);
        if let Some(q) = view::View::search_of(&crate::address::normalise(address)) {
            eprintln!("bitscomp: programme: search {q:?} from the address");
            jobs.extend(self.prog.search(&q, now));
        }
        let program = self.pages.by_pid.get(&pid).and_then(|s| s.view.program.clone());
        if let Some(p) = &program {
            jobs.extend(self.prog.detail(p, now));
        }
        self.prog_send(jobs);
        if let Some(p) = program.filter(|p| self.prog.remove_next.as_deref() == Some(p.as_str())) {
            self.prog.remove_next = None;
            self.prog_remove(pid, &p);
        }
        // A removal whose dialog went with a closed page: it is asked again here.
        if !self.prog_pages().iter().any(|p| self.pages.by_pid.get(p).is_some_and(|s| s.confirm.is_some())) {
            if let Some(c) = self.prog.card.as_mut().filter(|c| c.action == Action::Remove && c.phase == Phase::Ready) {
                c.asked = false;
            }
            self.prog_ask_remove();
        }
    }

    pub fn prog_msg(&mut self, m: Msg) {
        let jobs = self.prog.on(m, Instant::now());
        self.needs_frame = true;
        self.prog_send(jobs);
        self.prog_ask_remove();
    }

    pub fn prog_expire(&mut self, id: u64) {
        let open = self.prog.pending.len();
        let jobs = self.prog.expire(id, Instant::now());
        if self.prog.pending.len() != open {
            // Something was late: the page says so (a request answered in time changes nothing here).
            self.needs_frame = true;
            self.prog_send(jobs);
        }
    }

    /// A removal's plan is ready: the page's own confirmation (page_confirm) says what goes and what stays.
    fn prog_ask_remove(&mut self) {
        let Some(c) = self.prog.card.as_ref().filter(|c| c.action == Action::Remove && c.phase == Phase::Ready && !c.asked) else { return };
        let mut lines = vec![c.summary.clone()];
        lines.push(if c.keep_data { "Deine Daten bleiben.".into() } else if c.data_removed { format!("Der Datenordner ({}) wird mit gelöscht.", crate::programme_text::bytes(c.data)) } else { "Es gibt keinen Datenordner.".into() });
        lines.push("Andere Programme bleiben unberührt.".into());
        let d = crate::page_confirm::plain(format!("{} wirklich entfernen?", c.name), lines, "Entfernen");
        let name = c.name.clone();
        let pid = self.prog_pages().into_iter().find(|p| self.pages.by_pid.get(p).is_some_and(|s| s.view.program.as_deref() == Some(name.as_str()))).or_else(|| self.prog_pages().first().copied());
        // Asked only once a page holds the dialog (no page now: the next one that opens asks it).
        if let Some(s) = pid.and_then(|p| self.pages.by_pid.get_mut(&p)) {
            s.confirm = Some(d);
            if let Some(c) = self.prog.card.as_mut() {
                c.asked = true;
            }
        }
    }

    /// "Entfernen" in the dialog, or "Abbrechen".
    pub(crate) fn prog_confirmed(&mut self, pid: u32, yes: bool) {
        self.needs_frame = true;
        if !yes {
            self.prog.close_card();
            return self.prog_say(pid, "Nicht entfernt (abgebrochen).", false);
        }
        if !self.pages.real {
            eprintln!("bitscomp: programme: removal confirmed by injected input: refused (D6) [{}]", self.prog_origins());
            self.prog.close_card();
            return self.prog_say(pid, REAL_INPUT, true);
        }
        // The dialog confirms a removal: never whatever plan replaced it meanwhile.
        if !self.prog.card.as_ref().is_some_and(|c| c.action == Action::Remove && c.phase == Phase::Ready) {
            eprintln!("bitscomp: programme: removal confirmed, but its plan is gone");
            return self.prog_say(pid, "Nicht entfernt: der Plan wurde inzwischen ersetzt; bitte noch einmal.", true);
        }
        let job = self.prog.go(Instant::now());
        self.prog_send(job.into_iter().collect());
    }

    /// For the log: where the parts of the click or key being handled came from.
    fn prog_origins(&self) -> String {
        format!("press={:?} release={:?} pointer={:?}", self.desk.press_origin, self.input_origin, self.pointer_origin).to_lowercase()
    }

    fn prog_say(&mut self, _pid: u32, t: &str, error: bool) {
        self.prog.note = Some((t.to_string(), error));
        self.needs_frame = true;
    }

    fn prog_lib_rows(&self) -> Vec<crate::library_view::Row> {
        crate::library_view::rows(&self.library, &self.desk.favourites, &self.session.recent_addresses(), "")
    }

    pub(crate) fn prog_items(&self, pid: u32) -> Option<Vec<Item>> {
        let s = self.pages.by_pid.get(&pid)?;
        let rows = self.prog_lib_rows();
        Some(view::items(&self.prog, &s.view, &Lib { index: &self.library, rows: &rows }))
    }

    fn prog_tabs(&self, pid: u32) -> (Vec<String>, usize) {
        let s = self.pages.by_pid.get(&pid);
        let mut tabs: Vec<String> = TABS.iter().map(|t| t.1.to_string()).collect();
        if !self.prog.updates.is_empty() {
            tabs[2] = format!("Aktualisierungen ({})", self.prog.updates.len());
        }
        let v = s.map(|s| s.view.clone()).unwrap_or_default();
        if let (Tab::Program, Some(p)) = (v.tab, &v.program) {
            tabs.push(p.clone());
            return (tabs, 3);
        }
        let i = TABS.iter().position(|t| t.0 == v.tab).unwrap_or(0);
        (tabs, i)
    }

    pub(crate) fn prog_layout(&self, pid: u32, content: Rect) -> Option<(Vec<Item>, Layout)> {
        let items = self.prog_items(pid)?;
        let scroll = self.pages.by_pid.get(&pid)?.scroll;
        let lay = crate::programme_layout::layout(content, &items, &self.prog_tabs(pid).0, scroll);
        Some((items, lay))
    }

    pub(crate) fn prog_prepare(&mut self, pid: u32, content: Rect) {
        let Some((_, lay)) = self.prog_layout(pid, content) else { return };
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.scroll = s.scroll.clamp(0, lay.max_scroll());
        }
    }

    pub(crate) fn prog_look(&self, pid: u32, content: Rect) -> Option<ProgLook> {
        let s = self.pages.by_pid.get(&pid)?;
        let (items, layout) = self.prog_layout(pid, content)?;
        let (tabs, tab) = self.prog_tabs(pid);
        let hover = match self.desk.hover {
            Some(crate::desk::Spot::Page(p, part)) if p == pid && part != Part::Body => Some(part),
            _ => None,
        };
        let state = match self.prog.live {
            Some(false) => (self.prog.down.clone(), true),
            None if self.prog_client.is_none() => ("Programmverwaltung nicht verbunden".into(), true),
            _ => (String::new(), false),
        };
        Some(ProgLook {
            layout,
            items,
            theme: self.theme_gen,
            address: s.address.clone(),
            tabs,
            tab,
            state,
            msg: self.prog.note.clone(),
            hover,
            query: self.prog.query.clone(),
            field: s.edit.as_ref().map(|(p, e)| crate::page_draw::FieldLook { part: *p, text: e.text.clone(), before: e.before().to_string() }),
            dialog: s.confirm.clone(),
            live: self.prog.live != Some(false) && self.prog_client.is_some(),
        })
    }

    fn prog_view_set(&mut self, pid: u32, v: view::View) {
        let now = Instant::now();
        let program = v.program.clone().filter(|_| v.tab == Tab::Program);
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.address = v.address();
            s.view = v;
            s.scroll = 0;
        }
        self.page_edit_end();
        // Each view asks again what it shows (cheap reads): what changed elsewhere (bitspkg on the console,
        // Claude's install through bitsinstall) shows up.
        let mut jobs = self.prog.refresh(now);
        if let Some(p) = program {
            jobs.extend(self.prog.detail(&p, now));
        }
        self.prog_send(jobs);
        self.chrome_dirty();
    }

    fn prog_program(&mut self, pid: u32, name: &str) {
        let v = view::View { tab: Tab::Program, program: Some(name.to_string()), keep_data: false };
        self.prog_view_set(pid, v);
    }

    fn prog_change_refused(&mut self, pid: u32, what: &str) -> bool {
        if self.pages.real {
            return false;
        }
        eprintln!("bitscomp: programme: {what} from injected input refused (D6) [{}]", self.prog_origins());
        self.prog_say(pid, REAL_INPUT, true);
        true
    }

    fn prog_remove(&mut self, pid: u32, name: &str) {
        let keep = self.prog.can("keep_data") && self.pages.by_pid.get(&pid).is_some_and(|s| s.view.keep_data);
        let job = self.prog.plan(Action::Remove, name, &[], keep, Instant::now());
        self.prog_send(vec![job]);
    }

    pub(crate) fn prog_act(&mut self, pid: u32, part: ProgPart) {
        let now = Instant::now();
        let v = self.pages.by_pid.get(&pid).map(|s| s.view.clone()).unwrap_or_default();
        match part {
            ProgPart::Tab(i) => match TABS.get(i) {
                Some((tab, _, _)) => self.prog_view_set(pid, view::View { tab: *tab, ..view::View::default() }),
                None => {}
            },
            ProgPart::Sources => self.open_address("einstellungen/programme", Where::Tab, None),
            ProgPart::Search => {
                let q = self.prog.query.clone();
                self.page_edit_begin(pid, Part::Prog(ProgPart::Search), q);
            }
            ProgPart::Install(i) => {
                if let Some(h) = self.prog.hits.get(i).map(|h| h.name.clone()) {
                    let job = self.prog.plan(Action::Install, &h, &[], false, now);
                    self.prog_send(vec![job]);
                }
            }
            ProgPart::Suggest(i) => {
                if let Some((pkg, _, _)) = SUGGESTIONS.get(i) {
                    let job = self.prog.plan(Action::Install, pkg, &[], false, now);
                    self.prog_send(vec![job]);
                }
            }
            ProgPart::Manage(i) => {
                if let Some(h) = self.prog.hits.get(i).map(|h| h.name.clone()) {
                    self.prog_program(pid, &h);
                }
            }
            ProgPart::Program(i) => {
                if let Some(n) = self.prog.installed.get(i).map(|p| p.name.clone()) {
                    self.prog_program(pid, &n);
                }
            }
            ProgPart::CardGo => {
                if self.prog_change_refused(pid, "Jetzt installieren") {
                    return;
                }
                if self.prog.card.as_ref().is_some_and(|c| c.action == Action::Remove) {
                    // A removal runs only through its dialog.
                    if let Some(c) = self.prog.card.as_mut() {
                        c.asked = false;
                    }
                    return self.prog_ask_remove();
                }
                let job = self.prog.go(now);
                self.prog_send(job.into_iter().collect());
            }
            ProgPart::CardList => {
                if let Some(c) = self.prog.card.as_mut() {
                    c.show_list = !c.show_list;
                }
            }
            ProgPart::CardClose => self.prog.close_card(),
            ProgPart::Extra(i) => {
                if !self.prog.can("with") {
                    return;
                }
                let Some(c) = self.prog.card.as_ref().filter(|c| c.phase == Phase::Ready || c.phase == Phase::Refused) else { return };
                let mut with: Vec<String> = c.extras.iter().filter(|e| e.2).map(|e| e.0.clone()).collect();
                let Some(dep) = c.extras.get(i).map(|e| e.0.clone()) else { return };
                if with.contains(&dep) {
                    with.retain(|d| *d != dep);
                } else {
                    with.push(dep);
                }
                let (a, n) = (c.action, c.name.clone());
                let job = self.prog.plan(a, &n, &with, false, now);
                self.prog_send(vec![job]);
            }
            ProgPart::KeepData => {
                if !self.prog.can("keep_data") {
                    return;
                }
                if let Some(s) = self.pages.by_pid.get_mut(&pid) {
                    s.view.keep_data = !s.view.keep_data;
                }
            }
            ProgPart::Remove => {
                if let Some(p) = v.program.clone() {
                    self.prog_remove(pid, &p);
                }
            }
            ProgPart::Rollback => {
                if self.prog_change_refused(pid, "Zurück zu dieser Version") {
                    return;
                }
                if let Some(p) = v.program.clone() {
                    let job = self.prog.rollback(&p, now);
                    self.prog_send(vec![job]);
                }
            }
            ProgPart::Clean => {
                if self.prog_change_refused(pid, "Cache leeren") {
                    return;
                }
                if !self.prog.can("clean") {
                    eprintln!("bitscomp: programme: clean: bitspkgd has no clean op");
                    return self.prog_say(pid, crate::programme_text::NO_CLEAN, true);
                }
                if let Some(p) = v.program.clone() {
                    let job = self.prog.clean(&p, now);
                    self.prog_send(vec![job]);
                }
            }
            ProgPart::Start => {
                let id = v.program.as_deref().and_then(|p| self.library.entries.iter().find(|e| e.source == crate::library::Source::Package(p.to_string()))).map(|e| e.id.clone());
                if let Some(id) = id {
                    self.library_start(&id, Where::Tab);
                }
            }
            ProgPart::UpdateAll | ProgPart::Update(_) => {
                if self.prog_change_refused(pid, "Aktualisieren") || self.prog.busy() {
                    return;
                }
                let names: Vec<String> = match part {
                    ProgPart::Update(i) => self.prog.updates.get(i).map(|u| u.name.clone()).into_iter().collect(),
                    _ => self.prog.updates.iter().map(|u| u.name.clone()).collect(),
                };
                let job = self.prog.update(&names, now);
                self.prog_send(job.into_iter().collect());
            }
            ProgPart::CheckNow => {
                if self.prog_change_refused(pid, "Jetzt prüfen") || self.prog.busy() {
                    return;
                }
                let job = self.prog.check_now(now);
                self.prog_send(job.into_iter().collect());
            }
            ProgPart::Tile(i) => {
                let items = self.prog_items(pid).unwrap_or_default();
                if let Some((id, _)) = view::tile_ids(&items).get(i).cloned() {
                    self.library_start(&id, Where::Tab);
                }
            }
            ProgPart::More(i) => {
                let items = self.prog_items(pid).unwrap_or_default();
                if let Some((_, Some(pkg))) = view::tile_ids(&items).get(i).cloned() {
                    self.prog_program(pid, &pkg);
                }
            }
        }
        self.needs_frame = true;
    }

    /// A right click on a tile: the library's circle menu (true: taken).
    pub(crate) fn prog_right(&mut self, pid: u32, part: ProgPart) -> bool {
        let (ProgPart::Tile(i) | ProgPart::More(i)) = part else { return false };
        let items = self.prog_items(pid).unwrap_or_default();
        let Some((id, _)) = view::tile_ids(&items).get(i).cloned() else { return false };
        self.library_menu(id);
        true
    }

    /// The search text changed (every key) or Enter: bitspkgd searches for it.
    pub(crate) fn prog_typed(&mut self, pid: u32) {
        let Some(text) = self.pages.by_pid.get(&pid).and_then(|s| s.edit.as_ref()).map(|(_, e)| e.text.clone()) else { return };
        if text == self.prog.query {
            return;
        }
        let job = self.prog.search(&text, Instant::now());
        self.prog_send(job.into_iter().collect());
    }

    pub(crate) fn prog_commit(&mut self, _pid: u32, text: &str) {
        if text != self.prog.query {
            let job = self.prog.search(text, Instant::now());
            self.prog_send(job.into_iter().collect());
        }
    }

    /// The library's circle menu: "Programm verwalten" and "Entfernen" of a package tile: the page of that program
    /// (an open programme page shows it, else one opens), for "Entfernen" with the removal asked at once.
    pub fn library_manage(&mut self, remove: bool) {
        let Some(id) = self.lib.menu_for.take() else { return };
        let Some(pkg) = self.library.entries.iter().find(|e| e.id == id).and_then(|e| if let crate::library::Source::Package(p) = &e.source { Some(p.clone()) } else { None }) else { return };
        self.library_close("programme page");
        eprintln!("bitscomp: library: {} {pkg} on the programme page", if remove { "remove" } else { "manage" });
        match self.prog_pages().first().copied() {
            Some(pid) => {
                self.prog_program(pid, &pkg);
                if let Some(w) = self.all_windows().into_iter().find(|w| self.page_pid(w) == Some(pid)) {
                    self.ws.focus_window(&w);
                    self.layout_dirty = true;
                }
                if remove {
                    self.prog_remove(pid, &pkg);
                }
            }
            None => {
                if remove {
                    self.prog.remove_next = Some(pkg.clone());
                }
                self.open_address(&format!("programme/{pkg}"), Where::Tab, None);
            }
        }
    }

    /// `ctl hot` lines of a programme page.
    pub(crate) fn prog_hot(&self, pid: u32, content: Rect, dialog: Option<&crate::page_confirm::Dialog>) -> String {
        let s = self.theme.scale;
        let Some((items, lay)) = self.prog_layout(pid, content) else { return String::new() };
        let parts = dialog.map_or_else(|| lay.parts.clone(), |d| crate::page_confirm::parts(lay.content, d));
        let mut out = String::new();
        for (part, b) in &parts {
            let (x, y) = (((b.x + b.w / 2) as f64 * s).round(), ((b.y + b.h / 2) as f64 * s).round());
            let name = match part {
                Part::Prog(p) => view::part_name(&self.prog, &items, *p),
                p => crate::page::part_name(&[], *p),
            };
            out.push_str(&format!("page pid={pid} {name} at={x},{y}\n"));
        }
        out
    }

    /// `ctl page`'s line of a programme page.
    pub(crate) fn prog_report(&self, pid: u32) -> String {
        let Some(s) = self.pages.by_pid.get(&pid) else { return String::new() };
        let m = &self.prog;
        let card = m.card.as_ref().map_or("none".to_string(), |c| format!("{}:{}:{:?}", c.action.word(), c.name, c.phase).to_lowercase());
        let line = m.card.as_ref().map_or(String::new(), |c| c.line.clone());
        let msg = m.note.as_ref().map_or("none".to_string(), |(t, e)| format!("{}{t:?}", if *e { "error:" } else { "" }));
        let dialog = s.confirm.as_ref().map_or("none".to_string(), |d| format!("{:?}:focus={}", d.title, if d.on_ok { "ok" } else { "cancel" }));
        let hits: Vec<&str> = m.hits.iter().map(|h| h.name.as_str()).collect();
        let rust: Vec<&str> = m.hits.iter().filter(|h| h.rust).map(|h| h.name.as_str()).collect();
        let desc = m.hits.first().map_or(String::new(), |h| view::desc(&self.library, &h.name, &h.desc));
        let features = m.features.as_ref().map_or("unknown".to_string(), |f| if f.is_empty() { "none".into() } else { f.join(",") });
        let installed: Vec<String> = m.installed.iter().map(|i| format!("{}@{}", i.name, i.current.as_deref().unwrap_or("-"))).collect();
        let updates: Vec<&str> = m.updates.iter().map(|u| u.name.as_str()).collect();
        format!(
            "page pid={pid} kind=programme address={} scroll={} live={} pending={} query={:?} hits={} rust={} desc={desc:?} installed={} updates={} card={card} card_line={line:?} dialog={dialog} msg={msg} features={features} down={:?}\n",
            s.address,
            s.scroll,
            match m.live { Some(true) => "true", Some(false) => "false", None => "unknown" },
            m.pending.len(),
            m.query,
            hits.join(","),
            rust.join(","),
            installed.join(","),
            updates.join(","),
            m.down,
        )
    }
}

#[cfg(test)]
#[path = "programme_app_tests.rs"]
mod tests;
