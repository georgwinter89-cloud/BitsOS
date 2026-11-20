//! DB5b, EINS-B (PLAN K2/K3): the desktop tool `handeln` of every chat. The model proposes one step at a time
//! (`desk{name:"handeln"}` from `assist serve`); BitsOS runs the step, not the model: it shows it in the view that
//! shows the chat (which element, what, in plain German, the element framed) and waits for Georg's yes, or runs
//! it after a short pause in the Automodus with the bar on top; Esc / B stops at once. After each step it reads
//! the window again and gives the model what is there now. The level decides (`ai.level`): Nur antworten refuses,
//! Fragt vorher asks every step, Automodus runs the normal ones; a step that cannot be undone is asked at every
//! level (F2), and so is every step of a question that did not come from Georg's hand. One task per question of a
//! chat; one at a time on the desktop (there is one keyboard). Every step is a journal line (never typed text).

use std::time::Instant;

use serde_json::Value;
use uilist::{Element, Window};

use crate::assist_level::{self as lv, Level};
use crate::db_act::{self as act, Next, Risk, Step, Task};
use crate::db_question as q;
use crate::db_worker::Msg;
use crate::input::Origin;
use crate::state::App;
use crate::ui_reg::Full;

pub const ACT_TOOL: &str = "handeln";
pub const STOPPED: &str = "Georg hat angehalten. Tu nichts mehr; sag ihm kurz, was erledigt ist und was nicht.";
pub const STOP_NOTE: &str = "Angehalten. Der Assistent tut nichts mehr.";
pub const ANSWER_ONLY: &str = "Im Modus „Nur antworten“ handelt der Assistent nicht. Es wurde nichts getan.";
pub const OTHER_TASK: &str = "Ein anderer Chat bedient gerade den Bildschirm. Es wurde nichts getan.";
/// Board 2: under a step that the Automodus would have run.
pub const AUTO_WOULD: &str = "Im Automodus hätte er nicht gefragt.";
const READ_AGAIN: std::time::Duration = std::time::Duration::from_millis(400);
/// D (EINGABE2): the window still loads when the step's answer is due.
pub const LOADING_STEP: &str = "Die Seite lädt noch; sehen zeigt sie gleich.";
pub const LOADING_LIST: &str = "Achtung: Die Seite lädt noch; diese Liste kann noch die vorige Seite zeigen.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Waits for Georg's yes.
    Asking,
    /// The Automodus's pause before it runs.
    Pausing,
    /// Its input is on the way (a foreign program answers through its provider).
    Running,
    /// Done; the window is read again before the model hears of it.
    Reading,
}

#[derive(Clone, Debug)]
pub struct Pending {
    /// The service's call id (`desk_result`).
    pub tool: String,
    pub step: Step,
    pub element: Element,
    pub risk: Risk,
    pub said: String,
    pub phase: Phase,
    /// The window's fields when the step was proposed (B: where a text that went astray landed).
    pub fields: Vec<crate::desk_write_app::FieldBefore>,
    /// The window in front when the step was proposed: another one in front before it runs is not the plan.
    pub front: String,
    /// When the step was shown: Ja counts only `ARM` later.
    pub since: Instant,
}

#[derive(Default)]
pub struct Acting {
    pub task: Option<Task>,
    pub pending: Option<Pending>,
    /// The chat the task belongs to.
    pub chat: String,
    /// Counts tasks; a late answer of an older one goes nowhere (`To::Db`).
    pub seq: u64,
    /// A step's own keys and clicks are on their way: the views let them through to the window.
    pub stepping: bool,
    /// What the task's view says now (a refusal, a stop, "n von m Schritten").
    pub note: Option<String>,
}

/// Where a step's question is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Popup,
    Sidebar,
}

impl App {
    /// Whether a task runs now (not stopped).
    pub fn db_acting(&self) -> bool {
        self.act.task.as_ref().is_some_and(|t| t.stopped.is_none())
    }

    /// Steps run unasked now: the Automodus, and the question came from Georg's hand. The bar shows.
    pub fn db_auto(&self) -> bool {
        self.db_acting() && self.act.task.as_ref().is_some_and(|t| t.by_georg) && Level::of(&self.settings) == Level::Auto
    }

    /// The view that shows the acting chat (the popup when both do).
    pub fn act_view(&self) -> Option<View> {
        let chat = self.act.chat.as_str();
        if self.db.open && self.db.chat.as_deref() == Some(chat) {
            Some(View::Popup)
        } else if self.assist.chat.as_deref() == Some(chat) && self.assist_place().is_some() {
            Some(View::Sidebar)
        } else {
            None
        }
    }

    fn act_answer(&mut self, id: &str, r: Result<String, String>) {
        self.desk_result(id, r, None);
    }

    fn act_say(&mut self, text: Option<String>) {
        self.act.note = text.clone();
        match self.act_view() {
            Some(View::Popup) => self.db.note = text,
            Some(View::Sidebar) => self.assist.note = text.map(|t| (t, false)),
            None => {}
        }
        self.needs_frame = true;
        self.chrome_dirty();
    }

    /// The step is shown as a question (in the view of its chat; none shows it: the popup opens on it).
    fn act_show_question(&mut self) {
        if self.act_view().is_none() {
            let chat = self.act.chat.clone();
            self.db_open_on(&chat, "a step asks");
        }
        let text = self.db_step_text();
        match self.act_view() {
            Some(View::Sidebar) => {
                let now = crate::pacer::now();
                let remote = self.act.task.as_ref().is_some_and(|t| !t.by_georg);
                self.assist.card = Some(crate::assist::Card {
                    ask_id: self.act.seq,
                    of: crate::assist::CardFor::Step,
                    title: self.db_step_title(),
                    text,
                    yes_picked: true,
                    note: None,
                    remote,
                    since: now,
                    armed: false,
                });
                self.assist.phase = crate::assist::Phase::Waiting;
                self.assist_sync(Vec::new());
            }
            _ => self.db_set_stage(crate::db_app::Stage::Step),
        }
    }

    /// The view shows what runs (or that it waits again for the model).
    fn act_show_running(&mut self, running: bool) {
        match self.act_view() {
            Some(View::Sidebar) => {
                if self.assist.card.as_ref().is_some_and(|c| c.of == crate::assist::CardFor::Step) {
                    self.assist.card = None;
                }
                self.assist.phase = if running { crate::assist::Phase::Working } else { crate::assist::Phase::Thinking };
                self.assist.tool = running.then(|| self.db_step_text());
                self.assist_sync(Vec::new());
            }
            Some(View::Popup) => {
                use crate::db_app::Stage;
                if running {
                    self.db_set_stage(Stage::Acting);
                } else if matches!(self.db.stage, Stage::Step | Stage::Acting) {
                    self.db_set_stage(Stage::Waiting);
                }
            }
            None => {}
        }
    }

    fn db_act_journal(&self, p: &Step, e: Option<&Element>, app: &str, result: &str) {
        let Some(t) = self.act.task.as_ref() else { return };
        // A secret element that is not a field (a key of the on-screen keyboard) says nothing by its name.
        let name = e.filter(|e| !(e.has(uilist::State::Secret) && e.role != uilist::Role::Field)).map_or(String::new(), |e| e.name.chars().take(60).collect::<String>().replace(['\n', '"'], " "));
        let role = e.map_or("-".to_string(), |e| format!("{:?}", e.role).to_lowercase());
        crate::db_app::journal(&format!(
            "bitscomp assist.step chat={} task={} n={} app={app} element={} role={role} action={} chars={} result={result} name=\"{name}\"",
            self.act.chat,
            t.n,
            t.steps,
            p.element,
            crate::ui_act::words(p.action),
            p.text.as_ref().map_or(0, |t| t.chars().count())
        ));
    }

    /// Whether `e` is the desktop's own that only Georg answers: a question's answering element (any BitsOS
    /// question), the popup, the sidebar's assistant or the bar that says the assistant acts.
    fn db_act_own(&self, w: &Window, e: Option<&Element>) -> bool {
        let Some(e) = e else { return false };
        if e.answers {
            return true;
        }
        if w.window != crate::ui_reg::DESKTOP {
            return false;
        }
        let own = [crate::db_ui::GROUP, crate::db_act_draw::BAR, crate::assist_ctl::GROUP];
        w.elements.iter().filter(|g| g.role == uilist::Role::Group && own.contains(&g.name.as_str())).any(|g| g.id == e.id || walk_has(g, &e.id))
    }

    /// The model of chat `chat` proposed a step (`handeln`, call `id`).
    pub(crate) fn db_act_call(&mut self, chat: &str, id: &str, args: &Value) {
        // A task of another chat is busy with the screen: one keyboard, one task.
        if self.act.chat != chat && self.db_acting() {
            return self.act_answer(id, Err(OTHER_TASK.into()));
        }
        if self.act.chat == chat && self.act.pending.is_some() {
            return self.act_answer(id, Err("Ein Schritt nach dem anderen: der vorige ist noch nicht fertig.".into()));
        }
        let step = match act::parse(args) {
            Ok(s) => s,
            Err(e) => return self.act_answer(id, Err(e)),
        };
        let win = uilist::window_of(&step.element).unwrap_or("").to_string();
        let w = self.ui_windows(&Full::One(win.clone())).into_iter().find(|w| w.window == win);
        // One task per question: a stopped one stays stopped until the question ends (`db_act_end`).
        if self.act.task.is_none() || self.act.chat != chat {
            self.db_act_begin(chat, w.as_ref());
        }
        let e = w.as_ref().and_then(|w| w.find(&step.element).cloned());
        let own = w.as_ref().is_some_and(|w| self.db_act_own(w, e.as_ref()));
        let hidden = w.as_ref().is_some_and(|w| lv::never_sees(&self.settings, &w.app));
        let level = Level::of(&self.settings);
        let Some(task) = self.act.task.as_mut() else { return };
        let (next, risk) = task.decide(Instant::now(), w.as_ref(), e.as_ref(), own, hidden, &step, level);
        let word = match &next {
            Next::Run => "auto",
            Next::Ask => "asked",
            Next::Refuse(_) => "refused",
            Next::Stop(_) => "stopped",
        };
        let app = w.as_ref().map(|w| w.app.clone()).unwrap_or_default();
        self.db_act_journal(&step, e.as_ref(), &app, word);
        match next {
            Next::Refuse(why) => {
                self.act_say(Some(format!("Nicht getan: {why}")));
                self.act_answer(id, Err(why));
            }
            Next::Stop(why) => {
                self.db_act_stop(&why);
                self.act_answer(id, Err(format!("{why} Die Aufgabe ist beendet; tu nichts mehr.")));
            }
            Next::Ask | Next::Run => {
                let (Some(e), Some(w)) = (e, w) else { return };
                let e = self.db_act_reveal(&w, e);
                let said = act::describe(&e, &step);
                let phase = if next == Next::Ask { Phase::Asking } else { Phase::Pausing };
                let fields = w.walk().into_iter().filter(|x| matches!(x.role, uilist::Role::Field | uilist::Role::Choice)).map(|x| (x.id.clone(), x.name.clone(), x.value.clone())).collect();
                let front = self.ui_focused_id();
                self.act.pending = Some(Pending { tool: id.into(), step, element: e, risk, said, phase, fields, front, since: Instant::now() });
                self.act_say(None);
                if phase == Phase::Asking {
                    self.act_show_question();
                } else {
                    self.act_show_running(true);
                    self.db_act_later();
                }
            }
        }
    }

    /// The first step of a question in chat `chat`: a new task, Georg's hand or not as the question came.
    fn db_act_begin(&mut self, chat: &str, w: Option<&Window>) {
        let by_georg = self.assist_route.hand.get(chat).copied().unwrap_or(false);
        self.act.seq += 1;
        let n = self.act.seq;
        let (window, app) = w.map_or((String::new(), String::new()), |w| (w.window.clone(), w.app.clone()));
        let task = Task { n, started: Instant::now(), steps: 0, done: 0, by_georg, app: app.clone(), stopped: None };
        self.act = Acting { task: Some(task), pending: None, chat: chat.to_string(), seq: n, stepping: false, note: None };
        crate::db_app::journal(&format!("bitscomp assist.task chat={chat} n={n} app={app} window={window} by={}", if by_georg { "georg" } else { "remote" }));
    }

    /// A page's row below its head is scrolled into view before the step is shown, so Georg sees what is framed
    /// (the wheel over the page, as `ui_act` would do it anyway when the step runs).
    fn db_act_reveal(&mut self, w: &Window, e: Element) -> Element {
        if e.visible || w.source != uilist::Source::Desktop || [crate::ui_reg::DESKTOP, crate::ui_reg::SOFA].contains(&w.window.as_str()) {
            return e;
        }
        let origin = std::mem::replace(&mut self.ctl_origin, Origin::Injected);
        self.act.stepping = true;
        let shown = self.ui_reveal(w, &e.id);
        self.act.stepping = false;
        self.ctl_origin = origin;
        shown.unwrap_or(e)
    }

    /// The Automodus's pause, then `ActGo` for this step (a thread: the main loop never sleeps).
    fn db_act_later(&self) {
        let (Some(tx), Some(t)) = (self.db.tx.clone(), self.act.task.as_ref()) else { return };
        let n = t.steps as u64;
        let _ = std::thread::Builder::new().name("bitscomp-db-pause".into()).spawn(move || {
            std::thread::sleep(act::AUTO_PAUSE);
            let _ = tx.send((crate::db_app::ACT_GEN, Msg::ActGo(n)));
        });
    }

    /// The pause of step `n` is over: it runs unless Georg stopped or something else came first.
    pub(crate) fn db_act_go(&mut self, n: u64) {
        let now = self.act.task.as_ref().map(|t| t.steps as u64);
        if now != Some(n) || !self.act.pending.as_ref().is_some_and(|p| p.phase == Phase::Pausing) || !self.db_acting() {
            return;
        }
        if self.db_act_answer_only() {
            return;
        }
        // The Automodus was switched off during the pause: the step is asked after all.
        if !self.db_auto() {
            if let Some(p) = self.act.pending.as_mut() {
                (p.phase, p.since) = (Phase::Asking, Instant::now());
            }
            return self.act_show_question();
        }
        self.db_act_run();
    }

    /// Georg's yes (his own hand, checked by the view).
    pub(crate) fn db_act_confirm(&mut self) {
        if self.act.pending.as_ref().is_some_and(|p| p.phase == Phase::Asking) {
            if self.db_act_answer_only() {
                return;
            }
            self.act_show_running(true);
            self.db_act_run();
        }
    }

    /// The step's input, as a real input would come, with the control socket's origin (a question that counts
    /// only Georg's own hand still refuses it) and the rules of an agent (it never answers a question).
    fn db_act_run(&mut self) {
        if self.db_act_answer_only() {
            return;
        }
        let Some(p) = self.act.pending.clone() else { return };
        let front = self.ui_focused_id();
        let step_window = uilist::window_of(&p.step.element).unwrap_or("").to_string();
        if front != p.front && front != step_window {
            let why = "Ein anderes Fenster ist nach vorn gekommen: der Bildschirm sieht anders aus als geplant.";
            self.db_act_stop(why);
            return self.act_answer(&p.tool, Err(why.into()));
        }
        if let Some(q) = self.act.pending.as_mut() {
            q.phase = Phase::Running;
        }
        let g = self.act.seq;
        let origin = std::mem::replace(&mut self.ctl_origin, Origin::Injected);
        self.act.stepping = true;
        self.ui_fetch.pending = Some(crate::ui_fetch::To::Db(g));
        let r = self.ui_act_by(&p.step.element, crate::ui_act::words(p.step.action), p.step.text.as_deref(), crate::ui_act::By::Mcp);
        let _ = self.ui_fetch.pending.take();
        self.act.stepping = false;
        self.ctl_origin = origin;
        match r {
            Ok(t) if t == crate::ui_fetch::LATER => {}
            other => self.db_act_done(g, other.map(|_| ())),
        }
    }

    /// The step's input arrived (or did not).
    pub(crate) fn db_act_done(&mut self, generation: u64, r: Result<(), String>) {
        let running = self.act.pending.as_ref().is_some_and(|p| p.phase == Phase::Running);
        if generation != self.act.seq || !running {
            // Georg stopped while a foreign program still carried the step out: its answer goes nowhere.
            eprintln!("bitscomp: assist: a step's late answer dropped ({})", if r.is_ok() { "done" } else { "failed" });
            return;
        }
        let Some(p) = self.act.pending.clone() else { return };
        let app = self.act.task.as_ref().map(|t| t.app.clone()).unwrap_or_default();
        self.db_act_journal(&p.step, Some(&p.element), &app, if r.is_ok() { "ok" } else { "failed" });
        if let Err(e) = r {
            self.act.pending = None;
            self.act_say(Some(format!("Das ging nicht: {e}")));
            self.act_show_running(false);
            return self.act_answer(&p.tool, Err(format!("Das ging nicht: {e}")));
        }
        if let Some(t) = self.act.task.as_mut() {
            t.done += 1;
        }
        if let Some(q) = self.act.pending.as_mut() {
            q.phase = Phase::Reading;
        }
        let target = Full::One(uilist::window_of(&p.step.element).unwrap_or("").to_string());
        // D (EINGABE2): read once the program has settled (a page that loads, a list that still changes).
        let seq = self.act.seq;
        if !self.ui_fetch_settled(&target, move || crate::ui_fetch::Then::ActRead(seq)) {
            self.db_act_read_soon();
        }
    }

    /// EINGABE: the desktop wrote nothing (Georg's own input, the keyboard elsewhere, no way in, too slow): the
    /// screen is not what the plan expected, so the task ends.
    pub(crate) fn db_act_write_stopped(&mut self, generation: u64, why: &str) {
        let running = self.act.pending.as_ref().is_some_and(|p| p.phase == Phase::Running);
        if generation != self.act.seq || !running {
            eprintln!("bitscomp: assist: a step's late answer dropped (not written)");
            return;
        }
        let Some(p) = self.act.pending.take() else { return };
        let app = self.act.task.as_ref().map(|t| t.app.clone()).unwrap_or_default();
        self.db_act_journal(&p.step, Some(&p.element), &app, "stopped");
        self.db_act_stop(why);
        self.act_answer(&p.tool, Err(format!("{why} Die Aufgabe ist beendet; tu nichts mehr.")));
    }

    /// bitscomp's own window: read it a moment later, when a changed setting is back from settingsd.
    fn db_act_read_soon(&mut self) {
        let Some(tx) = self.db.tx.clone() else { return self.db_act_read() };
        let _ = std::thread::Builder::new().name("bitscomp-db-read-again".into()).spawn(move || {
            std::thread::sleep(READ_AGAIN);
            let _ = tx.send((crate::db_app::ACT_GEN, Msg::ActRead));
        });
    }

    /// The window was read again after a step.
    pub(crate) fn db_act_listed(&mut self, seq: u64) {
        if seq == self.act.seq && self.act.pending.as_ref().is_some_and(|p| p.phase == Phase::Reading) {
            self.db_act_read();
        }
    }

    /// What the window shows now goes to the model with the step's result; a window gone ends the task.
    fn db_act_read(&mut self) {
        let Some(p) = self.act.pending.take() else { return };
        let win = uilist::window_of(&p.step.element).unwrap_or("").to_string();
        let front = self.ui_focused_id();
        let now = self.ui_windows(&Full::One(win.clone())).into_iter().find(|w| w.window == win);
        let now = now.or_else(|| self.ui_windows(&Full::One(front.clone())).into_iter().find(|w| w.window == front));
        let Some(now) = now else {
            let why = format!("Erledigt: {}. Danach ist das Fenster weg: der Bildschirm sieht anders aus als geplant.", p.said);
            self.db_act_stop(&why);
            return self.act_answer(&p.tool, Err(format!("{why} Die Aufgabe ist beendet; tu nichts mehr.")));
        };
        if now.window == self.db.target {
            self.db.snapshot = Some(now.clone());
        }
        let seen = !lv::never_sees(&self.settings, &now.app);
        let mut text = format!("Erledigt: {}.", p.said);
        if p.step.action == uilist::Action::Type && now.source != uilist::Source::Atspi {
            text.push_str(&field_now(now.find(&p.step.element), seen));
        } else if p.step.action == uilist::Action::Type {
            // B (EINGABE2): a foreign program's field: "Erledigt" only when it now holds exactly the text.
            let target = now.find(&p.step.element).filter(|e| !e.has(uilist::State::Secret));
            let value = |id: &str| now.find(id).and_then(|e| e.value.clone());
            let others: Vec<(String, Option<String>, Option<String>)> = p.fields.iter().filter(|(id, ..)| *id != p.step.element).map(|(id, n, v)| (n.clone(), v.clone(), value(id))).collect();
            let after = self.writing.after.take();
            match crate::desk_write::landed(target.and_then(|e| e.value.as_deref()), p.step.text.as_deref().unwrap_or(""), &others, after.as_ref()) {
                crate::desk_write::Landed::Arrived => text.push_str(&field_now(target, seen)),
                crate::desk_write::Landed::Unchecked => text = format!("Geschrieben, aber nicht geprüft: {}. Was jetzt im Feld steht, lässt sich nicht lesen.", p.said),
                crate::desk_write::Landed::Missing(elsewhere) => {
                    let why = crate::desk_write::missing(&p.element.name, elsewhere.as_deref());
                    let app = self.act.task.as_ref().map(|t| t.app.clone()).unwrap_or_default();
                    self.db_act_journal(&p.step, Some(&p.element), &app, "missing");
                    self.db_act_stop(&why);
                    return self.act_answer(&p.tool, Err(format!("{why} Die Aufgabe ist beendet; tu nichts mehr.")));
                }
            }
        }
        if self.ui_fetch.loading(&now.window) {
            text.push(' ');
            text.push_str(LOADING_STEP);
        } else if seen {
            let list = q::build(&q::Inputs { window: &now, pointed: None, access: None, picture: false }).list;
            text.push_str("\nSo sieht das Fenster jetzt aus:\n");
            text.push_str(&list);
        } else {
            text.push(' ');
            text.push_str(lv::NOT_SEEN);
            text.push('.');
        }
        self.act_show_running(false);
        self.act_answer(&p.tool, Ok(text));
    }

    /// The level went to Nur antworten while a step waited (asked or in its pause): it is refused and the task
    /// ends, so less freedom holds at once (true: so it went).
    pub(crate) fn db_act_answer_only(&mut self) -> bool {
        let waits = self.act.pending.as_ref().is_some_and(|p| matches!(p.phase, Phase::Asking | Phase::Pausing));
        if !waits || Level::of(&self.settings) != Level::Answer {
            return false;
        }
        let Some(p) = self.act.pending.take() else { return false };
        let app = self.act.task.as_ref().map(|t| t.app.clone()).unwrap_or_default();
        self.db_act_journal(&p.step, Some(&p.element), &app, "answer-only");
        self.db_act_stop(ANSWER_ONLY);
        self.act_answer(&p.tool, Err(ANSWER_ONLY.into()));
        true
    }

    /// Esc, B, "Nein", "Anhalten", or a rule: nothing more runs in this task; a step waiting or on its way is
    /// answered now.
    pub fn db_act_stop(&mut self, why: &str) {
        let Some(t) = self.act.task.as_mut() else { return };
        if t.stopped.is_some() {
            return;
        }
        t.stopped = Some(why.to_string());
        crate::db_app::journal(&format!("bitscomp assist.task chat={} n={} stopped steps={} done={}", self.act.chat, t.n, t.steps, t.done));
        // A step being read again was done (so is a text the desktop has already put in): the model hears that, and
        // that nothing more comes, so its turn ends.
        let written = self.desk_write_in_db();
        if let Some(p) = self.act.pending.take() {
            let r = if p.phase == Phase::Reading || written { Ok(format!("Erledigt: {}. Danach hat Georg angehalten. {STOPPED}", p.said)) } else { Err(STOPPED.into()) };
            self.act_answer(&p.tool, r);
        }
        // EINGABE: a text the desktop was about to write is not written (a private paste not yet taken is taken back).
        self.desk_write_stop_db(crate::desk_write::HAND_STOP);
        self.act_say(Some(if why == STOP_NOTE { why.to_string() } else { format!("{STOP_NOTE} {why}") }));
        self.act_show_running(false);
        self.needs_frame = true;
    }

    /// The question of chat `chat` ended: its task is over.
    pub(crate) fn db_act_end(&mut self, chat: &str) {
        if self.act.chat != chat {
            return;
        }
        if let Some(t) = self.act.task.take() {
            if t.stopped.is_none() && t.steps > 0 {
                self.act_say(Some(format!("Der Assistent hat {} von {} Schritten ausgeführt.", t.done, t.steps)));
            }
            crate::db_app::journal(&format!("bitscomp assist.task chat={chat} n={} end steps={} done={}", t.n, t.steps, t.done));
        }
        self.act.pending = None;
    }

    /// The step's text in its view (board 2).
    pub(crate) fn db_step_text(&self) -> String {
        let Some(p) = &self.act.pending else { return "Der Assistent handelt …".into() };
        let mut s = format!("{}.", capital(&p.said));
        if !p.step.why.is_empty() {
            s.push_str(&format!("\n{}", p.step.why));
        }
        if p.step.action == uilist::Action::Type {
            s.push_str("\nDas Feld ist im Fenster umrandet. Was jetzt dort steht, wird ersetzt.");
        } else if p.element.visible {
            s.push_str("\nEs ist im Fenster umrandet.");
        }
        if let Risk::Final(why) = p.risk {
            s.push_str(&format!("\nAchtung: Das lässt sich wohl nicht rückgängig machen ({why})."));
        }
        s
    }

    /// The line above the step: "Schritt n" (the model plans one step at a time; how many follow nobody knows).
    pub(crate) fn db_step_title(&self) -> Option<String> {
        let n = self.act.task.as_ref().map_or(0, |t| t.steps);
        self.act.pending.as_ref().map(|_| format!("Schritt {n}"))
    }

    /// Under an asked step: the Automodus would have run it (board 2), or why it asks even there.
    /// The line under the popup's buttons as drawn: its own note, else why a step is asked (`ctl durchblick state`
    /// reports the same).
    pub(crate) fn db_note_shown(&self) -> Option<String> {
        let why = self.db_step_why_asked().filter(|_| self.db.stage == crate::db_app::Stage::Step && self.db.asking.is_none());
        self.db.note.clone().or_else(|| why.map(str::to_string))
    }

    pub(crate) fn db_step_why_asked(&self) -> Option<&'static str> {
        let p = self.act.pending.as_ref().filter(|p| p.phase == Phase::Asking)?;
        let t = self.act.task.as_ref()?;
        match (p.risk, t.by_georg, Level::of(&self.settings)) {
            (_, false, _) => Some(crate::assist::REMOTE_CARD),
            (Risk::Normal, true, Level::Ask) => Some(AUTO_WOULD),
            (Risk::Final(_), true, _) => Some("Das fragt er in jeder Stufe."),
            _ => None,
        }
    }

    /// The element of the step, framed on the screen.
    pub(crate) fn db_step_frame(&self) -> Option<[i32; 4]> {
        let p = self.act.pending.as_ref()?;
        let r = p.element.rect;
        (p.element.visible && r[2] > 0 && r[3] > 0).then_some(r)
    }
}

/// EINGABE: what the field holds after a `type` step, read again. Its words only where the window may be seen.
fn field_now(e: Option<&Element>, seen: bool) -> String {
    match e.and_then(|e| e.value.as_deref().filter(|_| !e.has(uilist::State::Secret))) {
        None => " Was jetzt im Feld steht, lässt sich nicht lesen.".into(),
        Some(v) if seen => format!(" Im Feld steht jetzt: „{}“.", v.chars().take(300).collect::<String>()),
        Some(v) => format!(" Im Feld stehen jetzt {} Zeichen.", v.chars().count()),
    }
}

fn walk_has(e: &Element, id: &str) -> bool {
    e.children.iter().any(|c| c.id == id || walk_has(c, id))
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

#[cfg(test)]
#[path = "db_act_app_tests.rs"]
mod tests;
