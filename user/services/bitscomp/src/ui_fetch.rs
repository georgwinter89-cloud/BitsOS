//! DB3 part 0: the glue between DB1's list and DB5a's bridge for foreign programs (contract C3). atspid may take up
//! to 10 s to read a big program, so the list never waits for it on the main loop: an explicit request (`ui`,
//! `ui <window>`, MCP `ui_list`, Durchblick) first sends C3 `tree` with the fields atspid uses (bitscomp's window
//! id, the surface's origin and size on the desktop, program and title) from a thread of its own with a limit of
//! `WAIT`, keeps each answer per window in a small cache, and is answered once that is done. Georg's own clicks
//! (the history's element under the pointer) only ever read that cache. Acting on such an element (`ui-act`) goes
//! to C3 `act` the same way, off the main loop.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};
use uilist::{Element, Window as UiWindow};

use crate::layout::Rect;
use crate::state::App;
use crate::ui_reg::Full;

/// atspid answers within 10 s (its own bound); a little more for the way.
pub const WAIT: Duration = Duration::from_secs(12);
/// How long a tree serves Georg's clicks and acting on it without being read again.
pub const FRESH: Duration = Duration::from_secs(60);
/// At most this many questions to atspid at once; more requests are answered from the cache at once.
const JOBS_MAX: usize = 8;
/// The word an act that went to its thread answers in place (never shown: the real answer follows).
pub const LATER: &str = "\u{0}later";

/// One window whose tree is asked for: what C3 `tree` gets.
#[derive(Clone, Debug, PartialEq)]
pub struct Need {
    pub window: String,
    pub pid: i32,
    pub app: String,
    pub title: String,
    /// The surface's top left on the desktop (logical px, client-side shadows included: `window_origin`).
    pub origin: [i32; 2],
    /// The surface's size in the same pixels.
    pub size: [i32; 2],
    /// EINGABE2 (D): how long atspid may wait for the window to settle (ms), and whether it must stay unchanged
    /// between two reads (after a step).
    pub settle: Option<(u64, bool)>,
    /// Asked once more already because the window's title changed while atspid read it.
    pub again: bool,
}

/// D: a plain list waits only while title and page disagree or the page loads.
pub const SETTLE_LIST: u64 = 1500;
/// D: after a step, until the window holds still (shorter when nothing changes).
pub const SETTLE_STEP: u64 = 3000;

impl Need {
    pub fn args(&self) -> serde_json::Value {
        let mut a = serde_json::json!({"pid": self.pid, "window": self.window, "app": self.app, "title": self.title, "origin": self.origin, "size": self.size});
        if let Some((ms, calm)) = self.settle {
            a["settle"] = serde_json::json!({"ms": ms, "calm": calm});
        }
        a
    }
}

struct Cached {
    pid: i32,
    origin: [i32; 2],
    at: Instant,
    /// atspid said the page still loads (or the tree still holds the old one).
    loading: bool,
    /// The provider's answer: element rects in desktop logical px for `origin`, the provider's own ids.
    tree: UiWindow,
}

/// Where an answer goes once the thread is done.
pub enum To {
    Ctl(UnixStream),
    Mcp(SyncSender<mcpreg::Reply>),
    /// DB5b, EINS-B: a step of the acting task of this number (`db_act_done`).
    Db(u64),
}

pub enum Then {
    /// A control socket `ui ...` line, answered by running it again on the fresh cache.
    CtlList(String, UnixStream),
    /// MCP `ui_list` of this window (None: the focused one's elements).
    McpList(Option<String>, SyncSender<mcpreg::Reply>),
    /// An act: its answer, and what goes into the history once it is done. `fields`: the window's fields before (B).
    Act { to: To, window: Box<UiWindow>, element: Box<Element>, kind: String, text: Option<String>, since: Instant, fields: Vec<crate::desk_write_app::FieldBefore> },
    /// The popup of this generation asked for its window (`db_app`).
    Panel(u64),
    /// EINGABE: the desktop's write of this number checks that its field still has the focus.
    Focused(u64),
    /// EINGABE2: the desktop's write of this number had all of its field's text selected.
    Selected(u64),
    /// EINGABE2 (B): an agent's write is answered once the window was read again.
    Verify { to: To, window: String, element: String, name: String, text: String, fields: Vec<crate::desk_write_app::FieldBefore>, secret: bool },
    /// EINS-B: the desktop tool `sehen` (call id, window, with a picture), answered with the window read again.
    Desk(String, String, bool),
    /// EINS-B: the acting task of this number read its step's window again (`db_act_listed`).
    ActRead(u64),
}

enum Work {
    Trees(Vec<Need>),
    Act { pid: i32, args: serde_json::Value },
    Focused { args: serde_json::Value },
    Select { args: serde_json::Value },
}

pub enum Done {
    Trees(Vec<(Need, Result<(UiWindow, bool), String>)>),
    /// Ok(true): the field has the focus and the desktop writes the text (EINGABE, C3 `"by":"desktop"`).
    Act(Result<bool, String>),
    /// Whether the field still has the focus (false also when atspid could not tell).
    Focused(bool),
    /// Whether all of the field's text is selected (false also when atspid could not tell).
    Selected(bool),
}

/// atspid's answer to an act: done by the program, done by the desktop (true), or why not.
pub fn acted(v: &serde_json::Value) -> Result<bool, String> {
    match v.get("ok").and_then(serde_json::Value::as_bool) {
        Some(false) => Err(v.get("error").and_then(|e| e.as_str()).unwrap_or("Das ging nicht.").to_string()),
        _ => Ok(v.get("by").and_then(serde_json::Value::as_str) == Some("desktop")),
    }
}

#[derive(Default)]
pub struct Fetch {
    cache: HashMap<String, Cached>,
    waiting: HashMap<u64, Then>,
    next: u64,
    tx: Option<Sender<(u64, Done)>>,
    /// Set by a door (control socket, MCP) just before an act: where its answer goes when it must go to a thread.
    pub pending: Option<To>,
    /// What was said about the provider last (said once).
    said: Option<String>,
}

/// The C3 socket.
pub fn socket() -> std::path::PathBuf {
    crate::ui_client::ATSPI_SOCKET.into()
}

/// One C3 `tree`: the provider's window (scrubbed), or why not (atspid's `{"ok":false}`: switched off, no tree,
/// a slow program, refused).
pub fn ask_tree(path: &Path, need: &Need, wait: Duration) -> Result<(UiWindow, bool), String> {
    let v = crate::ui_client::atspi_ask_at(path, "tree", need.args(), wait)?;
    if v.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err(v.get("error").and_then(|e| e.as_str()).unwrap_or("kein Baum").to_string());
    }
    let loading = v.get("loading").and_then(serde_json::Value::as_bool) == Some(true);
    let mut w: UiWindow = serde_json::from_value(v).map_err(|e| format!("not a window: {e}"))?;
    uilist::scrub(&mut w.elements);
    Ok((w, loading))
}

fn work(path: &Path, w: Work) -> Done {
    match w {
        Work::Trees(needs) => Done::Trees(needs.into_iter().map(|n| {
            let r = ask_tree(path, &n, WAIT);
            (n, r)
        }).collect()),
        Work::Focused { args } => Done::Focused(
            crate::ui_client::atspi_ask_at(path, "focused", args, WAIT).is_ok_and(|v| v.get("focused").and_then(serde_json::Value::as_bool) == Some(true)),
        ),
        Work::Select { args } => Done::Selected(
            crate::ui_client::atspi_ask_at(path, "select", args, WAIT).is_ok_and(|v| v.get("selected").and_then(serde_json::Value::as_bool) == Some(true)),
        ),
        Work::Act { pid, args } => Done::Act(crate::ui_client::atspi_ask_at(path, "act", args, WAIT).and_then(|v| acted(&v)).map_err(|e| {
            eprintln!("bitscomp: ui: act in pid {pid}: {e}");
            e
        })),
    }
}

/// Moves the provider's rects (desktop logical px for `then`) to where the window's surface is `now`, in
/// physical px; an element whose centre is outside the window's content is not shown.
fn place(v: &mut [Element], shift: (i32, i32), content: Rect, s: f64) {
    for e in v {
        let [x, y, w, h] = e.rect;
        let r = Rect { x: x + shift.0, y: y + shift.1, w, h };
        let p = crate::header::px_rect(r, s);
        e.rect = [p.x, p.y, p.w, p.h];
        e.visible &= p.w > 0 && p.h > 0 && content.contains(r.x as f64 + w as f64 / 2.0, r.y as f64 + h as f64 / 2.0);
        place(&mut e.children, shift, content, s);
    }
}

impl Fetch {
    /// The provider's elements of window `window` (pid `pid`) as they lie now, when a fresh tree is cached.
    pub fn elements(&self, window: &str, pid: i32, origin: [i32; 2], content: Rect, s: f64) -> Option<Vec<Element>> {
        let c = self.cache.get(window).filter(|c| c.pid == pid && c.at.elapsed() < FRESH)?;
        let mut v = c.tree.elements.clone();
        crate::ui_client::mark_atspi(&mut v);
        place(&mut v, (origin[0] - c.origin[0], origin[1] - c.origin[1]), content, s);
        Some(v)
    }

    /// The provider's own id of our element `id` in window `window` (the stable ids are made from its ids).
    pub fn provider_id(&self, window: &str, pid: i32, id: &str) -> Option<String> {
        let c = self.cache.get(window).filter(|c| c.pid == pid && c.at.elapsed() < FRESH)?;
        let mut keyed = c.tree.clone();
        crate::ui_client::mark_atspi(&mut keyed.elements);
        keyed.window = window.to_string();
        keyed.number();
        let pos = keyed.walk().iter().position(|x| x.id == id)?;
        c.tree.walk().get(pos).map(|x| x.id.clone())
    }

    /// D: whether atspid said window `window`'s page still loads at its last read.
    pub fn loading(&self, window: &str) -> bool {
        self.cache.get(window).is_some_and(|c| c.loading)
    }

    fn keep(&mut self, need: Need, r: Result<(UiWindow, bool), String>) {
        match r {
            Ok((tree, loading)) => {
                self.said = None;
                self.cache.insert(need.window.clone(), Cached { pid: need.pid, origin: need.origin, at: Instant::now(), loading, tree });
            }
            Err(e) => {
                self.cache.remove(&need.window);
                let line = format!("window {} (pid {}) stays a picture: {e}", need.window, need.pid);
                if self.said.as_deref() != Some(line.as_str()) {
                    eprintln!("bitscomp: ui: {line}");
                    self.said = Some(line);
                }
            }
        }
        self.cache.retain(|_, c| c.at.elapsed() < FRESH * 10);
    }

    pub fn busy(&self) -> usize {
        self.waiting.len()
    }
}

impl App {
    pub fn ui_fetch_start(&mut self, handle: &LoopHandle<'static, App>) {
        let (tx, rx) = channel::channel::<(u64, Done)>();
        if let Err(e) = handle.insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg((token, done)) = ev {
                app.ui_fetch_done(token, done);
            }
        }) {
            eprintln!("bitscomp: ui: no channel for the provider's answers: {e} (foreign windows stay pictures)");
            return;
        }
        self.ui_fetch.tx = Some(tx);
    }

    /// The surface origin and size of window `w` drawn in `content` (logical px), as C3 takes them.
    pub(crate) fn ui_surface(&self, w: &crate::win::Window, content: Rect) -> ([i32; 2], [i32; 2]) {
        let g = w.geometry().loc;
        let b = w.bbox().size;
        ([content.x - g.x, content.y - g.y], [b.w.max(1), b.h.max(1)])
    }

    /// The windows `full` wants whose content only the provider knows (no own file, not bitscomp's page).
    pub fn ui_needs(&self, full: &Full) -> Vec<Need> {
        if self.ui_fetch.tx.is_none() || !socket().exists() {
            return Vec::new();
        }
        let focused = self.ui_focused_id();
        let mut out = Vec::new();
        for p in self.ui_drawn() {
            let id = crate::ui_reg::wid(&p.win);
            let wanted = match full {
                Full::Focused => id == focused,
                Full::One(w) => &id == w,
                Full::Local(_) => false,
            };
            let pid = self.pid_of(&p.win);
            if !wanted || !p.visible || pid <= 0 || self.page_pid(&p.win).is_some() || self.ui_source_of(pid) == uilist::Source::Client {
                continue;
            }
            let content = if p.fullscreen { p.frame } else { self.content_rect(p.frame) };
            let (origin, size) = self.ui_surface(&p.win, content);
            let (app, title) = crate::ctl::names(&p.win);
            out.push(Need { window: id, pid, app, title, origin, size, settle: Some((SETTLE_LIST, false)), again: false });
        }
        out
    }

    fn ui_job(&mut self, w: Work, then: Then) {
        let Some(tx) = self.ui_fetch.tx.clone() else { return };
        self.ui_fetch.next += 1;
        let token = self.ui_fetch.next;
        self.ui_fetch.waiting.insert(token, then);
        let path = socket();
        let spawned = std::thread::Builder::new().name("bitscomp-c3".into()).spawn(move || {
            let _ = tx.send((token, work(&path, w)));
        });
        if let Err(e) = spawned {
            eprintln!("bitscomp: ui: no thread for the provider: {e}");
            let done = match self.ui_fetch.waiting.get(&token) {
                Some(Then::Act { .. }) => Done::Act(Err("Gerade geht das nicht.".into())),
                Some(Then::Focused(_)) => Done::Focused(false),
                Some(Then::Selected(_)) => Done::Selected(false),
                _ => Done::Trees(Vec::new()),
            };
            self.ui_fetch_done(token, done);
        }
    }

    /// Asks the provider for `full`'s windows first and answers `then` afterwards; false when nothing needs
    /// asking (or too much is asked already): then the caller answers at once from what is known.
    pub fn ui_fetch_then(&mut self, full: &Full, then: impl FnOnce() -> Then) -> bool {
        let needs = self.ui_needs(full);
        if needs.is_empty() || self.ui_fetch.busy() >= JOBS_MAX {
            return false;
        }
        self.ui_job(Work::Trees(needs), then());
        true
    }

    /// D: as `ui_fetch_then`, after a step: atspid waits until the window holds still.
    pub fn ui_fetch_settled(&mut self, full: &Full, then: impl FnOnce() -> Then) -> bool {
        let mut needs = self.ui_needs(full);
        if needs.is_empty() || self.ui_fetch.busy() >= JOBS_MAX {
            return false;
        }
        for n in needs.iter_mut() {
            n.settle = Some((SETTLE_STEP, true));
        }
        self.ui_job(Work::Trees(needs), then());
        true
    }

    /// As `ui_fetch_then` with a ready answer: Err gives it back when nothing needs asking.
    pub fn ui_fetch_give(&mut self, full: &Full, then: Then) -> Result<(), Then> {
        if self.ui_needs(full).is_empty() || self.ui_fetch.busy() >= JOBS_MAX {
            return Err(then);
        }
        let needs = self.ui_needs(full);
        self.ui_job(Work::Trees(needs), then);
        Ok(())
    }

    /// Which windows a `ui_list` of `window` wants.
    pub fn ui_full_of(&self, window: Option<&str>) -> Full {
        match window {
            None => Full::Focused,
            Some("focus") => Full::One(self.ui_focused_id()),
            Some(w) => Full::One(w.to_string()),
        }
    }

    /// A control socket line: `ui ...` waits for the provider (then None: it is answered later); every other line
    /// comes back to be run as usual. `ui-act` on a foreign program's element is answered later too.
    pub fn ui_defer_ctl(&mut self, cmd: &str, stream: UnixStream) -> Option<UnixStream> {
        let words: Vec<&str> = cmd.split_whitespace().collect();
        match words.first().copied() {
            Some("ui") => {
                let rest = if words.get(1) == Some(&"text") { &words[2..] } else { &words[1..] };
                let full = self.ui_full_of(rest.first().copied());
                if self.ui_needs(&full).is_empty() || self.ui_fetch.busy() >= JOBS_MAX {
                    return Some(stream);
                }
                let cmd = cmd.to_string();
                self.ui_fetch_then(&full, move || Then::CtlList(cmd, stream));
                None
            }
            Some("ui-act") => {
                self.ui_fetch.pending = Some(To::Ctl(stream));
                let r = crate::ui_ctl::ctl(self, &words, cmd).unwrap_or_else(|| Err("ui-act".into()));
                match self.ui_fetch.pending.take() {
                    Some(To::Ctl(mut s)) => {
                        let answer = r.unwrap_or_else(|e| format!("error: {e}"));
                        let _ = s.write_all(format!("{answer}\n").as_bytes());
                        None
                    }
                    // Taken by an act that went to its thread.
                    _ => None,
                }
            }
            _ => Some(stream),
        }
    }

    /// An act on a foreign program's element: off the main loop when a door said where its answer goes.
    pub(crate) fn ui_act_provider(&mut self, w: &UiWindow, e: &Element, action: &str, text: Option<&str>) -> Result<String, String> {
        let theirs = self.ui_fetch.provider_id(&w.window, w.pid as i32, &e.id).ok_or_else(|| format!("Das Element {} kennt Durchblick nicht mehr; „ui {}“ liest das Fenster neu.", e.id, w.window))?;
        let mut args = serde_json::json!({"pid": w.pid, "element": theirs, "action": action});
        if let Some(t) = text {
            args["text"] = serde_json::Value::String(t.to_string());
        }
        let to = self.ui_fetch.pending.take().ok_or("Das geht nur über ui-act.")?;
        let fields = w.walk().into_iter().filter(|x| matches!(x.role, uilist::Role::Field | uilist::Role::Choice)).map(|x| (x.id.clone(), x.name.clone(), x.value.clone())).collect();
        let mut window = w.clone();
        window.elements.clear();
        if matches!(action, "type" | "submit") {
            self.desk_write_prewatch(&w.window);
        }
        let then = Then::Act { to, window: Box::new(window), element: Box::new(e.clone()), kind: action.into(), text: text.map(str::to_string), since: Instant::now(), fields };
        self.ui_job(Work::Act { pid: w.pid as i32, args }, then);
        Ok(LATER.into())
    }

    /// EINGABE: asks atspid whether `element` of window `w` still has the focus; the answer goes to write `seq`.
    pub(crate) fn ui_ask_focused(&mut self, w: &UiWindow, element: &str, seq: u64) -> bool {
        let Some(theirs) = self.ui_fetch.provider_id(&w.window, w.pid as i32, element) else { return false };
        self.ui_job(Work::Focused { args: serde_json::json!({"pid": w.pid, "element": theirs}) }, Then::Focused(seq));
        true
    }

    /// EINGABE2: asks atspid to select all of `element`'s text; the answer goes to write `seq`.
    pub(crate) fn ui_ask_select(&mut self, w: &UiWindow, element: &str, seq: u64) -> bool {
        let Some(theirs) = self.ui_fetch.provider_id(&w.window, w.pid as i32, element) else { return false };
        self.ui_job(Work::Select { args: serde_json::json!({"pid": w.pid, "element": theirs}) }, Then::Selected(seq));
        true
    }

    fn ui_fetch_done(&mut self, token: u64, done: Done) {
        let Some(then) = self.ui_fetch.waiting.remove(&token) else { return };
        match done {
            Done::Selected(yes) => {
                if let Then::Selected(seq) = then {
                    self.desk_write_selected(seq, yes);
                }
                return;
            }
            Done::Focused(yes) => {
                if let Then::Focused(seq) = then {
                    self.desk_write_focused(seq, yes);
                }
                return;
            }
            Done::Trees(all) => {
                // D: the program's title changed while atspid read the window (it may have read the old page by the
                // old title): once more with the new one.
                let mut again = Vec::new();
                for (need, r) in all {
                    let fresh = self.ui_needs(&Full::One(need.window.clone())).into_iter().next();
                    if let Some(mut f) = fresh.filter(|f| need.settle.is_some() && !need.again && f.title != need.title) {
                        (f.settle, f.again) = (need.settle, true);
                        again.push(f);
                    }
                    self.ui_fetch.keep(need, r);
                }
                if !again.is_empty() {
                    return self.ui_job(Work::Trees(again), then);
                }
            }
            Done::Act(r) => {
                let Then::Act { to, window, element, kind, text, since, fields } = then else { return };
                // EINGABE: the field has the keyboard; the desktop writes (or presses Enter), then answers.
                if r == Ok(true) {
                    let k = if kind == "submit" { crate::desk_write::Kind::Enter } else { crate::desk_write::Kind::Text };
                    return self.desk_write(to, window, element, text, since, k, fields);
                }
                if matches!(kind.as_str(), "type" | "submit") {
                    self.desk_write_unprewatch();
                }
                let r = r.map(|_| ());
                if let To::Db(g) = to {
                    return self.db_act_done(g, r);
                }
                let typed = text.as_deref().filter(|_| kind == "type" && !element.has(uilist::State::Secret));
                if r.is_ok() {
                    self.ui_note(&window, &element, &kind, typed, false);
                }
                answer(to, r.map(|()| "ok".to_string()));
                return;
            }
        }
        match then {
            Then::CtlList(cmd, mut s) => {
                let words: Vec<&str> = cmd.split_whitespace().collect();
                let r = crate::ui_ctl::ctl(self, &words, &cmd).unwrap_or_else(|| Err("ui".into()));
                let _ = s.set_nonblocking(false);
                let _ = s.write_all(format!("{}\n", r.unwrap_or_else(|e| format!("error: {e}"))).as_bytes());
            }
            Then::McpList(w, tx) => {
                let _ = tx.try_send(self.ui_mcp_list(w));
            }
            Then::Panel(generation) => self.db_list_ready(generation),
            Then::Act { to: To::Db(g), .. } => {
                self.desk_write_unprewatch();
                self.db_act_done(g, Err("Gerade geht das nicht.".into()))
            }
            Then::Act { to, .. } => {
                self.desk_write_unprewatch();
                answer(to, Err("Gerade geht das nicht.".into()))
            }
            Then::Focused(seq) => self.desk_write_focused(seq, false),
            Then::Selected(seq) => self.desk_write_selected(seq, false),
            Then::Verify { to, window, element, name, text, fields, secret } => self.desk_write_verified(to, &window, &element, &name, &text, &fields, secret),
            Then::Desk(call, window, picture) => self.desk_see_ready(&call, &window, picture),
            Then::ActRead(seq) => self.db_act_listed(seq),
        }
    }
}

pub(crate) fn answer(to: To, r: Result<String, String>) {
    match to {
        To::Ctl(mut s) => {
            let _ = s.write_all(format!("{}\n", r.unwrap_or_else(|e| format!("error: {e}"))).as_bytes());
        }
        To::Mcp(tx) => {
            let _ = tx.try_send(match r {
                Ok(t) => mcpreg::Reply::text(t),
                Err(e) => mcpreg::Reply::error(e),
            });
        }
        To::Db(_) => {}
    }
}

#[cfg(test)]
#[path = "ui_fetch_tests.rs"]
mod tests;
