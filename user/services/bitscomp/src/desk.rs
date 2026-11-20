//! The desktop around the tiles (WY-5): real workspaces (each keeps its tiles), Claude's sidebar state, the
//! favourites and the name from the settings, what the pointer is over in the chrome, a short notice line.
//! The active workspace lives in `App::ws` (all window management works on it); the others are parked here.

use std::time::Duration;

use crate::win::Window;

use crate::layout::Workspace;
use crate::state::App;

/// Claude's ring (plan/spec/desktop.md §3). CL-3b: the conversation with the assistant drives it (`assist_app`);
/// without an assistant in the session only the control command `ring` sets it (`App::set_ring`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ring {
    Idle,
    Listening,
    Thinking,
    Working,
    Waiting,
}

impl Ring {
    #[cfg(test)]
    pub const ALL: [Ring; 5] = [Ring::Idle, Ring::Listening, Ring::Thinking, Ring::Working, Ring::Waiting];

    /// The state line under the name (the mock-up's "BEREIT").
    pub fn word(self) -> &'static str {
        match self {
            Ring::Idle => "BEREIT",
            Ring::Listening => "HÖRT ZU",
            Ring::Thinking => "DENKT",
            Ring::Working => "ARBEITET",
            Ring::Waiting => "WARTET AUF DICH",
        }
    }
}

/// Something of bitscomp's own the pointer can be over, besides window frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    Chrome(crate::chrome::Part),
    Suggest(usize),
    Start(crate::start::Part),
    /// A settings page (the pid of its window's program) and the part of it (WY-5b).
    Page(u32, crate::page::Part),
    /// CL-3b: a part of the conversation with the assistant (the sidebar, the Bühne's panel).
    Assist(crate::assist_layout::Part),
    /// DS-3: a part of the Bühne preset WY's chrome has no name for (sun, bell, Feld bar, strip, "+N").
    Buehne(crate::chrome_render::buehne::Part),
    /// A (12.11.): the question before a workspace with windows closes: Some(true) its "Schließen", Some(false)
    /// "Abbrechen", None the rest of the screen (the question is modal: nothing happens there).
    WsAsk(Option<bool>),
}

pub struct Desk {
    pub names: Vec<String>,
    /// One per workspace; the active one's entry is empty (its windows are in `App::ws`).
    pub parked: Vec<Workspace<Window>>,
    pub active: usize,
    /// The number the next new workspace is named with ("Arbeitsraum 3").
    pub next_no: usize,
    /// WY-5c: a folded bar shown over the tiles (pointer against its edge, `ctl reveal`).
    pub reveal: crate::reveal::Reveal,
    /// Fold changes sent to settingsd whose echo has not come back yet (key, value, when sent): an older echo
    /// must not undo a newer fold (`apply`).
    pub fold_sent: Vec<(String, bool, Duration)>,
    pub ring: Ring,
    pub favourites: Vec<String>,
    pub claude_name: String,
    pub hover: Option<Spot>,
    /// A press on a spot: it acts when released over the same spot.
    pub press: Option<Spot>,
    /// CL-3b: who pressed (the release alone does not say whose click it is).
    pub press_origin: crate::input::Origin,
    /// Shown below the address bar until the deadline (`pacer` time).
    pub notice: Option<(String, Duration)>,
    /// "Startseite" was asked for while the workspace has windows.
    pub start_forced: bool,
    /// B (12.11.): "Startseite" pressed where the start page is shown already: the Bühne's hint line says so on
    /// that workspace until the deadline (`pacer` time).
    pub home_note: Option<(usize, Duration)>,
    /// A (12.11.): the open question "Arbeitsraum N schließen?".
    pub ws_ask: Option<crate::ws_close::Ask>,
    /// C (12.11.): the list of minimised windows behind the bottom row's "+N".
    pub min_list: crate::min_row::MinList,
    /// DESK-LEFT: minimised windows closed by their x, until they are gone or their answer shows them.
    pub min_closing: Vec<crate::min_close::Closing>,
    /// The address bar being edited.
    pub edit: Option<crate::address::Edit>,
    /// The key held down while editing, repeated by bitscomp itself.
    pub held: Option<crate::address::Held>,
}

pub const NOTICE_FOR: Duration = Duration::from_secs(5);
/// WY-5c: the fold states are settings, so a restart keeps them.
pub const WORKSPACES_KEY: &str = "theme.layout.workspaces_bar";
pub const SIDEBAR_KEY: &str = "theme.layout.claude_sidebar";
/// How long a sent fold waits for settingsd's echo before it no longer holds back other changes of its key.
pub const FOLD_ECHO_FOR: Duration = Duration::from_secs(5);
/// The `pid` of the fold's settings writes (no settings page has it: pids start at 1).
pub const DESK_PID: u32 = 0;

impl Default for Desk {
    fn default() -> Self {
        Desk {
            names: vec![name(1)],
            parked: vec![Workspace::default()],
            active: 0,
            next_no: 2,
            reveal: Default::default(),
            fold_sent: Vec::new(),
            ring: Ring::Idle,
            favourites: vec!["programm/terminal".into(), "einstellungen/aussehen".into()],
            claude_name: "Claude".into(),
            hover: None,
            press: None,
            press_origin: crate::input::Origin::Injected,
            notice: None,
            start_forced: false,
            home_note: None,
            ws_ask: None,
            min_list: Default::default(),
            min_closing: Vec::new(),
            edit: None,
            held: None,
        }
    }
}

pub fn name(n: usize) -> String {
    format!("Arbeitsraum {n}")
}

/// "2 Kacheln · 3 Tabs", "leer".
pub fn summary(tiles: usize, tabs: usize) -> String {
    if tiles == 0 && tabs == 0 {
        return "leer".into();
    }
    let k = if tiles == 1 { "Kachel" } else { "Kacheln" };
    let t = if tabs == 1 { "Tab" } else { "Tabs" };
    format!("{tiles} {k} · {tabs} {t}")
}

fn counts(ws: &Workspace<Window>) -> (usize, usize) {
    // WY-12: a minimised window is in no tile but still open: it counts as a tab (and keeps the workspace).
    (ws.tiles.len() + ws.floating.len(), ws.all_windows().count())
}

impl App {
    pub fn ws_count(&self) -> usize {
        self.desk.names.len()
    }

    /// (tiles, tabs) of workspace `i`.
    pub fn ws_counts(&self, i: usize) -> (usize, usize) {
        if i == self.desk.active { counts(&self.ws) } else { self.desk.parked.get(i).map_or((0, 0), counts) }
    }

    /// A new empty workspace after the others; it becomes the shown one.
    pub fn ws_new(&mut self) -> usize {
        self.desk.names.push(name(self.desk.next_no));
        self.desk.next_no += 1;
        self.desk.parked.push(Workspace::default());
        let i = self.desk.names.len() - 1;
        self.ws_switch(i);
        self.absicht_register();
        i
    }

    pub fn ws_switch(&mut self, i: usize) -> bool {
        let a = self.desk.active;
        if i == a || i >= self.desk.names.len() {
            return false;
        }
        for w in self.ws.tiles.iter().flat_map(|t| t.stack.iter()).chain(self.ws.floating.iter().map(|f| &f.win)) {
            self.space.unmap_elem(w);
        }
        std::mem::swap(&mut self.ws, &mut self.desk.parked[a]);
        std::mem::swap(&mut self.ws, &mut self.desk.parked[i]);
        self.desk.active = i;
        self.desk.start_forced = false;
        self.menu = None;
        self.grab = None;
        self.hover = None;
        self.layout_dirty = true;
        self.chrome_dirty();
        self.session_dirty();
        true
    }

    /// REWIND: Win+Tab: the workspace `dir` steps on from the shown one, wrapping around. Not in the sofa mode
    /// (its screens and its own workspace are its world; Win+Esc leaves it); the library, the Feld and the rewind
    /// view close first,
    /// so nothing is switched to unseen under them.
    pub fn ws_cycle(&mut self, dir: i32) {
        if self.sofa.active {
            eprintln!("bitscomp: Win+Tab: not in the sofa mode");
            return;
        }
        self.library_close("Win+Tab");
        self.feld_close("Win+Tab");
        self.rewind_close("Win+Tab");
        let n = self.ws_count() as i64;
        if n < 2 {
            return;
        }
        let i = (self.desk.active as i64 + i64::from(dir)).rem_euclid(n) as usize;
        if self.ws_switch(i) {
            eprintln!("bitscomp: Win+Tab: workspace {} of {n}", i + 1);
            self.fx_play(crate::fx::plan::WIPE);
        }
    }

    /// Removes workspace `i` when it is empty and not the only one (GM-6a: `Sofa` not while the sofa mode is on).
    pub fn ws_remove(&mut self, i: usize) -> bool {
        if self.ws_count() < 2 || i >= self.ws_count() || self.ws_counts(i) != (0, 0) {
            return false;
        }
        if self.sofa.active && self.desk.names[i] == crate::sofa::WORKSPACE {
            return false;
        }
        if i == self.desk.active {
            self.ws_switch(if i == 0 { 1 } else { i - 1 });
        }
        self.desk.names.remove(i);
        self.desk.parked.remove(i);
        if self.desk.active > i {
            self.desk.active -= 1;
        }
        self.chrome_dirty();
        self.session_dirty();
        true
    }

    /// Windows of the parked workspaces (they are unmapped, but alive).
    pub fn parked_windows(&self) -> Vec<Window> {
        let mut v = Vec::new();
        for ws in &self.desk.parked {
            v.extend(ws.all_windows().cloned());
        }
        v
    }

    /// Takes a window out of every parked workspace (it is gone).
    pub fn forget_parked(&mut self, w: &Window) {
        for ws in &mut self.desk.parked {
            ws.remove(w);
        }
    }

    /// Claude's state on the ring: the assistant's conversation (CL-3b), the control command `ring` in tests.
    pub fn set_ring(&mut self, r: Ring) {
        if self.desk.ring != r {
            self.desk.ring = r;
            self.chrome_dirty();
        }
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.notice_for(text, NOTICE_FOR);
    }

    /// A notice that stays `dur` (CRASHDISK: the crash note; SELFOPT-1: the morning line).
    pub fn notice_for(&mut self, text: impl Into<String>, dur: Duration) {
        let until = crate::pacer::now() + dur;
        self.desk.notice = Some((text.into(), until));
        self.needs_frame = true;
    }

    /// Drops an expired notice; when the next one expires.
    pub fn notice_tick(&mut self, now: Duration) -> Option<Duration> {
        if self.desk.notice.as_ref().is_some_and(|(_, t)| now >= *t) {
            self.desk.notice = None;
            self.needs_frame = true;
        }
        if self.desk.home_note.is_some_and(|(_, t)| now >= t) {
            self.desk.home_note = None;
            self.chrome_dirty();
        }
        let next = [self.desk.notice.as_ref().map(|(_, t)| *t), self.desk.home_note.map(|(_, t)| t)];
        next.into_iter().flatten().min()
    }

    /// WY-5c: docks (`open`) or folds the bar of `key` at once (a local value, as `ctl set` sets one) and keeps it
    /// in settingsd, so a restart keeps it; without settingsd it stays local. A reveal ends: the bar is docked
    /// (pinned) or gone.
    pub fn set_bar(&mut self, key: &str, open: bool) {
        let value = if open { "true" } else { "false" };
        self.desk.reveal.clear();
        self.settings.set(key, value);
        self.apply_settings(key);
        let now = crate::pacer::now();
        let req = crate::settings_write::Req::set(key, mcpreg::Json::Bool(open));
        let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(DESK_PID, req));
        if sent {
            self.fold_expire(now);
            self.desk.fold_sent.push((key.to_string(), open, now));
        }
        eprintln!("bitscomp: {key} = {value} ({})", if sent { "kept in settingsd" } else { "local only: settingsd not there" });
        self.layout_dirty = true;
    }

    /// settingsd reported `key`: true when the report is the echo of a fold sent since and a newer fold of the
    /// same key is still on its way (then the report must not be applied, it would undo the newer fold).
    pub fn fold_echo_held(&mut self, key: &str, now: Duration) -> bool {
        self.fold_expire(now);
        let Some(i) = self.desk.fold_sent.iter().position(|(k, _, _)| k == key) else { return false };
        self.desk.fold_sent.remove(i);
        self.desk.fold_sent.iter().any(|(k, _, _)| k == key)
    }

    /// Folds whose echo did not come within `FOLD_ECHO_FOR` no longer wait for it (R1: a stalled settingsd may
    /// never answer, so the list may not grow with every fold).
    fn fold_expire(&mut self, now: Duration) {
        self.desk.fold_sent.retain(|(key, open, at)| {
            let waiting = now < *at + FOLD_ECHO_FOR;
            if !waiting {
                eprintln!("bitscomp: settingsd did not echo {key} = {open} within {} s; it stays as set", FOLD_ECHO_FOR.as_secs());
            }
            waiting
        });
    }

    /// settingsd refused a fold (or did not answer): that fold no longer waits for its echo; it stays local.
    pub fn fold_refused(&mut self, key: &str) {
        if let Some(i) = self.desk.fold_sent.iter().position(|(k, _, _)| k == key) {
            self.desk.fold_sent.remove(i);
        }
        if key.starts_with("sofa.") {
            return eprintln!("bitscomp: sofa: settingsd refused the sofa keys: their defaults (and `ctl set`) apply");
        }
        // PK-L1b: a pin from the library that settingsd refused stays on this screen only.
        if key == crate::page::FAVOURITES {
            eprintln!("bitscomp: library: settingsd refused the favourites: they stay local until a restart");
            return self.notice("Favoriten nicht gespeichert: der Einstellungsdienst hat abgelehnt (gilt nur bis zum Neustart).");
        }
        eprintln!("bitscomp: {key} not kept in settingsd: the fold stays local");
    }

    /// The pointer moved: a folded bar's edge starts the reveal timer, leaving a revealed bar folds it again. No
    /// reveal while a button is held for a drag, the circle menu is open, or a floating window lies at the edge.
    pub fn reveal_motion(&mut self, now: Duration) {
        let (x, y) = (self.pointer.x, self.pointer.y);
        let ch = self.chrome();
        // WY-8: no bar comes over a fullscreen window (it is the client's whole screen).
        let busy = self.grab.is_some() || self.menu.is_some() || self.desk.press.is_some() || self.ws.fullscreen_shown().is_some();
        let at = if busy || self.float_at(x, y) { None } else { ch.edge_at(x, y) };
        // The open "+N" list lies beside the revealed list (and the gap between them belongs to it).
        let left = ch.overlay().filter(|o| *o == ch.list);
        let list = left.zip(self.min_list_area()).map(|(o, a)| crate::layout::Rect { x: o.x + o.w, w: a.x + a.w - (o.x + o.w), ..a });
        let inside = ch.overlay().is_some_and(|r| r.contains(x, y)) || list.is_some_and(|r| r.contains(x, y));
        if self.desk.reveal.motion(now, at, inside) {
            eprintln!("bitscomp: revealed bar folds again (pointer left it)");
            self.desk.min_list.open = false;
            self.chrome_dirty();
            self.layout_dirty = true;
        }
    }

    /// Shows a bar whose edge the pointer was pushed against long enough; returns when to look again.
    pub fn reveal_tick(&mut self, now: Duration) -> Option<Duration> {
        if self.desk.reveal.tick(now) {
            eprintln!("bitscomp: folded bar revealed: {:?} (pointer at the edge)", self.desk.reveal.shown);
            self.chrome_dirty();
            // The pointer may be on it already: hover follows.
            self.motion(self.now_ms());
        }
        self.desk.reveal.due()
    }

    /// Whether the active workspace shows the start page.
    pub fn start_shown(&self) -> bool {
        self.desk.start_forced || (self.ws.tiles.is_empty() && self.ws.floating.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_german() {
        assert_eq!(summary(0, 0), "leer");
        assert_eq!(summary(1, 1), "1 Kachel · 1 Tab");
        assert_eq!(summary(3, 5), "3 Kacheln · 5 Tabs");
        assert_eq!(name(2), "Arbeitsraum 2");
    }
}

#[cfg(test)]
#[path = "ws_cycle_tests.rs"]
mod cycle_tests;
