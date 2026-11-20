//! DB3, EINS-B (PLAN K1, K4): the popup "Durchblick": a view of the one assistant. The key (Win+F1, gamepad
//! Select+Y, `durchblick.taste`/`durchblick.pad`) opens it over the window that had the keyboard, with the input
//! line focused; each opening is a new chat (origin popup) of `assist serve`. Enter sends what is typed, Enter on
//! the empty line "Erklär mir, was ich hier sehe" (board 1). The first question carries the window: its element
//! list and its picture, without a preview or a picture question; a never-see program's window carries nothing and
//! the popup says so. Every question is a journal line (kinds, bytes, to whom; never the content). The answer
//! names element ids, which are framed on the screen (`db_draw`); a tool's question and a step of `handeln` are
//! asked here (board 2) when this popup shows the chat. Esc stops a running step first, else closes.

use std::time::Instant;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};
use uilist::{State, Window as UiWindow};

use crate::assist::ToolState;
use crate::assist_level::{self as lv, Level};
use crate::assist_route::NewFor;
use crate::db_layout::Button;
use crate::db_question::{self as q, Kind};
use crate::db_settings as set;
use crate::db_shot::Shot;
use crate::db_worker::Msg;
use crate::input::Origin;
use crate::layout::Rect;
use crate::state::App;
use crate::ui_reg::Full;

/// A yes counts only this long after the question appeared (a click meant for what was there before never says it).
pub use crate::install_dialog::ARM;
pub const REAL_INPUT: &str = "Ja geht nur mit deiner echten Maus, Tastatur oder deinem Gamepad.";
/// The reason `db_open` gets from the gamepad's chord.
pub const PAD_OPEN: &str = "gamepad";
/// The sofa keyboard's field for a question.
pub const SOFA_FIELD: &str = "durchblick.frage";
/// Messages of the channel that belong to no popup (the acting task's, the desktop tools' pictures).
pub const ACT_GEN: u64 = u64::MAX;
/// How long the popup waits for its chat or its first event.
pub const CHAT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
pub const NO_HELPER: &str = "Der Assistent ist auf diesem Rechner nicht da, darum kann Durchblick gerade nichts erklären.";
pub const NO_ANSWER: &str = "Der Assistent hat keine Antwort gegeben.";
/// After a delete in the popup (board 5's sentence and the way to the listing).
pub const TRASH_NOTE: &str = "Im Papierkorb, bis du ihn leerst. Zeigen: in der Seitenleiste oder im Terminal mit „bits papierkorb liste“.";

#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// The input line waits for a question (board 1).
    Input,
    Waiting,
    Answering,
    Done,
    Failed(String),
    /// A tool's question or a step waits for Georg's Ja (board 2).
    Step,
    /// A step runs (or the Automodus waits a moment before it).
    Acting,
    /// No assistant to ask: why, and the way to the page "KI".
    Setup(String),
}

impl Stage {
    pub fn word(&self) -> &'static str {
        match self {
            Stage::Input => "input",
            Stage::Waiting => "waiting",
            Stage::Answering => "answering",
            Stage::Done => "done",
            Stage::Failed(_) => "failed",
            Stage::Step => "step",
            Stage::Acting => "acting",
            Stage::Setup(_) => "setup",
        }
    }
}

/// A question of the engine's own (a tool asks, PLAN K2), shown as the popup's step.
#[derive(Clone, Debug, PartialEq)]
pub struct Asking {
    pub ask_id: u64,
    pub name: String,
    pub text: String,
}

pub struct Db {
    pub open: bool,
    pub generation: u64,
    pub target: String,
    pub snapshot: Option<UiWindow>,
    pub pointed: Option<String>,
    pub stage: Stage,
    /// The chat this popup shows (None: asked for, not made yet).
    pub chat: Option<String>,
    /// When the chat was asked for (a service that never answers is said after `CHAT_WAIT`).
    chat_asked: Option<Instant>,
    /// The question typed before the chat was there.
    pending: Option<(String, bool)>,
    /// The asked question's id and text, and whether injected input asked it.
    pub ask_id: u64,
    pub question: String,
    pub remote: bool,
    /// The window went with the first question (follow-ups carry none: the model reads it with `sehen`).
    pub context_sent: bool,
    /// The window's program is on the never-see list: nothing of it goes out.
    pub hidden: bool,
    list_ready: bool,
    pub want_shot: Option<Rect>,
    /// Whether anything was drawn over the window in the frame the picture comes from (None: no frame yet).
    pub shot_cover: Option<bool>,
    /// When the picture was asked for: a frame that never comes does not hold the panel up for long.
    pub shot_asked: Option<Instant>,
    pub shot_covered: bool,
    shot_pending: bool,
    pub shot: Option<Shot>,
    pub answer: String,
    pub shown: String,
    pub ids: Vec<String>,
    /// The tools' lines of the running answer (name, sentence, how far).
    pub tools: Vec<(String, String, ToolState)>,
    pub asking: Option<Asking>,
    pub note: Option<String>,
    pub field: crate::address::Edit,
    /// The button with the keyboard's focus (None: the input line).
    pub focus: Option<Button>,
    /// The level's three choices are open in place of the pill.
    pub level_open: bool,
    pub(crate) since: Instant,
    pub scroll: usize,
    pub(crate) tx: Option<Sender<(u64, Msg)>>,
    pub images: crate::db_draw::Images,
    /// The sofa mode's program was in front when the on-screen keyboard came for a question.
    sofa_program: bool,
    /// The access log of the window's program (read on this machine; it goes along with a question about it).
    access: Option<String>,
    access_sent: bool,
    /// Opened with the keyboard or the mouse at the desktop: the pointer says what Georg means. From the gamepad
    /// or in the sofa mode the pointer is stale or parked; the element with the focus counts there.
    by_pointer: bool,
    pub bar: crate::db_act_draw::BarImage,
}

impl Default for Db {
    fn default() -> Db {
        Db {
            open: false,
            generation: 0,
            target: String::new(),
            snapshot: None,
            pointed: None,
            stage: Stage::Input,
            chat: None,
            chat_asked: None,
            pending: None,
            ask_id: 0,
            question: String::new(),
            remote: false,
            context_sent: false,
            hidden: false,
            list_ready: false,
            want_shot: None,
            shot_cover: None,
            shot_asked: None,
            shot_covered: false,
            shot_pending: false,
            shot: None,
            answer: String::new(),
            shown: String::new(),
            ids: Vec::new(),
            tools: Vec::new(),
            asking: None,
            note: None,
            field: Default::default(),
            focus: None,
            level_open: false,
            since: Instant::now(),
            scroll: 0,
            tx: None,
            images: Default::default(),
            sofa_program: false,
            access: None,
            access_sent: false,
            by_pointer: false,
            bar: Default::default(),
        }
    }
}

impl Db {
    /// The buttons of the stage, in their order (the App adds the level's).
    pub fn buttons(&self) -> Vec<Button> {
        match &self.stage {
            Stage::Setup(_) => vec![Button::OpenKi, Button::Close],
            Stage::Step => vec![Button::StepYes, Button::StepNo],
            Stage::Acting => vec![Button::StepStop],
            Stage::Input | Stage::Done | Stage::Failed(_) => vec![Button::Keyboard],
            Stage::Waiting | Stage::Answering => Vec::new(),
        }
    }

    pub fn has_field(&self) -> bool {
        matches!(self.stage, Stage::Input | Stage::Done | Stage::Failed(_))
    }

    pub fn waits_for_chat(&self) -> bool {
        self.open && self.chat.is_none() && self.chat_asked.is_some()
    }

    /// An answer runs (the service works on this popup's question).
    pub fn running(&self) -> bool {
        matches!(self.stage, Stage::Waiting | Stage::Answering | Stage::Step | Stage::Acting)
    }
}

/// A journal line in one write: stderr is unbuffered, and `eprintln!` writes each piece of its format alone, so
/// the kernel log could get another line in the middle of it (seen in cl/41).
pub fn journal(line: &str) {
    let _ = std::io::Write::write_all(&mut std::io::stderr(), format!("{line}\n").as_bytes());
}

impl App {
    pub fn db_start(&mut self, handle: &LoopHandle<'static, App>) {
        let (tx, rx) = channel::channel::<(u64, Msg)>();
        if let Err(e) = handle.insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg((g, m)) = ev {
                app.db_msg(g, m);
            }
        }) {
            eprintln!("bitscomp: durchblick: no channel: {e} (the key does nothing)");
            return;
        }
        self.db.tx = Some(tx);
    }

    /// The popup's and the views' keys into settingsd (owner bitscomp), once per connection; the old Durchblick
    /// keys go once the engine's `ai.level` is there (its migration has read them, PLAN K8).
    pub fn db_register(&mut self) {
        if !self.settings_live {
            return;
        }
        let known = |k: &str| self.setting_keys.iter().any(|(have, _)| have == k);
        let Some(w) = self.settings_writer.as_ref() else { return };
        if !known(lv::NEVER_SEE) || !known(set::TASTE) {
            let sent = w.send(crate::desk::DESK_PID, crate::settings_write::Req::Defs { keys: set::defs() });
            eprintln!("bitscomp: durchblick: keys {} ({})", lv::NEVER_SEE, if sent { "registering in settingsd" } else { "writer gone" });
        }
        if known(lv::LEVEL) {
            for g in set::GONE {
                if self.setting_keys.iter().any(|(k, _)| k == g || k.starts_with(&format!("{g}."))) {
                    eprintln!("bitscomp: durchblick: the old keys below {g} go (ai.level is there)");
                    w.send(crate::desk::DESK_PID, crate::settings_write::Req::Unregister { prefix: g.into() });
                }
            }
        }
    }

    fn db_refused_open(&self) -> Option<&'static str> {
        if self.install.shown() || self.images.boot.shown() {
            Some("the installer's question or the boot scene is shown")
        } else if self.db.tx.is_none() {
            Some("no channel")
        } else {
            None
        }
    }

    pub fn db_toggle(&mut self, why: &str) {
        if self.db.open { self.db_close(why) } else { self.db_open(why) }
    }

    /// The element Georg points at in `w`, else the one with the keyboard.
    fn db_pointed(&self, w: &UiWindow) -> Option<String> {
        let s = self.theme.scale;
        let (x, y) = (self.pointer.x * s, self.pointer.y * s);
        // The panel itself is in the desktop's list (DB1): never what Georg means.
        fn ids(e: &uilist::Element, out: &mut std::collections::BTreeSet<String>) {
            out.insert(e.id.clone());
            e.children.iter().for_each(|c| ids(c, out));
        }
        let mut panel = std::collections::BTreeSet::new();
        let desktop = w.window == crate::ui_reg::DESKTOP;
        w.elements.iter().filter(|e| desktop && e.role == uilist::Role::Group && e.name == crate::db_ui::GROUP).for_each(|e| ids(e, &mut panel));
        let keep = |e: &uilist::Element| e.role != uilist::Role::Group && !panel.contains(&e.id);
        let under = if self.db.by_pointer { w.at_where(x, y, &keep) } else { None };
        under.or_else(|| w.walk().into_iter().find(|e| e.has(State::Focused) && keep(e))).map(|e| e.id.clone())
    }

    fn db_snapshot(&self) -> Option<UiWindow> {
        let t = self.db.target.clone();
        self.ui_windows(&Full::One(t.clone())).into_iter().find(|w| w.window == t)
    }

    /// The popup over the window in front, with a new chat.
    pub fn db_open(&mut self, why: &str) {
        if self.db.open {
            return;
        }
        if let Some(no) = self.db_refused_open() {
            eprintln!("bitscomp: durchblick: not opened ({why}): {no}");
            return;
        }
        self.db_open_for(None, why);
        if !self.assist_on() {
            self.db_set_stage(Stage::Setup(NO_HELPER.into()));
            return;
        }
        if let crate::assist::Gate::NotReady(why) = self.assist.gate() {
            self.db_set_stage(Stage::Setup(why));
            // The answer may be old (a provider set a moment before the service saw it): asked again.
            let lines = self.assist.refresh(crate::pacer::now());
            self.assist_sync(lines);
            return;
        }
        if !self.assist.up {
            let why = self.assist.silent_line();
            self.db_set_stage(Stage::Failed(why));
            return;
        }
        self.db_ask_chat();
    }

    fn db_ask_chat(&mut self) {
        let app = self.db.snapshot.as_ref().filter(|_| !self.db.hidden).map(|w| w.app.clone());
        self.db.chat_asked = Some(Instant::now());
        let line = crate::assist::new_line(crate::assist::ChatOrigin::Popup, app.as_deref());
        self.assist_out(vec![line], NewFor::Popup(self.db.generation));
    }

    /// The popup shows its set-up stage because the service said it was not ready: once a newer status says
    /// ready, it gets its chat after all.
    pub(crate) fn db_setup_ready(&mut self) {
        let waiting = self.db.open && matches!(self.db.stage, Stage::Setup(_)) && self.db.chat.is_none() && self.db.chat_asked.is_none();
        if waiting && self.assist.up && self.assist.gate() == crate::assist::Gate::Ready {
            self.db_set_stage(Stage::Input);
            self.db_ask_chat();
        }
    }

    /// The popup on chat `chat` that exists already (a question of it needs Georg and no view shows it).
    pub fn db_open_on(&mut self, chat: &str, why: &str) {
        if self.db.open && self.db.chat.as_deref() == Some(chat) {
            return;
        }
        if self.db.open {
            self.db_close("another chat asks");
        }
        if let Some(no) = self.db_refused_open() {
            eprintln!("bitscomp: durchblick: not opened ({why}): {no}");
            return;
        }
        self.db_open_for(Some(chat.to_string()), why);
        // Its question runs already: the popup shows what comes of it.
        self.db.context_sent = true;
        self.db.question = self.assist.chat_title(chat);
        self.db_set_stage(Stage::Waiting);
    }

    fn db_open_for(&mut self, chat: Option<String>, why: &str) {
        let tx = self.db.tx.clone();
        let generation = self.db.generation + 1;
        let by_pointer = why != PAD_OPEN && !self.sofa.active;
        self.db = Db { generation, tx, open: true, target: self.ui_focused_id(), by_pointer, chat, ..Db::default() };
        self.db.snapshot = self.db_snapshot();
        self.db.hidden = self.db.snapshot.as_ref().is_some_and(|w| lv::never_sees(&self.settings, &w.app));
        if !self.db.hidden {
            self.db.pointed = self.db.snapshot.as_ref().and_then(|w| self.db_pointed(w));
        }
        let (app_name, pid, rect) = match &self.db.snapshot {
            Some(w) => (w.app.clone(), w.pid as i32, Some(w.rect)),
            None => (String::new(), 0, None),
        };
        // The window's picture now, before the popup covers anything (PLAN K4: it goes with the first question).
        // A never-see program's window over it would be in the cut: then the list goes alone (PLAN K4).
        let covered = rect.is_some_and(|r| self.never_seen_in(r, &self.db.target));
        if covered {
            journal("bitscomp assist.picture view=popup none (a window the assistant never sees lies over it)");
        }
        if let Some([x, y, w, h]) = rect.filter(|r| r[2] > 0 && r[3] > 0 && !self.db.hidden && !covered && self.db.chat.is_none()) {
            self.db.want_shot = Some(Rect { x, y, w, h });
            self.db.shot_asked = Some(Instant::now());
            self.db.shot_pending = true;
        }
        if !self.db.hidden && let Some(tx) = self.db.tx.clone() {
            crate::db_worker::prep(app_name, pid, generation, tx);
        }
        let target = Full::One(self.db.target.clone());
        if self.db.hidden || !self.ui_fetch_then(&target, move || crate::ui_fetch::Then::Panel(generation)) {
            self.db.list_ready = true;
        }
        eprintln!("bitscomp: durchblick: opened for {} ({why}){}", self.db.target, if self.db.hidden { ": a program the assistant never sees" } else { "" });
        self.db_focus_default();
        self.needs_frame = true;
    }

    /// Esc, B, "Esc schließt": a running question of this popup is cancelled (its chat stays in the list).
    pub fn db_close(&mut self, why: &str) {
        if !self.db.open {
            return;
        }
        if let Some(chat) = self.db.chat.clone().filter(|_| self.db.running()) {
            if self.act.chat == chat {
                self.db_act_stop(crate::db_act_app::STOP_NOTE);
            }
            let line = Json::obj(vec![("op", Json::str("cancel")), ("chat", Json::Str(chat)), ("id", Json::Num(self.db.ask_id as f64))]).to_string();
            self.assist_out(vec![line], NewFor::Popup(self.db.generation));
        }
        let (generation, tx) = (self.db.generation, self.db.tx.clone());
        // The picture and the list are dropped with it.
        self.db = Db { generation, tx, ..Db::default() };
        eprintln!("bitscomp: durchblick: closed ({why})");
        self.needs_frame = true;
        self.chrome_dirty();
    }

    pub fn db_list_ready(&mut self, generation: u64) {
        if generation != self.db.generation || !self.db.open {
            return;
        }
        if let Some(w) = self.db_snapshot() {
            if self.db.pointed.as_ref().is_none_or(|p| w.find(p).is_none()) && !self.db.hidden {
                self.db.pointed = self.db_pointed(&w);
            }
            self.db.snapshot = Some(w);
        }
        self.db.list_ready = true;
        self.db_go();
    }

    pub fn db_shot_taken(&mut self) {
        self.db.shot_pending = false;
        self.needs_frame = true;
        self.db_go();
    }

    pub(crate) fn db_set_stage(&mut self, s: Stage) {
        self.db.stage = s;
        self.db.since = Instant::now();
        self.db.scroll = 0;
        self.db_focus_default();
        self.needs_frame = true;
    }

    fn db_focus_default(&mut self) {
        self.db.focus = match self.db.stage {
            Stage::Setup(_) => Some(Button::OpenKi),
            Stage::Step => Some(Button::StepYes),
            Stage::Acting => Some(Button::StepStop),
            _ => None,
        };
    }

    /// Enter in the input line: the typed question, or on the empty line "Erklär mir, was ich hier sehe".
    pub fn db_submit(&mut self, text: &str, origin: Origin) {
        if !self.db.open || !self.db.has_field() {
            return;
        }
        let typed = text.trim();
        let question: String = if typed.is_empty() { q::DEFAULT.to_string() } else { typed.chars().take(2000).collect() };
        self.db.field = Default::default();
        self.db.level_open = false;
        let remote = origin != Origin::Physical;
        match self.db.chat.clone() {
            Some(_) => self.db_ask(question, remote),
            None if self.db.chat_asked.is_some() => {
                // The service makes the chat a moment later: the question goes then (`db_chat_made`).
                self.db.pending = Some((question.clone(), remote));
                self.db.question = question;
                self.db_set_stage(Stage::Waiting);
            }
            None => self.db_set_stage(Stage::Failed(NO_ANSWER.into())),
        }
    }

    /// The service made the popup's chat.
    pub(crate) fn db_chat_made(&mut self, generation: u64, chat: &str) {
        if generation != self.db.generation || !self.db.open {
            // A popup that closed meanwhile leaves an empty chat; the service keeps it, the list shows it.
            eprintln!("bitscomp: durchblick: chat {chat} came after its popup closed");
            return;
        }
        self.db.chat = Some(chat.to_string());
        if let Some((question, remote)) = self.db.pending.take() {
            self.db_ask(question, remote);
        }
    }

    pub(crate) fn db_refused(&mut self, j: &Json) {
        let m = j.get("message").and_then(Json::as_str).filter(|m| !m.is_empty()).unwrap_or(NO_ANSWER).to_string();
        self.db.pending = None;
        self.db.chat_asked = None;
        self.db_set_stage(Stage::Failed(m));
    }

    /// A question into the popup's chat; the first one waits for the window (list and picture) and carries it.
    fn db_ask(&mut self, question: String, remote: bool) {
        self.db.question = question;
        self.db.remote = remote;
        if !self.db.context_sent && (!self.db.list_ready || self.db.shot_pending) {
            self.db.pending = Some((self.db.question.clone(), remote));
            return self.db_set_stage(Stage::Waiting);
        }
        let Some(chat) = self.db.chat.clone() else { return };
        let mut context = None;
        let mut words = "-".to_string();
        let mut bytes = self.db.question.len();
        let access = self.db.access.clone().filter(|_| q::about_program(&self.db.question) && !self.db.access_sent);
        if let Some(w) = self.db.snapshot.clone().filter(|_| !self.db.hidden && (!self.db.context_sent || access.is_some())) {
            let first = !self.db.context_sent;
            let image = self.db.shot.as_ref().filter(|_| first).map(|s| crate::db_shot::base64(&s.png));
            let mut question = q::build(&q::Inputs { window: &w, pointed: self.db.pointed.as_deref(), access: access.as_deref(), picture: image.is_some() });
            if !first {
                // A follow-up: only the access log goes (the model reads the window again with `sehen`).
                question.list.clear();
                question.kinds.retain(|k| *k == Kind::Zugriffe);
            }
            bytes += question.list.len() + question.extras.len() + image.as_ref().map_or(0, String::len);
            words = question.words();
            self.db.access_sent |= question.kinds.contains(&Kind::Zugriffe);
            context = Some(question.context(&w, image));
            self.db.context_sent = true;
        }
        self.db.context_sent = true;
        let (provider, place) = self.assist.status.as_ref().map_or(("?".to_string(), "?".to_string()), |s| (s.provider.clone().unwrap_or_else(|| "?".into()), s.place.clone().unwrap_or_else(|| "?".into())));
        // B6: what went where, never what it said.
        journal(&format!("bitscomp assist.sent view=popup chat={chat} provider={provider} place={place} kinds={words} bytes={bytes} by={}", if remote { "remote" } else { "hand" }));
        self.db.ask_id += 1;
        let id = self.db.ask_id;
        let line = crate::assist::ask_line(&chat, id, &self.db.question, remote, context);
        self.assist_out(vec![line], NewFor::Popup(self.db.generation));
        (self.db.answer, self.db.shown, self.db.ids, self.db.tools, self.db.asking, self.db.note) = (String::new(), String::new(), Vec::new(), Vec::new(), None, None);
        self.db_set_stage(Stage::Waiting);
    }

    /// The next step once everything that goes into the first question is there.
    fn db_go(&mut self) {
        if !self.db.open || !self.db.list_ready || self.db.shot_pending || self.db.chat.is_none() {
            return;
        }
        if let Some((question, remote)) = self.db.pending.take() {
            self.db_ask(question, remote);
        }
    }

    /// An event of the popup's chat (`assist_route`).
    pub(crate) fn db_event(&mut self, ev: &str, j: &Json) {
        let text = |k: &str| j.get(k).and_then(Json::as_str).unwrap_or("").to_string();
        match ev {
            "start" | "notice" => {}
            "delta" => {
                if self.db.answer.len() < 64 * 1024 {
                    self.db.answer.push_str(&text("text"));
                }
                let (shown, ids) = q::ids_in(&self.db.answer);
                (self.db.shown, self.db.ids) = (shown, ids);
                if self.db.stage == Stage::Waiting {
                    self.db.stage = Stage::Answering;
                }
            }
            "tool" => {
                let name = text("name");
                let line = Some(text("line")).filter(|l| !l.is_empty()).unwrap_or_else(|| text("text"));
                let state = match text("state").as_str() {
                    "asks" => ToolState::Asks,
                    "runs" => ToolState::Runs,
                    "refused" => ToolState::Refused,
                    _ => ToolState::Done,
                };
                match self.db.tools.iter().rposition(|(n, _, s)| *n == name && matches!(s, ToolState::Asks | ToolState::Runs)) {
                    Some(i) => {
                        if !line.is_empty() {
                            self.db.tools[i].1 = line.clone();
                        }
                        self.db.tools[i].2 = state;
                    }
                    None if self.db.tools.len() < 200 => self.db.tools.push((name.clone(), line.clone(), state)),
                    None => {}
                }
                if state == ToolState::Asks {
                    let ask_id = j.get("ask_id").and_then(Json::as_u64).unwrap_or(0);
                    self.db.asking = Some(Asking { ask_id, name, text: line });
                    return self.db_set_stage(Stage::Step);
                }
                if self.db.stage == Stage::Step && self.db.asking.is_some() {
                    self.db.asking = None;
                    self.db_set_stage(Stage::Waiting);
                }
                if state == ToolState::Done && name == crate::assist::DELETE_TOOL {
                    // F4 (board 5): where it went, and the way to look (the sidebar's "Papierkorb zeigen").
                    self.db.note = Some(TRASH_NOTE.into());
                }
            }
            "done" => {
                eprintln!("bitscomp: durchblick: answered ({} characters, {} elements framed)", self.db.shown.chars().count(), self.db_frames().len());
                self.db.asking = None;
                self.db_set_stage(Stage::Done);
            }
            "error" => {
                self.db.asking = None;
                let m = Some(text("message")).filter(|m| !m.is_empty()).unwrap_or_else(|| NO_ANSWER.into());
                if m == "abgebrochen" {
                    self.db.note = Some("Abgebrochen.".into());
                    self.db_set_stage(Stage::Done);
                } else {
                    self.db_set_stage(Stage::Failed(m));
                }
            }
            _ => {}
        }
        self.needs_frame = true;
    }

    /// The connection to the assistant went: a running question is over.
    pub(crate) fn db_link_down(&mut self) {
        if self.db.open && (self.db.running() || self.db.waits_for_chat()) {
            self.db.pending = None;
            self.db.chat_asked = None;
            self.db_set_stage(Stage::Failed(NO_ANSWER.into()));
        }
    }

    /// Deadlines: a chat or a first answer that does not come.
    pub fn db_tick(&mut self) {
        if self.db.waits_for_chat() && self.db.chat_asked.is_some_and(|t| t.elapsed() >= CHAT_WAIT) {
            eprintln!("bitscomp: durchblick: no chat within {} s", CHAT_WAIT.as_secs());
            self.db.chat_asked = None;
            self.db.pending = None;
            self.db_set_stage(Stage::Failed(NO_ANSWER.into()));
        }
    }

    pub(crate) fn db_msg(&mut self, generation: u64, m: Msg) {
        if generation == ACT_GEN {
            match m {
                Msg::ActGo(n) => self.db_act_go(n),
                Msg::ActRead => {
                    let seq = self.act.seq;
                    self.db_act_listed(seq)
                }
                Msg::DeskShot(call, shot) => self.desk_shot_done(&call, shot),
                Msg::Trash(text) => self.assist_trash_shown(&text),
                _ => {}
            }
            self.needs_frame = true;
            return;
        }
        if generation != self.db.generation || !self.db.open {
            return;
        }
        match m {
            Msg::Access(a) => self.db.access = a,
            Msg::Shot(shot) => {
                self.db.shot = shot;
                self.db_shot_taken();
            }
            _ => {}
        }
        self.needs_frame = true;
        self.db_go();
    }

    /// Georg's own hand, and not a click meant for what was there before.
    fn db_consent(&mut self) -> bool {
        if self.input_origin != Origin::Physical {
            self.db.note = Some(REAL_INPUT.into());
            eprintln!("bitscomp: durchblick: a yes from injected input refused");
            return false;
        }
        self.db.since.elapsed() >= ARM
    }

    pub fn db_press(&mut self, b: Button) {
        if !self.db_buttons().contains(&b) || (b.consents() && !self.db_consent()) {
            self.needs_frame = true;
            return;
        }
        match b {
            Button::Close => self.db_close("Schließen"),
            Button::OpenKi => {
                self.db_close("Seite KI");
                self.open_address("einstellungen/ki", crate::address::Where::Tab, None);
            }
            Button::Keyboard => self.db_sofa_keyboard(),
            Button::StepYes => self.db_step_answer(true),
            Button::StepNo | Button::StepStop => self.db_step_answer(false),
            Button::Level => self.db.level_open = !self.db.level_open,
            Button::LevelPick(l) => {
                let origin = self.input_origin;
                let armed = self.db.since.elapsed() >= ARM;
                self.assist_level_pick(l, origin, armed);
                self.db.level_open = false;
                // The choices close: the keys are the input line's again.
                self.db.focus = None;
            }
        }
        self.needs_frame = true;
    }

    /// Ja or Nein to the popup's question: the engine's (its `answer`) or a step's (the desktop's gate).
    fn db_step_answer(&mut self, yes: bool) {
        if let Some(a) = self.db.asking.take() {
            if let Some(chat) = self.db.chat.clone() {
                eprintln!("bitscomp: durchblick: question {} of chat {chat} answered {}", a.ask_id, if yes { "yes" } else { "no" });
            }
            let line = Json::obj(vec![("op", Json::str("answer")), ("ask_id", Json::Num(a.ask_id as f64)), ("yes", Json::Bool(yes))]).to_string();
            self.assist_out(vec![line], NewFor::Popup(self.db.generation));
            if let Some(t) = self.db.tools.iter_mut().rev().find(|t| t.0 == a.name && t.2 == ToolState::Asks) {
                t.2 = if yes { ToolState::Runs } else { ToolState::Refused };
            }
            return self.db_set_stage(Stage::Waiting);
        }
        if yes {
            self.db_act_confirm();
        } else {
            self.db_act_stop(crate::db_act_app::STOP_NOTE);
        }
    }

    /// The elements the answer named that are on the screen now (physical px).
    pub fn db_frames(&self) -> Vec<[i32; 4]> {
        // DB5b: the step's element first.
        if let Some(r) = self.db_step_frame() {
            return vec![r];
        }
        let Some(w) = self.db.snapshot.as_ref().filter(|_| self.db.open) else { return Vec::new() };
        self.db.ids.iter().filter_map(|id| w.find(id)).filter(|e| e.visible && e.rect[2] > 0 && e.rect[3] > 0).map(|e| e.rect).collect()
    }

    /// The level now and whether it was the migration's (shown under the pill once).
    pub fn assist_level(&self) -> Level {
        Level::of(&self.settings)
    }

    /// The on-screen keyboard of the sofa mode for a question.
    fn db_sofa_keyboard(&mut self) {
        if !self.sofa.active || !self.db.has_field() {
            self.db.note = Some("Tippe die Frage auf der Tastatur ein.".into());
            return;
        }
        self.db.sofa_program = self.sofa.front == crate::sofa::Front::Program;
        self.sofa.front = crate::sofa::Front::Shell;
        let target = crate::sofa_search::Target::Setting { key: SOFA_FIELD.into(), title: "Frage an den Assistenten".into(), secret: false };
        self.sofa_search_open(target, "");
        self.sofa_changed();
    }

    /// Fertig on the sofa keyboard: true when it was the question's (then it goes).
    pub fn db_sofa_enter(&mut self) -> bool {
        let crate::sofa_search::Target::Setting { key, .. } = &self.sofa.search.target else { return false };
        if key != SOFA_FIELD {
            return false;
        }
        let text = self.sofa.search.field.text.clone();
        let origin = self.input_origin;
        self.sofa_pad(crate::pad::Press::Btn(crate::pad::Btn::B));
        if std::mem::take(&mut self.db.sofa_program) {
            self.sofa.front = crate::sofa::Front::Program;
            self.sofa_changed();
        }
        self.db_submit(&text, origin);
        true
    }

    /// Whether the sofa keyboard types a question now (the panel steps aside).
    pub fn db_sofa_typing(&self) -> bool {
        self.sofa.nav.screen == crate::sofa_focus::Screen::Suche && matches!(&self.sofa.search.target, crate::sofa_search::Target::Setting { key, .. } if key == SOFA_FIELD)
    }
}

#[cfg(test)]
#[path = "db_app_tests.rs"]
mod tests;
