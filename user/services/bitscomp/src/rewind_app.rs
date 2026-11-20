//! REWIND in bitscomp (rewind.rs has the model and the texts, rewind_view.rs the places, rewind_draw.rs the look):
//! opened by the strip's "Zurückspulen", Win+Z or `ctl rewind open`, only while bitssnap lists snapshots (else the
//! button is grey and a click or Win+Z says why); closed by Esc, "Zur Bühne", "Zum Jetzt", Win+Z again. Never over
//! the sofa mode's screens, the installer's question or another confirmation dialog; the library, the Feld and the
//! view close each other. While open it takes every key but the modifiers and every press.
//!
//! Every change asks first: the way's button gets bitssnap's dry run (what would change), shown in the pages'
//! confirmation dialog (focus on "Abbrechen"); only "Zurückholen" / "Zurücksetzen" / ... by Georg's own mouse or
//! keyboard, a moment after the dialog appeared (`install_dialog::ARM`), sends the request with `confirmed`.
//! Injected input (`ctl key`, `ctl click`, MCP's devices) may open the view, choose, open the dialog and cancel it,
//! never confirm and never move the dialog's focus to its yes; bitssnap itself takes `confirmed` only from this
//! compositor (or root), so no other program can do it for him either.

use std::time::Duration;

use mcpreg::Json;
use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::input::Origin;
use crate::layout::Rect;
use crate::page_confirm::{self, Step};
use crate::rewind::{self, Ask, Avail, Hot, Tab, Want};
use crate::rewind_view::{self as view, Geo, PaneGeo};
use crate::snap_client::{self, Job, Msg};
use crate::state::App;

pub const REAL_INPUT: &str = "Das geht nur mit deiner echten Maus oder Tastatur: nichts geändert.";

/// What the pane's buttons say now: (action, second action).
pub(crate) fn act_texts(r: &rewind::Rewind) -> (String, String) {
    match r.tab {
        Tab::Choose => (String::new(), String::new()),
        Tab::Path => match r.browse.pick.and_then(|i| r.browse.entries.get(i)) {
            Some(e) => (format!("„{}“ zurückholen", e.name), String::new()),
            None if rewind::mine(&r.browse.path) => ("Diesen Ordner zurückholen".into(), String::new()),
            None => (String::new(), String::new()),
        },
        Tab::Program => match r.progs.pick.and_then(|i| r.progs.list.get(i)) {
            Some(p) => (format!("„{}“ zurücksetzen", p.name), String::new()),
            None => (String::new(), String::new()),
        },
        Tab::All if r.pending.is_some() || r.set_for_start => ("Jetzt neu starten".into(), "Rückgängig machen".into()),
        Tab::All => ("Alles auf diesen Zeitpunkt".into(), String::new()),
    }
}

impl App {
    pub fn rewind_geo(&self) -> Geo {
        view::geo(self.area_full(), &self.theme)
    }

    pub fn rewind_pane(&self, g: &Geo) -> PaneGeo {
        let (a, b) = act_texts(&self.rewind);
        view::pane(g, &self.theme, &a, &b)
    }

    /// Why the view cannot open now (None: it can).
    pub fn rewind_refused(&self) -> Option<&'static str> {
        if self.sofa.listens() {
            Some("the sofa mode's screen listens")
        } else if self.install.shown() || self.images.boot.shown() {
            Some("the installer's question or the boot scene is shown")
        } else if self.desk.ws_ask.is_some() || self.page_dialog_pid().is_some() {
            Some("a confirmation dialog is open")
        } else {
            None
        }
    }

    /// The strip's button and Win+Z: the view, or (grey) the reason in a notice, and a fresh look.
    pub fn rewind_button(&mut self, why: &str) {
        if self.rewind.open {
            return self.rewind_close(why);
        }
        if let Some(r) = self.rewind.avail.reason() {
            eprintln!("bitscomp: rewind: not opened ({why}): {}", self.rewind.avail.name());
            self.notice(r);
            self.rewind.said = Some(r.to_string());
            self.rewind_probe(why);
            return;
        }
        self.rewind_open(why);
    }

    pub fn rewind_open(&mut self, why: &str) {
        if self.rewind.open {
            return;
        }
        if let Some(no) = self.rewind_refused() {
            eprintln!("bitscomp: rewind: not opened ({why}): {no}");
            return;
        }
        self.library_close("rewind");
        self.feld_close("rewind");
        self.assist_menu_close("rewind");
        self.address_end();
        self.page_edit_end();
        self.assist_blur();
        self.menu = None;
        self.desk.min_list.open = false;
        // A reason the grey button gave a moment ago is no longer true.
        if self.rewind.said.take().is_some_and(|s| self.desk.notice.as_ref().is_some_and(|(n, _)| *n == s)) {
            self.desk.notice = None;
        }
        let r = &mut self.rewind;
        (r.open, r.pick, r.first, r.tab, r.dialog, r.msg, r.hover, r.set_for_start) = (true, 0, 0, Tab::Choose, None, None, None, false);
        r.browse = Default::default();
        r.progs = Default::default();
        eprintln!("bitscomp: rewind: opened ({why}), {} snapshots", self.rewind.snaps.len());
        self.rewind_probe("opened");
        self.hover = None;
        self.chrome_dirty();
        self.needs_frame = true;
        self.motion(self.now_ms());
    }

    pub fn rewind_close(&mut self, why: &str) {
        if !self.rewind.open {
            return;
        }
        let r = &mut self.rewind;
        (r.open, r.dialog, r.hover) = (false, None, None);
        eprintln!("bitscomp: rewind: closed ({why})");
        self.chrome_dirty();
        self.needs_frame = true;
        self.motion(self.now_ms());
    }

    /// Sends `line` for `want`; false when there is no client (the answer is then "absent" at once).
    fn rewind_send(&mut self, want: Want, line: String, wait: Duration) -> bool {
        let r = &mut self.rewind;
        r.next_id += 1;
        let id = r.next_id;
        #[cfg(test)]
        if let Some(s) = r.sent.as_mut() {
            s.push((id, line));
            r.waiting.insert(id, want);
            return true;
        }
        let sent = self.snap_client.as_ref().is_some_and(|c| c.send(Job::Ask { id, line, wait }));
        if sent {
            self.rewind.waiting.insert(id, want);
        } else {
            self.rewind.waiting.insert(id, want);
            self.rewind_msg(Msg::Failed(id, "absent".into()));
        }
        sent
    }

    /// Asks for the list (the timer, the view opening, after a restore); one list at a time.
    pub fn rewind_probe(&mut self, _why: &str) {
        if self.rewind.waiting.values().any(|w| *w == Want::List) {
            return;
        }
        self.rewind_send(Want::List, r#"{"op":"list"}"#.into(), snap_client::SHORT);
    }

    fn rewind_say(&mut self, text: impl Into<String>, bad: bool) {
        let t = text.into();
        if self.rewind.open {
            self.rewind.msg = Some((t, bad));
            self.needs_frame = true;
        } else {
            self.notice(t);
        }
    }

    /// An answer of bitssnap (or of init, for the restart).
    pub fn rewind_msg(&mut self, m: Msg) {
        let (id, reply) = match m {
            Msg::Reply(id, j) => (id, Ok(j)),
            Msg::Failed(id, why) => (id, Err(why)),
            Msg::Power(id, text, down) => {
                self.rewind.waiting.remove(&id);
                self.rewind.busy = None;
                eprintln!("bitscomp: rewind: restart: {text}");
                return self.rewind_say(text, !down);
            }
        };
        let Some(want) = self.rewind.waiting.remove(&id) else { return };
        // A refusal or failure of bitssnap is an `ok: false` line with its code.
        let reply = reply.and_then(|j| if j.get("ok").and_then(Json::as_bool) == Some(true) { Ok(j) } else { Err(j.get("error").and_then(Json::as_str).unwrap_or("failed").to_string()) });
        if let Err(code) = &reply {
            eprintln!("bitscomp: rewind: {want:?}: {code}");
        }
        self.needs_frame |= self.rewind.open;
        match want {
            Want::List => {
                let was_ready = self.rewind.avail.ready();
                let changed = match &reply {
                    Ok(j) => self.rewind.take_list(j),
                    Err(code) => {
                        let a = if code == "absent" { Avail::Absent } else { Avail::Failed };
                        let changed = self.rewind.avail != a;
                        self.rewind.avail = a;
                        changed
                    }
                };
                if changed {
                    eprintln!("bitscomp: rewind: {} ({} snapshots)", self.rewind.avail.name(), self.rewind.snaps.len());
                    self.rewind_first_fit();
                }
                // Only what shows: a still desktop draws no frame for a reason nobody hovers (B4, desktop.md §0.7).
                if changed && (was_ready != self.rewind.avail.ready() || self.rewind.open) {
                    self.chrome_dirty();
                    self.needs_frame = true;
                }
            }
            Want::Browse { snap, path } => {
                if self.rewind.chosen().map(|s| s.id) != Some(snap) || self.rewind.browse.path != path {
                    return;
                }
                let b = &mut self.rewind.browse;
                b.loading = false;
                match reply {
                    Ok(j) => {
                        (b.entries, b.more, b.error) = (rewind::entries(&j), j.get("more").and_then(Json::as_bool) == Some(true), None);
                    }
                    Err(_) => {
                        b.entries.clear();
                        b.error = Some("Diesen Ordner gibt es in diesem Stand nicht, oder er ist nicht deiner.".into());
                    }
                }
            }
            Want::Programs { snap } => {
                if self.rewind.chosen().map(|s| s.id) != Some(snap) {
                    return;
                }
                let p = &mut self.rewind.progs;
                p.loading = false;
                match reply {
                    Ok(j) => (p.list, p.error) = (rewind::programs(&j), None),
                    Err(code) => (p.list, p.error) = (Vec::new(), Some(rewind::failed(&code).into())),
                }
            }
            Want::Preview(ask) => {
                self.rewind.busy = None;
                match reply {
                    Ok(j) => self.rewind_dialog_for(ask, &j),
                    Err(code) => self.rewind_say(rewind::failed(&code), true),
                }
            }
            Want::Do(ask) => {
                self.rewind.busy = None;
                match reply {
                    Ok(j) => self.rewind_done(&ask, &j),
                    Err(code) => self.rewind_say(rewind::failed(&code), true),
                }
                self.rewind_probe("after a change");
            }
            Want::Power => {}
        }
    }

    /// The dialog for `ask` with bitssnap's preview `j`.
    fn rewind_dialog_for(&mut self, ask: Ask, j: &Json) {
        let r = &self.rewind;
        // The question names the snapshot asked about; a choice changed meanwhile (input waits while a preview
        // is asked, a new list can still move things) asks nothing.
        let Some((snap, tab)) = ask.about() else { return };
        let s = r.snaps.iter().find(|s| s.id == snap).cloned();
        let Some(s) = s.filter(|_| r.chosen().map(|c| c.id) == Some(snap) && r.tab == tab) else {
            eprintln!("bitscomp: rewind: the preview of {} is for a choice no longer shown", ask.name());
            return self.rewind_say("Die Auswahl hat sich geändert: nichts gefragt, nichts geändert.", false);
        };
        let (off, now) = (r.off_min, r.now_s);
        let (title, lines, ok) = match &ask {
            Ask::Path { .. } => ("Wirklich zurückholen?", rewind::path_lines(&s, j, off, now), "Zurückholen"),
            Ask::Program { .. } => ("Programm wirklich zurücksetzen?", rewind::program_lines(&s, j, off, now), "Zurücksetzen"),
            Ask::All { .. } => ("Wirklich alles auf diesen Zeitpunkt?", rewind::all_lines(j, off), "Beim Neustart zurück"),
            Ask::Undo | Ask::Restart => return,
        };
        self.rewind_dialog(title, lines, ok, ask);
    }

    fn rewind_dialog(&mut self, title: &str, lines: Vec<String>, ok: &str, ask: Ask) {
        if !self.rewind.open {
            return;
        }
        eprintln!("bitscomp: rewind: asks before {} ({} lines)", ask.name(), lines.len());
        self.rewind.dialog = Some((page_confirm::plain(title.into(), lines, ok), ask));
        self.rewind.since = crate::pacer::now();
        self.needs_frame = true;
    }

    /// What a confirmed change did, in words.
    fn rewind_done(&mut self, ask: &Ask, j: &Json) {
        let n = |k: &str| j.get(k).and_then(Json::as_u64).unwrap_or(0);
        let text = match ask {
            Ask::Path { path, .. } => {
                let p = j.get("restored").and_then(Json::as_str).unwrap_or(path);
                // The folder list shows the restored state.
                let b = self.rewind.browse.path.clone();
                if self.rewind.tab == Tab::Path && !b.is_empty() {
                    self.rewind_browse(&b);
                }
                format!("Zurückgeholt: {p}. Der Stand davor ist als Nr. {} gesichert.", n("before"))
            }
            Ask::Program { name, .. } => format!("„{name}“ ist zurückgesetzt (der Stand davor ist Nr. {}). Läuft es gerade, starte es neu.", n("before")),
            Ask::All { snap, .. } => {
                self.rewind.set_for_start = true;
                let to = j.get("snapshot").and_then(Json::as_u64).unwrap_or(*snap);
                self.rewind.pending = Some(to);
                self.rewind.ours = Some((to, j.get("pin").and_then(Json::as_u64)));
                let s = j.get("snapshot_s").and_then(Json::as_u64).unwrap_or(0);
                format!("Eingestellt: beim nächsten Start ist alles wie {}.", rewind::when(s, self.rewind.off_min, self.rewind.now_s))
            }
            Ask::Undo => {
                self.rewind.set_for_start = false;
                if j.get("undone").and_then(Json::as_bool) == Some(true) {
                    (self.rewind.pending, self.rewind.ours) = (None, None);
                    "Zurückgenommen: der nächste Start lässt alles, wie es ist.".into()
                } else if self.rewind.pending.is_some() {
                    // A rollback set another way (bitssnap rollback, MCP) is not the kind this undo takes back.
                    "Das ließ sich hier nicht zurücknehmen: es wurde nicht hier eingestellt. Beim nächsten Start gilt es weiter.".into()
                } else {
                    "Es war nichts zurückzunehmen.".into()
                }
            }
            Ask::Restart => return,
        };
        eprintln!("bitscomp: rewind: {} done", ask.name());
        self.rewind_say(text, false);
    }

    /// The first chip shown keeps the chosen one on the time line.
    fn rewind_first_fit(&mut self) {
        let n = self.rewind_geo().chips.len().max(1);
        let r = &mut self.rewind;
        r.pick = r.pick.min(r.snaps.len().saturating_sub(1));
        if r.pick < r.first {
            r.first = r.pick;
        } else if r.pick >= r.first + n {
            r.first = r.pick + 1 - n;
        }
    }

    /// Chooses snapshot `i` (newest first): what the pane shows is that one's.
    pub fn rewind_pick(&mut self, i: usize) {
        if i >= self.rewind.snaps.len() {
            return;
        }
        let same = i == self.rewind.pick;
        self.rewind.pick = i;
        self.rewind_first_fit();
        self.needs_frame = true;
        if same {
            return;
        }
        self.rewind.msg = None;
        match self.rewind.tab {
            Tab::Path => {
                let p = if self.rewind.browse.path.is_empty() { rewind::home() } else { self.rewind.browse.path.clone() };
                self.rewind_browse(&p);
            }
            Tab::Program => self.rewind_programs(),
            _ => {}
        }
    }

    pub fn rewind_tab(&mut self, tab: Tab) {
        let r = &mut self.rewind;
        if r.tab == tab {
            return;
        }
        (r.tab, r.msg) = (tab, None);
        self.needs_frame = true;
        match tab {
            Tab::Path => self.rewind_browse(&rewind::home()),
            Tab::Program => self.rewind_programs(),
            _ => {}
        }
    }

    fn rewind_browse(&mut self, path: &str) {
        let Some(s) = self.rewind.chosen().map(|s| s.id) else { return };
        self.rewind.browse = rewind::Browse { path: path.to_string(), loading: true, ..Default::default() };
        let line = Json::obj(vec![("op", Json::str("browse")), ("snapshot", Json::Num(s as f64)), ("path", Json::str(path))]).to_string();
        self.rewind_send(Want::Browse { snap: s, path: path.to_string() }, line, snap_client::SHORT);
        self.needs_frame = true;
    }

    fn rewind_programs(&mut self) {
        let Some(s) = self.rewind.chosen().map(|s| s.id) else { return };
        self.rewind.progs = rewind::Progs { loading: true, ..Default::default() };
        let line = Json::obj(vec![("op", Json::str("browse")), ("snapshot", Json::Num(s as f64)), ("programs", Json::Bool(true))]).to_string();
        self.rewind_send(Want::Programs { snap: s }, line, snap_client::SHORT);
    }

    /// The pane's action: the preview for a restore (the dialog follows its answer), or the dialog at once.
    pub fn rewind_act(&mut self, second: bool) {
        let r = &self.rewind;
        if r.busy.is_some() || r.dialog.is_some() {
            return;
        }
        let Some(s) = r.chosen().cloned() else { return };
        let ask = match r.tab {
            Tab::Choose => return,
            Tab::Path => {
                let path = match r.browse.pick.and_then(|i| r.browse.entries.get(i)) {
                    Some(e) => format!("{}/{}", r.browse.path.trim_end_matches('/'), e.name),
                    None if rewind::mine(&r.browse.path) => r.browse.path.clone(),
                    None => return,
                };
                Ask::Path { snap: s.id, path }
            }
            Tab::Program => match r.progs.pick.and_then(|i| r.progs.list.get(i)) {
                Some(p) => Ask::Program { snap: s.id, name: p.name.clone() },
                None => return,
            },
            Tab::All if r.set_for_start || r.pending.is_some() => {
                if second {
                    let lines = rewind::undo_lines(&self.rewind);
                    return self.rewind_dialog("Wirklich rückgängig machen?", lines, "Rückgängig", Ask::Undo);
                }
                return self.rewind_restart_ask();
            }
            Tab::All => Ask::All { snap: s.id, at: s.created_s },
        };
        let Some(line) = ask.line(true) else { return };
        self.rewind.busy = Some("Wird geprüft …".into());
        self.rewind.msg = None;
        self.rewind_send(Want::Preview(ask), line, snap_client::SHORT);
        self.needs_frame = true;
    }

    fn rewind_restart_ask(&mut self) {
        if !self.sofa.quick.power.as_ref().is_some_and(|p| p.exists()) {
            return self.rewind_say("Neu starten geht von hier noch nicht: starte BitsOS über das Menü neu.", true);
        }
        let lines = rewind::restart_lines(&self.rewind);
        self.rewind_dialog("Jetzt neu starten?", lines, "Neu starten", Ask::Restart);
    }

    /// The dialog closes: `yes` only counts when the input is Georg's own and the dialog stood long enough.
    pub fn rewind_dialog_close(&mut self, yes: bool, real: bool) {
        if self.rewind.dialog.is_none() {
            return;
        }
        if yes && !real {
            eprintln!("bitscomp: rewind: a yes by injected input refused");
            self.rewind.since = crate::pacer::now();
            if let Some((d, _)) = self.rewind.dialog.as_mut() {
                d.on_ok = false;
            }
            self.rewind.msg = Some((REAL_INPUT.into(), true));
            self.needs_frame = true;
            return;
        }
        if yes && crate::pacer::now() < self.rewind.since + crate::install_dialog::ARM {
            eprintln!("bitscomp: rewind: a yes too soon after the question appeared does not count");
            return;
        }
        let Some((_, ask)) = self.rewind.dialog.take() else { return };
        self.needs_frame = true;
        if !yes {
            eprintln!("bitscomp: rewind: {} cancelled, nothing sent", ask.name());
            return self.rewind_say("Abgebrochen: nichts geändert.", false);
        }
        eprintln!("bitscomp: rewind: Georg confirmed {}", ask.name());
        if ask == Ask::Restart {
            let Some(socket) = self.sofa.quick.power.clone() else { return };
            let r = &mut self.rewind;
            r.next_id += 1;
            let id = r.next_id;
            let sent = self.snap_client.as_ref().is_some_and(|c| c.send(Job::Power { id, socket, physical: true }));
            if sent {
                self.rewind.waiting.insert(id, Want::Power);
                self.rewind.busy = Some("BitsOS startet neu …".into());
            } else {
                self.rewind_say("Neu starten ging nicht: kein Faden für die Anfrage.", true);
            }
            return;
        }
        let Some(line) = ask.line(false) else { return };
        self.rewind.busy = Some(match ask {
            Ask::Path { .. } => "Wird zurückgeholt …",
            Ask::Program { .. } => "Wird zurückgesetzt …",
            _ => "Wird eingestellt …",
        }
        .into());
        self.rewind_send(Want::Do(ask), line, snap_client::LONG);
    }

    /// Every part a click can hit, the dialog's first (it is modal: then only its buttons).
    pub fn rewind_parts(&self) -> Vec<(Hot, Rect)> {
        let g = self.rewind_geo();
        if let Some((d, _)) = &self.rewind.dialog {
            let (_, cancel, ok) = page_confirm::boxes(g.full, d.lines.len());
            return vec![(Hot::DialogCancel, cancel), (Hot::DialogOk, ok)];
        }
        let r = &self.rewind;
        let mut v = vec![(Hot::Back, g.back), (Hot::Now, g.now), (Hot::Older, g.older), (Hot::Newer, g.newer)];
        for (k, c) in g.chips.iter().enumerate() {
            if r.first + k < r.snaps.len() {
                v.push((Hot::Chip(r.first + k), *c));
            }
        }
        for (i, w) in g.ways.iter().enumerate() {
            v.push((Hot::Way(view::way(i)), *w));
        }
        let p = self.rewind_pane(&g);
        let (a, b) = act_texts(r);
        match r.tab {
            Tab::Path => {
                if rewind::parent(&r.browse.path).is_some() {
                    v.push((Hot::Up, p.up));
                }
                for (k, row) in p.rows.iter().enumerate() {
                    if r.browse.first + k < r.browse.entries.len() {
                        v.push((Hot::Row(r.browse.first + k), *row));
                    }
                }
            }
            Tab::Program => {
                for (k, row) in p.rows.iter().enumerate() {
                    if r.progs.first + k < r.progs.list.len() {
                        v.push((Hot::Row(r.progs.first + k), *row));
                    }
                }
            }
            _ => {}
        }
        if !a.is_empty() {
            let hot = if r.tab == Tab::All && !b.is_empty() { Hot::Restart } else { Hot::Do };
            v.push((hot, p.act));
        }
        if !b.is_empty() {
            v.push((Hot::Undo, p.act2));
        }
        v
    }

    fn rewind_row(&mut self, i: usize) {
        match self.rewind.tab {
            Tab::Path => {
                let b = &self.rewind.browse;
                let Some(e) = b.entries.get(i).cloned() else { return };
                if b.pick == Some(i) && e.open {
                    let p = format!("{}/{}", b.path.trim_end_matches('/'), e.name);
                    return self.rewind_browse(&p);
                }
                self.rewind.browse.pick = Some(i);
            }
            Tab::Program => self.rewind.progs.pick = Some(i),
            _ => return,
        }
        self.rewind.msg = None;
        self.needs_frame = true;
    }

    fn rewind_up(&mut self) {
        if let Some(p) = rewind::parent(&self.rewind.browse.path) {
            self.rewind_browse(&p);
        }
    }

    /// A press while the view is open.
    pub fn rewind_press(&mut self, code: u32) {
        if code != crate::pointer::BTN_LEFT {
            return;
        }
        // The view acts on the press (as the installer's question): it and the pointer's last move must be real.
        let real = self.input_origin == Origin::Physical && self.pointer_origin == Origin::Physical;
        let Some(h) = view::hit(&self.rewind_parts(), self.pointer.x, self.pointer.y) else { return };
        // While bitssnap is asked the choice stays as asked: only leaving the view goes.
        if self.rewind.busy.is_some() && !matches!(h, Hot::Back | Hot::Now) {
            return;
        }
        if !real && self.rewind.dialog.is_some() {
            // Injected input touched the question: whatever it prepared, its yes waits anew.
            self.rewind.since = crate::pacer::now();
        }
        match h {
            Hot::DialogCancel => self.rewind_dialog_close(false, real),
            Hot::DialogOk => self.rewind_dialog_close(true, real),
            Hot::Back | Hot::Now => self.rewind_close(if h == Hot::Back { "Zur Bühne" } else { "Zum Jetzt" }),
            Hot::Older => self.rewind_step(1),
            Hot::Newer => self.rewind_step(-1),
            Hot::Chip(i) => self.rewind_pick(i),
            Hot::Way(t) => self.rewind_tab(t),
            Hot::Up => self.rewind_up(),
            Hot::Row(i) => self.rewind_row(i),
            Hot::Do | Hot::Restart => self.rewind_act(false),
            Hot::Undo => self.rewind_act(true),
        }
    }

    fn rewind_step(&mut self, d: i32) {
        let n = self.rewind.snaps.len();
        if n == 0 {
            return;
        }
        let i = (self.rewind.pick as i64 + i64::from(d)).clamp(0, n as i64 - 1) as usize;
        self.rewind_pick(i);
    }

    fn rewind_list_step(&mut self, d: i32) {
        let rows = self.rewind_pane(&self.rewind_geo()).rows.len().max(1);
        let (pick, first, n) = match self.rewind.tab {
            Tab::Path => (&mut self.rewind.browse.pick, &mut self.rewind.browse.first, self.rewind.browse.entries.len()),
            Tab::Program => (&mut self.rewind.progs.pick, &mut self.rewind.progs.first, self.rewind.progs.list.len()),
            _ => return,
        };
        if n == 0 {
            return;
        }
        // Up from the first entry is "the folder itself" (no pick), as at the start.
        *pick = match (*pick, d > 0) {
            (None, true) => Some(0),
            (None, false) => None,
            (Some(0), false) => None,
            (Some(i), true) => Some((i + 1).min(n - 1)),
            (Some(i), false) => Some(i - 1),
        };
        if let Some(i) = *pick {
            if i < *first {
                *first = i;
            } else if i >= *first + rows {
                *first = i + 1 - rows;
            }
        }
        self.rewind.msg = None;
        self.needs_frame = true;
    }

    pub fn rewind_wheel(&mut self, steps: f64) {
        if self.rewind.dialog.is_some() {
            return;
        }
        let g = self.rewind_geo();
        let (x, y) = (self.pointer.x, self.pointer.y);
        let rows = self.rewind_pane(&g).rows.len().max(1);
        let d = if steps > 0.0 { 1 } else { -1 };
        if g.lower.contains(x, y) {
            let n = self.rewind.snaps.len();
            let room = g.chips.len().max(1);
            let r = &mut self.rewind;
            r.first = (r.first as i64 + d).clamp(0, n.saturating_sub(room) as i64) as usize;
        } else if g.pane.contains(x, y) {
            let (first, n) = match self.rewind.tab {
                Tab::Path => (&mut self.rewind.browse.first, self.rewind.browse.entries.len()),
                Tab::Program => (&mut self.rewind.progs.first, self.rewind.progs.list.len()),
                _ => return,
            };
            *first = (*first as i64 + d).clamp(0, n.saturating_sub(rows) as i64) as usize;
        } else {
            return;
        }
        self.needs_frame = true;
    }

    /// A key while the view is open; false for the modifiers (they reach the client as before).
    pub fn rewind_key(&mut self, sym: Keysym, mods: &ModifiersState) -> bool {
        if crate::library_ui::is_modifier(sym) {
            return false;
        }
        let real = self.input_origin == Origin::Physical;
        if let Some((d, _)) = self.rewind.dialog.as_mut() {
            let before = d.on_ok;
            let step = d.key(sym);
            let moved = before != d.on_ok;
            if !real && step != Step::Cancel {
                // Injected input may cancel the question, never prepare or give its yes.
                d.on_ok = false;
                self.rewind.since = crate::pacer::now();
                self.needs_frame = true;
                if step == Step::Confirm {
                    self.rewind_dialog_close(true, false);
                }
                return true;
            }
            match step {
                Step::Stay => self.needs_frame |= moved,
                Step::Cancel => self.rewind_dialog_close(false, real),
                Step::Confirm => self.rewind_dialog_close(true, real),
            }
            return true;
        }
        if self.rewind.busy.is_some() {
            if sym == Keysym::Escape {
                self.rewind_close("Esc");
            }
            return true;
        }
        let tab_step = |t: Tab, d: i32| {
            let all = [Tab::Path, Tab::Program, Tab::All];
            let i = all.iter().position(|x| *x == t).map_or(if d > 0 { 0 } else { 2 }, |i| (i as i32 + d).rem_euclid(3) as usize);
            all[i]
        };
        match sym {
            Keysym::Escape => self.rewind_close("Esc"),
            Keysym::Left | Keysym::KP_Left => self.rewind_step(1),
            Keysym::Right | Keysym::KP_Right => self.rewind_step(-1),
            Keysym::Down | Keysym::KP_Down => self.rewind_list_step(1),
            Keysym::Up | Keysym::KP_Up => self.rewind_list_step(-1),
            Keysym::Tab => self.rewind_tab(tab_step(self.rewind.tab, if mods.shift { -1 } else { 1 })),
            Keysym::ISO_Left_Tab => self.rewind_tab(tab_step(self.rewind.tab, -1)),
            Keysym::BackSpace if self.rewind.tab == Tab::Path => self.rewind_up(),
            Keysym::Return | Keysym::KP_Enter => {
                let b = &self.rewind.browse;
                let into = (self.rewind.tab == Tab::Path).then(|| b.pick.and_then(|i| b.entries.get(i))).flatten().filter(|e| e.open).map(|e| format!("{}/{}", b.path.trim_end_matches('/'), e.name));
                match into {
                    Some(p) => self.rewind_browse(&p),
                    None => self.rewind_act(false),
                }
            }
            _ => {}
        }
        true
    }

    pub fn rewind_motion(&mut self) {
        let h = view::hit(&self.rewind_parts(), self.pointer.x, self.pointer.y);
        if h != self.rewind.hover {
            self.rewind.hover = h;
            self.needs_frame = true;
        }
    }

    /// `ctl rewind state`: open, the availability, the chosen snapshot and way, every snapshot, the pane's lists,
    /// the dialog.
    pub fn rewind_state(&self) -> String {
        let r = &self.rewind;
        let dash = |o: Option<String>| o.unwrap_or_else(|| "-".into());
        let mut out = format!(
            "rewind open={} avail={} pick={} snap={} tab={} busy={} pending={} set={} dialog={} msg={}\n",
            r.open,
            r.avail.name(),
            r.pick,
            dash(r.chosen().map(|s| s.id.to_string())),
            r.tab.name(),
            dash(r.busy.clone().map(|b| format!("{b:?}"))),
            dash(r.pending.map(|p| p.to_string())),
            r.set_for_start,
            dash(r.dialog.as_ref().map(|(_, a)| a.name().to_string())),
            dash(r.msg.as_ref().map(|(m, _)| format!("{m:?}"))),
        );
        for (i, s) in r.snaps.iter().enumerate() {
            out.push_str(&format!("snap {i} id={} time={} day={:?} kind={}\n", s.id, rewind::clock(s.created_s, r.off_min), rewind::day(s.created_s, r.off_min, r.now_s), s.kind.name()));
        }
        if r.tab == Tab::Path {
            let b = &r.browse;
            out.push_str(&format!("browse path={:?} pick={} loading={} entries={} error={}\n", b.path, dash(b.pick.map(|p| p.to_string())), b.loading, b.entries.len(), b.error.is_some()));
            for (i, e) in b.entries.iter().enumerate() {
                out.push_str(&format!("entry {i} name={:?} kind={:?} open={}\n", e.name, e.kind, e.open));
            }
        }
        if r.tab == Tab::Program {
            let p = &r.progs;
            out.push_str(&format!("programs pick={} loading={} count={}\n", dash(p.pick.map(|p| p.to_string())), p.loading, p.list.len()));
            for (i, g) in p.list.iter().enumerate() {
                out.push_str(&format!("program {i} name={:?} then={} now={} data={}\n", g.name, dash(g.version.clone()), dash(g.now.clone()), g.data));
            }
        }
        if let Some((d, a)) = &r.dialog {
            out.push_str(&format!("dialog {} title={:?} ok={:?} on_ok={}\n", a.name(), d.title, d.ok, d.on_ok));
            for l in &d.lines {
                out.push_str(&format!("line {l:?}\n"));
            }
        }
        let im = &r.images;
        out.push_str(&format!("view draws={} draw_max_ms={:.1} draw_px={}\n", im.draws, im.draw_max_ms, im.draw_px));
        out.push_str("end");
        out
    }

    /// `ctl hot`: the strip's button (`-` where the preset has none) and, while open, every part of the view.
    pub fn rewind_hot(&self) -> String {
        let s = self.theme.scale;
        let mid = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        let at = self.buehne().and_then(|b| b.rewind).map_or("-".to_string(), mid);
        let mut out = format!("rewind at={at} avail={} open={}\n", self.rewind.avail.name(), self.rewind.open);
        if !self.rewind.open {
            return out;
        }
        let r = &self.rewind;
        let (a, b) = act_texts(r);
        for (h, rect) in self.rewind_parts() {
            let what = match h {
                Hot::Back => "back".to_string(),
                Hot::Now => "now".into(),
                Hot::Older => "older".into(),
                Hot::Newer => "newer".into(),
                Hot::Chip(i) => format!("chip {i} id={} pick={}", r.snaps[i].id, i == r.pick),
                Hot::Way(t) => format!("way {} chosen={}", t.name(), t == r.tab),
                Hot::Up => "up".into(),
                Hot::Row(i) if r.tab == Tab::Path => format!("row {i} name={:?} pick={}", r.browse.entries[i].name, r.browse.pick == Some(i)),
                Hot::Row(i) => format!("row {i} name={:?} pick={}", r.progs.list[i].name, r.progs.pick == Some(i)),
                Hot::Do => format!("do text={a:?}"),
                Hot::Restart => format!("restart text={a:?}"),
                Hot::Undo => format!("undo text={b:?}"),
                Hot::DialogCancel => "dialog cancel".into(),
                Hot::DialogOk => "dialog ok".into(),
            };
            out.push_str(&format!("rewind {what} at={}\n", mid(rect)));
        }
        out
    }
}

/// `ctl rewind open|close|state|probe`.
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    if words.first() != Some(&"rewind") {
        return None;
    }
    Some(match words.get(1).copied() {
        Some("open") => {
            if let Some(r) = app.rewind.avail.reason() {
                Err(format!("rewind: not opened: {} ({r})", app.rewind.avail.name()))
            } else {
                app.rewind_open("ctl");
                if app.rewind.open { Ok(app.rewind_state()) } else { Err(format!("rewind: not opened: {}", app.rewind_refused().unwrap_or("?"))) }
            }
        }
        Some("close") => {
            app.rewind_close("ctl");
            Ok(app.rewind_state())
        }
        Some("probe") => {
            app.rewind_probe("ctl");
            Ok(app.rewind_state())
        }
        Some("state") | None => Ok(app.rewind_state()),
        _ => Err("usage: rewind open|close|state|probe".into()),
    })
}

#[cfg(test)]
#[path = "rewind_app_tests.rs"]
mod tests;
