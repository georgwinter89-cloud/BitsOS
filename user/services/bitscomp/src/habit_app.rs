//! DB4 in bitscomp: what the desktop tells the one memory (ctxd) and what it does with what ctxd learned.
//! Sent (fire-and-forget, `habit_client`): a program's start and end (its first window opened, its last one
//! closed), each window's close with its state (maximised, fullscreen, tiled with its width, floating) and
//! workspace, picks in the address bar and the library, the sofa mode, sound per program (the sofa quick menu).
//! Done: a learned window state or tile width is imitated at the window's first configure, before Georg sees it
//! (never a window he looks at), with a one-time hint and "Rückgängig" (`habit_hint`); the rest only ranks and
//! fills "Gerade passend" (`passend`) and the page "Was BitsOS über dich gelernt hat" (`gelernt`).

use std::collections::HashMap;

use mcpreg::Json;

use crate::habit_client::{self as hc, Job, Msg, Tag};
use crate::layout::Place;
use crate::state::App;
use crate::win::Window;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rule {
    pub id: String,
    pub app: String,
    pub state: Option<String>,
    pub width: Option<f64>,
    pub hinted: bool,
    pub title: String,
    pub reason: String,
}

/// One entry of "Gerade passend" (C4 `suggest`).
#[derive(Clone, Debug, PartialEq)]
pub struct Pass {
    pub kind: String,
    pub app: Option<String>,
    pub action: Option<String>,
    pub name: String,
    pub reason: String,
}

/// One entry of the learned list (C4 `learned_list`).
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub part: String,
    pub title: String,
    pub reason: String,
    pub state: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Learned {
    pub loaded: bool,
    pub enabled: bool,
    /// (part, German name, on).
    pub parts: Vec<(String, String, bool)>,
    pub entries: Vec<Entry>,
}

#[derive(Default)]
pub struct Habit {
    pub client: Option<hc::Client>,
    pub rules: Vec<Rule>,
    /// Pick frecency per address (the address bar's programs are ranked by it), and the newest from ctxd: those
    /// are taken only while the address bar is closed (its order never changes under the pointer).
    pub ranks: HashMap<String, f64>,
    pub ranks_new: HashMap<String, f64>,
    /// The newest suggestions, and the ones shown: those change only while nothing shows them.
    pub passend: Vec<Pass>,
    pub shown: Vec<Pass>,
    pub learned: Learned,
    pub hint: Option<crate::habit_hint::Hint>,
    /// Hints behind the one shown (two learned programs opened together): each is shown in turn.
    pub waiting: Vec<crate::habit_hint::Waiting<Window>>,
    /// Pages asking before "Alles vergessen" (pid -> what is forgotten).
    pub asking: HashMap<u32, String>,
    /// The last answer line about a forget, for the page and `ctl habits`.
    pub said: Option<String>,
    /// VORRANG: the learned priorities, the open question about a hoch, refusals already told (once per run).
    pub vorrang: Vec<crate::habit_vorrang::VRule>,
    pub ask: Option<crate::habit_ask::Ask>,
    pub vorrang_said: std::collections::HashSet<String>,
    sent: u64,
}

impl Habit {
    /// A hint was shown: not again for this rule, also before ctxd's next answer.
    pub fn hinted(&mut self, id: &str) {
        for r in self.rules.iter_mut().filter(|r| r.id == id) {
            r.hinted = true;
        }
        for r in self.vorrang.iter_mut().filter(|r| r.id == id) {
            r.hinted = true;
        }
    }

    /// "Rückgängig": the rule is not imitated again, also before ctxd's next answer.
    pub fn undone(&mut self, id: &str) {
        self.rules.retain(|r| r.id != id);
        self.vorrang.retain(|r| r.id != id);
    }
}

fn s(j: &Json, k: &str) -> String {
    j.get(k).and_then(Json::as_str).unwrap_or("").to_string()
}

fn opt(j: &Json, k: &str) -> Option<String> {
    j.get(k).and_then(Json::as_str).map(str::to_string)
}

/// The rules of a `habit_rules` answer.
pub fn rules_of(r: &Json) -> (Vec<Rule>, HashMap<String, f64>) {
    let rules = r.get("rules").and_then(Json::as_array).unwrap_or(&[]).iter().filter(|j| j.get("vorrang").is_none()).map(|j| Rule {
        id: s(j, "id"),
        app: s(j, "app"),
        state: opt(j, "state"),
        width: j.get("width").and_then(Json::as_f64),
        hinted: j.get("hinted").and_then(Json::as_bool).unwrap_or(false),
        title: s(j, "title"),
        reason: s(j, "reason"),
    });
    let ranks = match r.get("ranks") {
        Some(Json::Obj(v)) => v.iter().filter_map(|(k, x)| Some((k.clone(), x.as_f64()?))).collect(),
        _ => HashMap::new(),
    };
    (rules.collect(), ranks)
}

pub fn passes_of(r: &Json) -> Vec<Pass> {
    r.as_array()
        .unwrap_or(&[])
        .iter()
        .map(|j| Pass { kind: s(j, "kind"), app: opt(j, "app"), action: opt(j, "action"), name: s(j, "name"), reason: s(j, "reason") })
        .collect()
}

pub fn learned_of(r: &Json) -> Learned {
    let parts = r.get("parts").and_then(Json::as_array).unwrap_or(&[]).iter().map(|p| (s(p, "part"), s(p, "name"), p.get("on").and_then(Json::as_bool).unwrap_or(true))).collect();
    let entries = r
        .get("entries")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .map(|e| Entry { id: s(e, "id"), part: s(e, "part"), title: s(e, "title"), reason: s(e, "reason"), state: s(e, "state") })
        .collect();
    Learned { loaded: true, enabled: r.get("enabled").and_then(Json::as_bool).unwrap_or(true), parts, entries }
}

/// Marks a window of a restored session (`place_new`).
pub struct Restored;

/// Marks a window that opened as a dialog: it stays one at its close, also when its parent went first.
struct Child;

/// The local hour now.
pub fn hour() -> u64 {
    let secs = crate::status::epoch_secs() + crate::status::offset_min() as i64 * 60;
    (secs.rem_euclid(86_400) / 3600) as u64
}

/// A dialog or another child window of a program (an xdg parent, an X11 transient_for): only main windows
/// teach and get imitated, a program's "Speichern unter" keeps its place.
pub fn is_child(w: &Window) -> bool {
    w.toplevel().is_some_and(|t| t.parent().is_some()) || w.x11().is_some_and(|x| x.transient_for.get() != 0)
}

/// What bitscomp knows about a window as it closes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Closed {
    /// On the sofa mode's workspace: the sofa mode made it fullscreen, not Georg.
    pub sofa: bool,
    pub child: bool,
    /// Minimised: its place now says nothing about how Georg uses it.
    pub minimised: bool,
    /// Not found on any workspace.
    pub lost: bool,
    pub maximised: bool,
    pub fullscreen: bool,
    pub place: Option<Place>,
    pub weight: Option<f32>,
}

impl Closed {
    /// The state this close teaches (None: it is history only).
    pub fn taught(&self) -> Option<(&'static str, Option<f32>)> {
        if self.sofa || self.child || self.minimised || self.lost {
            return None;
        }
        Some(close_state(self.maximised, self.fullscreen, self.place, self.weight))
    }
}

/// The state a window is closed in, and its tile's width when it has a tile of its own.
pub fn close_state(maximised: bool, fullscreen: bool, place: Option<Place>, weight: Option<f32>) -> (&'static str, Option<f32>) {
    match (fullscreen, maximised, place) {
        (true, ..) => ("fullscreen", None),
        (_, true, _) => ("maximised", None),
        (_, _, Some(Place::Tiled { depth: 1, .. })) => ("tiled", weight),
        (_, _, Some(Place::Tiled { .. })) => ("tiled", None),
        _ => ("floating", None),
    }
}

impl App {
    pub fn habit_start(&mut self, handle: &smithay::reexports::calloop::LoopHandle<'static, App>) {
        match hc::start(handle, hc::socket()) {
            Ok(c) => self.habit.client = Some(c),
            Err(e) => eprintln!("bitscomp: habits: no client ({e}); nothing is learned or imitated"),
        }
    }

    fn habit_job(&self, job: Job) {
        if let Some(c) = &self.habit.client {
            c.send(job);
        }
    }

    /// One thing Georg did, to ctxd (fire-and-forget).
    pub fn habit_event(&mut self, kind: &str, mut args: Vec<(&str, Json)>) {
        args.insert(0, ("kind", Json::str(kind)));
        args.push(("hour", Json::Num(hour() as f64)));
        self.habit.sent += 1;
        self.habit_job(Job::Event(hc::line("habit_event", Json::obj(args))));
    }

    /// Asks ctxd again: rules, suggestions (with the open programs), the learned list.
    pub fn habit_refresh(&mut self, _why: &str) {
        let open: Vec<Json> = self.habit_open_apps().into_iter().map(|a| Json::str(&a)).collect();
        self.habit_job(Job::Ask(Tag::Rules, hc::line("habit_rules", Json::obj(vec![]))));
        let args = Json::obj(vec![("hour", Json::Num(hour() as f64)), ("open_apps", Json::Arr(open))]);
        self.habit_job(Job::Ask(Tag::Suggest, hc::line("suggest", args)));
        self.habit_job(Job::Ask(Tag::Learned, hc::line("learned_list", Json::obj(vec![]))));
    }

    pub fn habit_forget(&mut self, pid: u32, id: &str) {
        eprintln!("bitscomp: habits: forget {id} (page {pid})");
        self.habit_job(Job::Ask(Tag::Forget(pid, id.to_string()), hc::line("learned_forget", Json::obj(vec![("id", Json::str(id))]))));
    }

    pub fn habit_msg(&mut self, m: Msg) {
        let (tag, reply) = match m {
            Msg::Reply(t, j) => (t, j),
            Msg::Failed(Tag::Forget(pid, id), e) => {
                eprintln!("bitscomp: habits: forget {id} failed: {e}");
                return self.gelernt_said(pid, "Das Gedächtnis antwortet gerade nicht; nichts gelöscht.".into());
            }
            Msg::Failed(..) => return,
        };
        let ok = reply.get("ok").and_then(Json::as_bool) == Some(true);
        let result = reply.get("result").cloned().unwrap_or(Json::Null);
        match tag {
            Tag::Rules if ok => {
                let (rules, ranks) = rules_of(&result);
                self.habit.rules = rules;
                self.habit.vorrang = crate::habit_vorrang::rules_of(&result);
                self.habit.ranks_new = ranks;
                self.passend_settle();
            }
            // Settled at every answer (every 30 s), so what came while the library was open is shown after it.
            Tag::Suggest if ok => {
                self.habit.passend = passes_of(&result);
                self.passend_settle();
            }
            Tag::Learned if ok => {
                let l = learned_of(&result);
                if l != self.habit.learned {
                    self.habit.learned = l;
                    self.gelernt_changed();
                }
            }
            Tag::Forget(pid, id) => {
                let text = if ok {
                    let n = result.get("forgotten").and_then(Json::as_u64).unwrap_or(0);
                    eprintln!("bitscomp: habits: forgot {id} ({n} records)");
                    if id == "all" { "Alles Gelernte ist vergessen.".to_string() } else { "Gelöscht.".to_string() }
                } else {
                    let why = reply.get("error").map(|e| s(e, "message")).unwrap_or_default();
                    eprintln!("bitscomp: habits: forget {id} refused: {why}");
                    "Das ließ sich nicht löschen.".to_string()
                };
                self.habit.rules.retain(|r| id != "all" && r.id != id);
                self.gelernt_said(pid, text);
                self.habit_refresh("forget");
            }
            _ => {}
        }
    }

    /// The programs with a window now (app ids).
    pub fn habit_open_apps(&self) -> Vec<String> {
        let mut v: Vec<String> = self.all_windows().iter().map(|w| crate::ctl::names(w).0).filter(|a| !a.is_empty() && !a.starts_with("bitscomp.")).collect();
        v.sort();
        v.dedup();
        v
    }

    pub(crate) fn habit_name(&self, app: &str) -> String {
        self.library.entries.iter().find(|e| e.owns(app)).map_or_else(|| app.to_string(), |e| e.name.clone())
    }

    /// A new window before its first configure (its app id is known): tell ctxd, and imitate what was learned.
    pub fn habit_initial(&mut self, w: &Window) {
        let app = crate::ctl::names(w).0;
        if app.is_empty() || app.starts_with("bitscomp.") {
            return;
        }
        let first = !self.all_windows().iter().any(|o| o != w && crate::ctl::names(o).0 == app);
        let name = self.habit_name(&app);
        let ws = self.desk.active as f64;
        let restored = w.user_data().get::<Restored>().is_some();
        self.habit_event("window_open", vec![("app", Json::str(&app)), ("name", Json::str(&name)), ("ws", Json::Num(ws)), ("restored", Json::Bool(restored))]);
        // VORRANG: a program's priority goes with its start (also a restored one's, also in the sofa mode).
        if first && !is_child(w) {
            self.vorrang_start(w, &app, &name);
        }
        // A restored session starts many programs at once: no "started together", no time, its places stay.
        if restored {
            return;
        }
        if first {
            self.habit_event("program_start", vec![("app", Json::str(&app)), ("name", Json::str(&name))]);
        }
        if is_child(w) {
            w.user_data().insert_if_missing(|| Child);
            return;
        }
        if self.sofa.active {
            return;
        }
        let Some(rule) = self.habit.rules.iter().find(|r| r.app == app).cloned() else { return };
        let Some(place) = self.ws.find(w) else { return };
        let undo = match (rule.state.as_deref(), rule.width, place) {
            (Some("maximised"), _, _) if !self.ws.is_max(w) => {
                let tiled = matches!(place, Place::Tiled { .. });
                if tiled {
                    self.ws.toggle_float(w, self.area());
                }
                self.ws.toggle_max(w, self.area_full());
                crate::habit_hint::Undo::Max { tiled }
            }
            (Some("fullscreen"), _, _) if !self.ws.is_fullscreen(w) => {
                self.window_fullscreen_why(w, true, "as learned");
                crate::habit_hint::Undo::Full
            }
            (None, Some(width), Place::Tiled { tile, depth: 1, .. }) => {
                let old = self.ws.tiles[tile].weight;
                self.ws.tiles[tile].weight = (width as f32).clamp(0.1, 10.0);
                crate::habit_hint::Undo::Width(old)
            }
            _ => return,
        };
        eprintln!("bitscomp: habits: window {} ({app}) as learned: {}", crate::ctl::win_id(w), rule.title);
        self.layout_dirty = true;
        self.apply_layout();
        if !rule.hinted {
            self.habit_hint_offer(w, rule, undo);
        }
    }

    /// The workspace holding `w` (its index and the workspace), the minimised ones and parked ones too.
    fn habit_ws_of(&self, w: &Window) -> Option<(usize, &crate::layout::Workspace<Window>)> {
        if self.ws.all_windows().any(|x| x == w) {
            return Some((self.desk.active, &self.ws));
        }
        (0..self.desk.parked.len()).find(|&i| i != self.desk.active && self.desk.parked[i].all_windows().any(|x| x == w)).map(|i| (i, &self.desk.parked[i]))
    }

    /// A window goes: its state to ctxd, and the program's end with its last window.
    pub fn habit_closed(&mut self, w: &Window) {
        let app = crate::ctl::names(w).0;
        if app.is_empty() || app.starts_with("bitscomp.") || w.toplevel().is_some_and(|t| !t.is_initial_configure_sent()) {
            return;
        }
        let child = is_child(w) || w.user_data().get::<Child>().is_some();
        let mut c = Closed { child, lost: true, ..Default::default() };
        let mut ws_no = self.desk.active;
        if let Some((i, ws)) = self.habit_ws_of(w) {
            let place = ws.find(w);
            ws_no = i;
            c = Closed {
                sofa: self.desk.names.get(i).is_some_and(|n| n == crate::sofa::WORKSPACE) || (self.sofa.active && i == self.desk.active),
                minimised: ws.is_minimized(w),
                lost: false,
                maximised: ws.is_max(w),
                fullscreen: ws.is_fullscreen(w),
                place,
                weight: match place {
                    Some(Place::Tiled { tile, .. }) => ws.tiles.get(tile).map(|t| t.weight),
                    _ => None,
                },
                ..c
            };
        }
        let mut args = vec![("app", Json::str(&app)), ("ws", Json::Num(ws_no as f64))];
        if let Some((state, width)) = c.taught() {
            args.push(("state", Json::str(state)));
            if let Some(x) = width {
                args.push(("tile", Json::Num((x as f64 * 1000.0).round() / 1000.0)));
            }
        }
        if let Some(p) = self.placements().into_iter().find(|p| p.win == *w) {
            let f = p.frame;
            args.extend([("x", Json::Num(f.x as f64)), ("y", Json::Num(f.y as f64)), ("w", Json::Num(f.w as f64)), ("h", Json::Num(f.h as f64))]);
        }
        self.habit_event("window_close", args);
        if !self.all_windows().iter().any(|o| o != w && crate::ctl::names(o).0 == app) {
            self.habit_event("program_stop", vec![("app", Json::str(&app))]);
        }
        // A waiting hint of this window is dropped unshown: the habit is hinted at its next start.
        self.habit.waiting.retain(|x| x.win != *w);
        if self.habit.hint.as_ref().is_some_and(|h| h.win == *w) {
            self.habit.hint = None;
            self.needs_frame = true;
            self.habit_hint_next();
        }
    }

    /// A pick in the address bar, the library or the sofa mode.
    pub fn habit_pick(&mut self, via: &str, target: &str) {
        let a = crate::address::normalise(target);
        let name = crate::library_view::entry_of(&self.library, &a).map_or_else(|| crate::suggest::short_name(&a), |e| e.name.clone());
        self.habit_event("pick", vec![("via", Json::str(via)), ("target", Json::str(&a)), ("name", Json::str(&name))]);
    }

    /// The sound of the program in front changed (sofa quick menu): its output and volume.
    pub fn habit_sound(&mut self, output: Option<&str>, volume: Option<u8>) {
        let Some(w) = self.sofa_running().map(|(w, _)| w).or_else(|| self.ws.focused().cloned()) else { return };
        let app = crate::ctl::names(&w).0;
        if app.is_empty() {
            return;
        }
        let mut args = vec![("app", Json::str(&app)), ("name", Json::str(&self.habit_name(&app)))];
        if let Some(o) = output {
            args.push(("output", Json::str(o)));
        }
        if let Some(v) = volume {
            args.push(("volume", Json::Num(v as f64)));
        }
        self.habit_event("sound", args);
    }

    pub fn habit_report(&self) -> String {
        let mut out = format!(
            "habits client={} sent={} dropped={} rules={} passend={} shown={} learned={} waiting={} hint={}\n",
            self.habit.client.is_some(),
            self.habit.sent,
            self.habit.client.as_ref().map_or(0, |c| c.dropped.get()),
            self.habit.rules.len(),
            self.habit.passend.len(),
            self.habit.shown.len(),
            self.habit.learned.entries.len(),
            self.habit.waiting.len(),
            self.habit.hint.as_ref().map_or("-".to_string(), |h| format!("{:?}", h.text)),
        );
        for r in &self.habit.rules {
            out.push_str(&format!("rule id={} app={} state={} width={} hinted={}\n", r.id, r.app, r.state.as_deref().unwrap_or("-"), r.width.map_or("-".into(), |w| format!("{w:.2}")), r.hinted));
        }
        out.push_str(&self.vorrang_report());
        for p in &self.habit.shown {
            out.push_str(&format!("passend kind={} app={} name={:?} reason={:?}\n", p.kind, p.app.as_deref().unwrap_or("-"), p.name, p.reason));
        }
        for e in &self.habit.learned.entries {
            out.push_str(&format!("learned id={} state={} title={:?}\n", e.id, e.state, e.title));
        }
        out.push_str("end");
        out
    }
}

/// `ctl habits [refresh|undo|forget ID|vorrang APP VALUE]`.
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    if words.first() != Some(&"habits") {
        return None;
    }
    Some(match words.get(1).copied() {
        None => Ok(app.habit_report()),
        Some("refresh") => {
            app.habit_refresh("ctl");
            Ok("ok".into())
        }
        Some("undo") => {
            if app.habit_hint_undo() { Ok("ok".into()) } else { Err("no hint shown".into()) }
        }
        Some("vorrang") => match (words.get(2), words.get(3)) {
            (Some(a), Some(v)) => app.vorrang_ctl(a, v),
            _ => Err("usage: habits vorrang APP niedrig|normal|hoch".into()),
        },
        Some("forget") => match words.get(2) {
            Some(id) => {
                app.habit_forget(0, id);
                Ok("ok".into())
            }
            None => Err("usage: habits forget ID|all".into()),
        },
        Some(o) => Err(format!("habits: unknown word {o}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_of_ctxd_become_rules_suggestions_and_entries() {
        let r = Json::parse(r#"{"rules":[{"id":"fenster:brave","app":"brave","state":"maximised","hinted":false,"title":"Brave startet maximiert","reason":"weil"}],"ranks":{"programm/steam":2.5}}"#).unwrap();
        let (rules, ranks) = rules_of(&r);
        assert_eq!(rules[0].state.as_deref(), Some("maximised"));
        assert_eq!(ranks["programm/steam"], 2.5);
        let p = passes_of(&Json::parse(r#"[{"kind":"program","app":"steam","name":"Steam","reason":"weil"}]"#).unwrap());
        assert_eq!(p[0].app.as_deref(), Some("steam"));
        let l = learned_of(&Json::parse(r#"{"enabled":true,"parts":[{"part":"fenster","name":"F","on":false}],"entries":[{"id":"a:b","part":"a","title":"T","reason":"R","state":"gelernt"}]}"#).unwrap());
        assert_eq!((l.parts[0].2, l.entries[0].title.as_str()), (false, "T"));
    }

    #[test]
    fn a_close_says_the_state_and_only_a_tile_of_its_own_its_width() {
        let tile = |depth| Some(Place::Tiled { tile: 0, index: 0, depth, top: 0 });
        assert_eq!(close_state(false, false, tile(1), Some(1.5)), ("tiled", Some(1.5)));
        assert_eq!(close_state(false, false, tile(2), Some(1.5)), ("tiled", None));
        assert_eq!(close_state(true, false, Some(Place::Floating { index: 0 }), None), ("maximised", None));
        assert_eq!(close_state(true, true, tile(1), Some(1.0)), ("fullscreen", None));
        assert_eq!(close_state(false, false, Some(Place::Floating { index: 0 }), None), ("floating", None));
        assert!(hour() < 24);
    }

    #[test]
    fn only_a_main_window_sized_by_georg_teaches_its_state() {
        let full = Closed { fullscreen: true, place: Some(Place::Floating { index: 0 }), ..Default::default() };
        assert_eq!(full.taught(), Some(("fullscreen", None)));
        assert_eq!(Closed { sofa: true, ..full }.taught(), None, "the sofa mode made it fullscreen, not Georg");
        assert_eq!(Closed { child: true, ..full }.taught(), None, "a dialog teaches nothing");
        let max = Closed { maximised: true, ..full };
        assert_eq!(Closed { minimised: true, fullscreen: false, place: None, ..max }.taught(), None, "a minimised window says nothing");
        assert_eq!(Closed { lost: true, ..max }.taught(), None);
        assert_eq!(Closed { fullscreen: false, ..max }.taught(), Some(("maximised", None)));
    }
}
