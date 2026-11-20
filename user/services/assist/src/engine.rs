//! One question from Georg to its answer: the request to the chosen provider (with a bounded number of tries and
//! the fallback provider), the stream to the screen, the model's tool calls through schema check and permission
//! table, their results back to the model, until the model stops. The screen is behind [`Ui`]: the console chat,
//! `--once` and later the desktop window (CL-3b) are three implementations of it.

use crate::config::{self, Config};
use crate::conv::{Conversation, Image, Message, Part, Role, Stop, ToolCall, ToolResult, Turn};
use crate::de;
use crate::json::Json;
use crate::net::conn::{self, Conn};
use crate::net::sse::Sse;
use crate::net::url::Url;
use crate::net::{Ctl, NetError, http};
use crate::provider::program::ask::{Ask, Reply, Why};
use crate::provider::program::line::Step;
use crate::provider::program::{self, Limits, Offer, Piece, pictures};
use crate::provider::{ApiError, ErrKind, Event, Kind, Place, Provider};
use crate::secret::{Pieces, Secret};
use crate::settings::Settings;
use crate::prompt::{self, Origin};
use crate::term::wrap;
use crate::tools::permit::{self, By, Decision, Level};
use crate::tools::{Grant, ToolAnswer, ToolSource, ToolSpec, helper, schema};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// Nobody is there to ask (`--once`).
    CannotAsk,
    /// Georg was asked and did not answer in time. Like a no, with other words for whoever asked.
    Unanswered,
}

/// Where a tool call stands, for a screen that shows tools apart from the answer (the sidebar behind `serve`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    /// Runs without a question; the text says what it does.
    Runs,
    /// Runs now that Georg said yes to it.
    Confirmed,
    /// Did not run: a rule, or nobody to ask; the text says which.
    Refused,
    /// Did not run: Georg said no.
    Declined,
    /// Ended, its result went to the model.
    Done,
    /// Ended with an error; the model was told.
    Failed,
}

pub trait Ui {
    /// A piece of the assistant's answer as it arrives.
    fn text(&mut self, delta: &str);
    /// A line for Georg (German): a tool at work, a pause, the fallback.
    fn note(&mut self, line: &str);
    /// A technical line for the log (the provider's raw status); never holds a key.
    fn log(&mut self, line: &str);
    /// A yes/no question before a tool call.
    fn ask(&mut self, question: &str) -> Answer;
    /// Georg wants the running answer to stop.
    fn cancelled(&mut self) -> bool;
    /// The question before a call of the tool `name`. A screen that does not show tools by name asks as ever.
    fn ask_tool(&mut self, _name: &str, question: &str) -> Answer {
        self.ask(question)
    }
    /// The same for a question somebody else wants answered (a helper program): `wanted` turns false when
    /// that one went away, and a screen that can take a question back does so then. A screen that cannot
    /// asks as ever; the answer then goes nowhere.
    fn ask_tool_while(&mut self, name: &str, question: &str, _wanted: &dyn Fn() -> bool) -> Answer {
        self.ask_tool(name, question)
    }
    /// A step of the tool `name`. A screen without a place of its own for tools shows what Georg has not
    /// read yet as a note; what a question already told him is not said twice.
    fn tool(&mut self, _name: &str, state: ToolState, text: &str) {
        if matches!(state, ToolState::Runs | ToolState::Refused) {
            self.note(text);
        }
    }
    /// The provider `id` is asked now. `notice`: it is a cloud one and Georg wants to be told
    /// (`ai.cloud_notice`).
    fn provider(&mut self, _id: &str, _place: Place, _notice: bool) {}
}

pub struct Session<'a> {
    pub settings: &'a dyn Settings,
    pub tools: &'a dyn ToolSource,
    pub conv: Conversation,
    /// The most bytes one answer or tool result may take (`limits::budget`).
    pub budget: usize,
    /// The owner's home folder (`HOME`), inside which a helper program's tools read unasked. None: nowhere.
    pub home: Option<PathBuf>,
    /// The home whose helper descriptions say which helper takes pictures (`program::pictures`). None: none does.
    pub helpers: Option<PathBuf>,
    /// Where the chat was started: chooses the view's part of the one system prompt.
    pub origin: Origin,
    /// Who asked the question at work: one from the control socket or MCP gets at most the level `ask`.
    pub by: By,
}

enum Fail {
    Api(ApiError),
    /// A rule refused the provider before any connection (German).
    Refused(String),
    /// The provider's helper program failed, or said itself what is wrong (German).
    Helper(String),
}

/// Why a single request got no answer: the German sentence for Georg, and the kind of failure when the
/// provider or the network named one.
pub(crate) struct NoAnswer {
    pub why: String,
    pub kind: Option<ErrKind>,
}

fn net_error(e: NetError) -> ApiError {
    let (kind, raw) = match e {
        NetError::Dns(m) => (ErrKind::Dns, m),
        NetError::Connect(m) => (ErrKind::Network, m),
        NetError::Tls(m) => (ErrKind::Tls, m),
        NetError::Clock(m) => (ErrKind::Clock, m),
        NetError::Timeout => (ErrKind::Timeout, "deadline reached".into()),
        NetError::Cancelled => (ErrKind::Cancelled, "cancelled".into()),
        NetError::Cut(m) => (ErrKind::Cut, m),
        NetError::Protocol(m) => (ErrKind::Protocol, m),
        NetError::TooBig => (ErrKind::TooBig, "answer larger than the memory budget".into()),
    };
    ApiError::new(kind, raw)
}

/// What the model reads when Georg said no to a call.
const SAID_NO: &str = "Georg said no to this call. Do not try it again unless he asks.";

/// The stop reason of an answer that ended in the usual way but holds neither words nor a tool call.
const EMPTY: &str = "empty answer";
/// How often a running helper's answer looks whether Georg tightened the level.
const LEVEL_LOOK: Duration = Duration::from_millis(500);
/// The answer ended because the level was tightened while a helper worked under the looser one.
pub const TIGHTENED: &str = "Die Stufe wurde verschärft, während das Hilfsprogramm arbeitete; die Antwort ist hier beendet. Stell die Frage bitte noch einmal.";

/// How the permission step of a call ended without a run.
enum Halt {
    /// A rule forbids it; the words are for the model.
    Rule(String),
    No,
    Unanswered,
    /// Nobody can be asked here.
    Nobody,
}

/// The permission step of one call on the screen: runs, is refused, or waits for Georg. Ok: it may run, and
/// whether Georg said yes to it. One function for the chat's own tools and for a helper program's.
fn clear(ui: &mut dyn Ui, name: &str, decision: Decision, what: &str, wanted: &dyn Fn() -> bool) -> Result<bool, Halt> {
    match decision {
        Decision::Run => {
            ui.tool(name, ToolState::Runs, what);
            Ok(false)
        }
        Decision::Refuse(why) => {
            ui.tool(name, ToolState::Refused, &format!("Werkzeug {name} nicht ausgeführt (gesperrt)."));
            Err(Halt::Rule(why))
        }
        Decision::Ask(question) => match ui.ask_tool_while(name, &question, wanted) {
            Answer::Yes => {
                ui.tool(name, ToolState::Confirmed, what);
                Ok(true)
            }
            answer @ (Answer::No | Answer::Unanswered) => {
                ui.tool(name, ToolState::Declined, &question);
                Err(if answer == Answer::No { Halt::No } else { Halt::Unanswered })
            }
            Answer::CannotAsk => {
                ui.tool(name, ToolState::Refused, &format!("Nicht ausgeführt, weil niemand gefragt werden kann: {question}"));
                Err(Halt::Nobody)
            }
        },
    }
}

/// The screen and the permission step behind a running helper program, and (contract version 4) the session
/// whose tool code runs the helper's calls of assist's tools.
struct HelperHost<'u, 'a, 's> {
    ui: &'u RefCell<&'a mut dyn Ui>,
    shown: &'u mut bool,
    provider: &'u str,
    /// The level this question works under (the origin's cap applied).
    level: Level,
    session: &'u Session<'s>,
    cfg: &'u Config,
    offered: &'u [ToolSpec],
    /// Calls so far: each counts as one round of `ai.tool_rounds` (a helper's turns cannot be seen from here).
    calls: u32,
}

impl program::Host for HelperHost<'_, '_, '_> {
    fn piece(&mut self, piece: Piece) {
        let mut ui = self.ui.borrow_mut();
        match piece {
            Piece::Text(t) => {
                *self.shown = true;
                ui.text(t);
            }
            Piece::Note(n) => ui.note(n),
            Piece::Log(l) => ui.log(&format!("provider={} {l}", self.provider)),
            Piece::Tool(name, step, what) => {
                let state = match step {
                    Step::Runs => ToolState::Runs,
                    Step::Done => ToolState::Done,
                    Step::Failed => ToolState::Failed,
                };
                ui.tool(name, state, what);
            }
        }
    }

    fn ask(&mut self, ask: &Ask, wanted: &dyn Fn() -> bool) -> Reply {
        let (decision, what) = helper::judge(self.session.level_now(self.cfg), ask);
        let name = wrap::inline(ask.tool.trim(), 64);
        if matches!(&decision, Decision::Refuse(_)) && helper::too_long(ask) {
            let shown = wrap::inline(&what, 120);
            self.ui.borrow_mut().tool(&name, ToolState::Refused, &format!("Nicht gefragt und nicht ausgeführt, weil zu lang, um es ganz zu zeigen: {shown}"));
            return Reply::No(Why::TooLong);
        }
        match clear(&mut **self.ui.borrow_mut(), &name, decision, &what, wanted) {
            Ok(_) => Reply::Yes,
            Err(Halt::Rule(_)) => Reply::No(Why::Never),
            Err(Halt::No) => Reply::No(Why::Declined),
            Err(Halt::Unanswered) => Reply::No(Why::Unanswered),
            Err(Halt::Nobody) => Reply::No(Why::CannotAsk),
        }
    }

    fn own_tools(&self) -> bool {
        self.level != Level::Answer
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.offered.to_vec()
    }

    /// The call as the engine runs an HTTP provider's: the same schema check, permission step, tool code, tool
    /// lines and result. The same bound too: more calls than `ai.tool_rounds` end the answer with the chat's words.
    fn call(&mut self, name: &str, input: &Json, wanted: &dyn Fn() -> bool) -> Result<ToolAnswer, String> {
        self.calls += 1;
        if self.calls > self.cfg.tool_rounds {
            return Err(too_many_rounds(self.cfg.tool_rounds));
        }
        // A call that may have changed something: the fallback provider must not be asked the same question
        // again (it would not know what ran).
        *self.shown = true;
        let call = ToolCall::new(format!("helper-{}", self.calls), name.to_string(), input.to_text());
        let r = self.session.run_tool(self.cfg, self.offered, &call, &mut **self.ui.borrow_mut(), wanted);
        Ok(if r.is_error { Err(r.content) } else { Ok(r.content) })
    }

    fn pictures(&mut self) -> Vec<Image> {
        self.session.tools.take_pictures()
    }
}

fn too_many_rounds(n: u32) -> String {
    format!("Der Assistent wollte mehr als {n} Werkzeug-Runden für eine Frage (die Grenze steht in ai.tool_rounds).")
}

/// `turn` without the provider's key, wherever the provider put it: the words (they are kept in the
/// conversation and shown again as history), a tool call's arguments (the question to Georg shows them, and
/// no tool has a use for the key) and a stop reason of the provider's own.
fn scrubbed(mut turn: Turn, key: &Secret) -> Turn {
    if key.is_empty() {
        return turn;
    }
    for part in &mut turn.message.parts {
        match part {
            Part::Text(t) if t.contains(key.expose()) => *t = key.scrub(t),
            Part::Call(c) if c.args_text.contains(key.expose()) => *c = ToolCall::new(c.id.clone(), c.name.clone(), key.scrub(&c.args_text)),
            _ => {}
        }
    }
    if let Stop::Other(why) = &mut turn.stop {
        *why = key.scrub(why);
    }
    turn
}

/// The wait before try `n` (0-based) when the provider names none: 1 s, 2 s, 4 s, ...
fn backoff(n: u32) -> Duration {
    Duration::from_secs(1u64 << n.min(16))
}

fn retry_after(header: Option<&str>) -> Option<Duration> {
    let s = header?.trim().parse::<f64>().ok()?;
    (s.is_finite() && s >= 0.0).then(|| Duration::from_secs_f64(s))
}

impl<'a> Session<'a> {
    pub fn new(settings: &'a dyn Settings, tools: &'a dyn ToolSource, budget: usize) -> Session<'a> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        Session { settings, tools, conv: Conversation::default(), budget, home, helpers: pictures::owner_home(), origin: Origin::Console, by: By::Hand }
    }

    /// The level a question of this session works under: Georg's, at most `ask` for a remote question.
    pub fn level(&self, cfg: &Config) -> Level {
        cfg.level.for_origin(self.by)
    }

    /// The level a step of a running question works under: never more than the question began with, and less
    /// as soon as Georg tightened `ai.level` meanwhile (tightening is free and holds at once; a loosening waits
    /// for the next question). A value that cannot be read counts as `ask`, as in [`config::load`].
    pub fn level_now(&self, cfg: &Config) -> Level {
        let now = crate::settings::get(self.settings, Level::KEY).ok().and_then(|v| v.as_str().and_then(Level::parse)).unwrap_or(Level::Ask);
        self.level(cfg).min(now.for_origin(self.by))
    }

    /// Whether the helper program at `address` announced that it takes pictures (contract version 3).
    pub fn takes_pictures(&self, address: &str) -> bool {
        self.helpers.as_deref().is_some_and(|h| pictures::takes_pictures(h, address))
    }

    /// One question through all its tool rounds. Ok is how the answer ended: [`Stop::End`] for a whole one,
    /// anything else for one that was cut, refused or empty (Georg was told through `ui.note`). Err is the
    /// German sentence for Georg; the conversation is then as it was before the question.
    pub fn ask(&mut self, text: &str, ui: &mut dyn Ui) -> Result<Stop, String> {
        self.ask_with(text, None, ui)
    }

    /// The same with a picture after the question's text (DB3).
    pub fn ask_with(&mut self, text: &str, image: Option<Image>, ui: &mut dyn Ui) -> Result<Stop, String> {
        let cfg = config::load(self.settings).map_err(|e| format!("Die Einstellungen sind nicht erreichbar ({e}). Läuft settingsd?"))?;
        let level = self.level(&cfg);
        let offered: Vec<ToolSpec> = self.tools.list().into_iter().filter(|t| permit::offered(level, t)).collect();
        self.conv.system = prompt::system(&cfg.name, level, self.origin);
        let mark = self.conv.messages.len();
        self.conv.messages.push(Message::user_with(text, image));
        let r = self.rounds(&cfg, &offered, ui);
        // A question without anything to keep after it (an error, an empty answer) is not kept either: the
        // next request would end in two questions in a row.
        if r.is_err() || self.conv.messages.len() == mark + 1 {
            self.conv.messages.truncate(mark);
        }
        r
    }

    fn rounds(&mut self, cfg: &Config, offered: &[ToolSpec], ui: &mut dyn Ui) -> Result<Stop, String> {
        let mut round = 0;
        loop {
            let turn = self.send(cfg, offered, ui)?;
            let calls: Vec<ToolCall> = turn.message.calls().into_iter().cloned().collect();
            // An answer with neither words nor calls is not kept: providers refuse an empty turn when the
            // conversation comes back with the next question.
            let empty = calls.is_empty() && turn.message.text().trim().is_empty();
            if !empty {
                self.conv.messages.push(turn.message);
            }
            let unrun = |why: &str| -> Vec<ToolResult> {
                calls.iter().map(|c| ToolResult { id: c.id.clone(), name: c.name.clone(), content: format!("not run: {why}"), is_error: true }).collect()
            };
            match turn.stop {
                Stop::ToolUse if !calls.is_empty() => {
                    round += 1;
                    if round > cfg.tool_rounds {
                        return Err(too_many_rounds(cfg.tool_rounds));
                    }
                    let results = calls.iter().map(|c| self.run_tool(cfg, offered, c, ui, &|| true)).collect();
                    let mut m = Message::results(results);
                    // Pictures a tool brought (a window, the screen) go with its result.
                    m.parts.extend(self.tools.take_pictures().into_iter().map(Part::Image));
                    self.conv.messages.push(m);
                    if ui.cancelled() {
                        return Err("Abgebrochen.".into());
                    }
                }
                stop => {
                    // A tool call of a turn that did not end as a tool turn is incomplete or unwanted: it gets
                    // an answer (the next request needs one for every call) but never runs.
                    let stop = match stop {
                        // A tool turn without a call, or an end without a word: nothing Georg can use.
                        Stop::End | Stop::ToolUse if empty => Stop::Other(EMPTY.into()),
                        Stop::ToolUse => Stop::End,
                        other => other,
                    };
                    match &stop {
                        Stop::End | Stop::ToolUse => {}
                        Stop::MaxTokens => ui.note("Die Antwort wurde an der Längengrenze abgeschnitten (ai.max_tokens)."),
                        Stop::Refusal => ui.note("Der Anbieter hat die Antwort abgelehnt."),
                        Stop::Other(why) => {
                            let why = wrap::inline(why, 80);
                            ui.log(&format!("stop={why:?}"));
                            ui.note(&if why == EMPTY {
                                "Der Anbieter hat eine leere Antwort geschickt.".to_string()
                            } else if empty {
                                format!("Der Anbieter hat nichts geantwortet (gemeldet: „{why}“).")
                            } else {
                                format!("Der Anbieter hat die Antwort vorzeitig beendet; sie kann unvollständig sein (gemeldet: „{why}“).")
                            });
                        }
                    }
                    if !calls.is_empty() {
                        self.conv.messages.push(Message::results(unrun("the answer stopped before the call was complete")));
                    }
                    return Ok(stop);
                }
            }
        }
    }

    /// Schema, permission, the call. Whatever happens, the model gets an answer for the call. `wanted` turns false
    /// when whoever made the call went away (a helper's caller): a question about it is then taken back.
    fn run_tool(&self, cfg: &Config, offered: &[ToolSpec], call: &ToolCall, ui: &mut dyn Ui, wanted: &dyn Fn() -> bool) -> ToolResult {
        let answer = (|| -> Result<String, String> {
            // Esc during an earlier call of this turn: nothing more of it runs.
            if ui.cancelled() {
                return Err("not run: Georg pressed Esc".into());
            }
            let Some(spec) = offered.iter().find(|t| t.name == call.name) else {
                // A tool the level does not offer (K2: at `answer` a change is not offered, and refused) says why.
                if self.tools.list().iter().any(|t| t.name == call.name) {
                    return Err(permit::not_at_answer(&call.name));
                }
                return Err(format!("unknown tool {:?}", call.name));
            };
            let Some(args) = &call.args else {
                // The model's own text back, as JSON, so it can see what it wrote and try again.
                return Err(Json::obj().with("INVALID_JSON", call.args_text.as_str()).to_text());
            };
            schema::check(&spec.schema, args).map_err(|e| format!("invalid arguments: {e}"))?;
            let settled = self.tools.settle(&spec.name, args)?;
            let args = &settled;
            let level = self.level_now(cfg);
            let verdict = permit::judge(level, spec, args, self.settings);
            let confirmed = clear(ui, &spec.name, verdict.decision, &verdict.what, wanted).map_err(|halt| match halt {
                Halt::Rule(why) => why,
                Halt::No | Halt::Unanswered => SAID_NO.to_string(),
                Halt::Nobody => "this call needs Georg's yes, and nobody can be asked in this mode (assist --once). Tell him what you wanted to do.".to_string(),
            })?;
            let mut answer = self.call(&spec.name, args, Grant { confirmed, pin: verdict.pin, level }, ui);
            // A key of settingsd's class "confirm" behind a tool that is allowed without asking.
            if !confirmed && answer.as_ref().is_err_and(|e| e.starts_with("needs_confirm")) {
                answer = match ui.ask_tool(&spec.name, &verdict.what) {
                    Answer::Yes => {
                        ui.tool(&spec.name, ToolState::Confirmed, &verdict.what);
                        self.call(&spec.name, args, Grant { confirmed: true, pin: verdict.pin, level }, ui)
                    }
                    Answer::No | Answer::Unanswered => {
                        ui.tool(&spec.name, ToolState::Declined, &verdict.what);
                        return Err(SAID_NO.into());
                    }
                    Answer::CannotAsk => answer,
                };
            }
            ui.tool(&spec.name, if answer.is_ok() { ToolState::Done } else { ToolState::Failed }, &verdict.what);
            match answer {
                Ok(text) if text.len() > self.budget => Err("the tool's answer is larger than this machine can hold; ask for less".into()),
                other => other,
            }
        })();
        let (content, is_error) = match answer {
            Ok(t) => (t, false),
            Err(t) => (t, true),
        };
        ToolResult { id: call.id.clone(), name: call.name.clone(), content, is_error }
    }

    /// The call itself. A tool that waits for someone else (the kernel's tools socket) is given up when Georg
    /// presses Esc: the keyboard is looked at while it runs.
    fn call(&self, name: &str, args: &Json, grant: Grant, ui: &mut dyn Ui) -> ToolAnswer {
        let ui = RefCell::new(ui);
        self.tools.call_with(name, args, grant, &|| ui.borrow_mut().cancelled())
    }

    /// The answer of the first provider that gives one: the chat's provider, then its fallback when the first
    /// is refused by a rule or away before it said anything.
    fn send(&self, cfg: &Config, offered: &[ToolSpec], ui: &mut dyn Ui) -> Result<Turn, String> {
        let ids = cfg.candidates()?;
        let mut first_problem: Option<String> = None;
        for (i, id) in ids.iter().enumerate() {
            let next = ids.get(i + 1);
            let p = match cfg.provider(id) {
                Ok(p) => p,
                Err(why) => return Err(first_problem.unwrap_or(why)),
            };
            let (problem, shown) = match cfg.admit(&p).and_then(|_| config::with_key(self.settings, p)) {
                Err(why) => (why, false),
                Ok(p) => {
                    let mut shown = false;
                    ui.provider(&p.id, p.place, cfg.cloud_notice && p.place == Place::Cloud);
                    match self.attempt(cfg, &p, offered, ui, &mut shown) {
                        Ok(turn) => return Ok(scrubbed(turn, &p.key)),
                        Err(Fail::Refused(why) | Fail::Helper(why)) => (p.key.scrub(&why), shown),
                        Err(Fail::Api(e)) => {
                            ui.log(&p.key.scrub(&format!("provider={} {}", p.id, e.log_line())));
                            let why = de::sentence(&e, &p.id, &p.model);
                            if !e.kind.away() || shown {
                                return Err(why);
                            }
                            (why, shown)
                        }
                    }
                }
            };
            match next {
                Some(n) if !shown => ui.note(&format!("{problem} Ich nehme „{n}“.")),
                _ => return Err(first_problem.unwrap_or(problem)),
            }
            first_problem.get_or_insert(problem);
        }
        Err(first_problem.unwrap_or_else(|| "Kein Anbieter hat geantwortet.".into()))
    }

    /// One request to `p` alone with the conversation as it stands: no second try, no fallback, no tools
    /// (`assist --test`). The provider's own words go to the log.
    pub(crate) fn single(&self, cfg: &Config, p: &Provider, ui: &mut dyn Ui) -> Result<Turn, NoAnswer> {
        let mut shown = false;
        match self.exchange(cfg, p, &[], ui, &mut shown) {
            Ok(turn) => Ok(scrubbed(turn, &p.key)),
            // A helper's own words: it gets no key today, and this stays true if one ever does.
            Err(Fail::Refused(why) | Fail::Helper(why)) => Err(NoAnswer { why: p.key.scrub(&why), kind: None }),
            Err(Fail::Api(e)) => {
                ui.log(&p.key.scrub(&format!("provider={} {}", p.id, e.log_line())));
                Err(NoAnswer { why: de::sentence(&e, &p.id, &p.model), kind: Some(e.kind) })
            }
        }
    }

    /// Tries `p` up to `ai.retries` more times while it says "later" and nothing was shown yet.
    fn attempt(&self, cfg: &Config, p: &Provider, offered: &[ToolSpec], ui: &mut dyn Ui, shown: &mut bool) -> Result<Turn, Fail> {
        let mut tries = 0;
        loop {
            let e = match self.exchange(cfg, p, offered, ui, shown) {
                Ok(turn) => return Ok(turn),
                Err(Fail::Api(e)) => e,
                Err(refused) => return Err(refused),
            };
            let wait = e.retry_after.unwrap_or_else(|| backoff(tries));
            // A pause longer than Georg's own patience for an answer is reported, not sat out.
            if !e.kind.retryable() || *shown || tries >= cfg.retries || wait > cfg.timeout {
                return Err(Fail::Api(e));
            }
            tries += 1;
            ui.log(&p.key.scrub(&format!("provider={} {} retry={tries}", p.id, e.log_line())));
            let what = if e.kind == ErrKind::RateLimit { "bittet um eine Pause" } else { "ist gerade überlastet" };
            ui.note(&format!("„{}“ {what}. Neuer Versuch in {} s (Esc bricht ab).", p.id, wait.as_secs().max(1)));
            let until = Instant::now() + wait;
            while Instant::now() < until {
                if ui.cancelled() {
                    return Err(Fail::Api(ApiError::new(ErrKind::Cancelled, "cancelled while waiting")));
                }
                std::thread::sleep(Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())));
            }
        }
    }

    /// One answer from a helper program (`Kind::Program`): the conversation as text in, text out. The chat's
    /// tools (`offered`, contract version 4) are called through the tool socket while the helper runs and run
    /// right there, through [`Session::run_tool`] as an HTTP provider's calls do, so its turn never holds a call.
    /// What tools it has of its own it shows as tool lines and asks about through the bell, under the level (none
    /// at `answer`). Neither kind of call is kept in the conversation: Claude Code's own session keeps them.
    fn ask_helper(&self, cfg: &Config, p: &Provider, offered: &[ToolSpec], ui: &mut dyn Ui, shown: &mut bool) -> Result<Turn, Fail> {
        let fail = |what: String| Fail::Helper(format!("Das Hilfsprogramm von „{}“ {what}", p.id));
        // The file may have changed since the settings were read.
        let path = program::check(&p.address).map_err(|why| fail(format!("ist nicht brauchbar ({}): {why}.", p.address)))?;
        // A fallback helper after a tightening starts under the tighter level.
        let level = self.level_now(cfg);
        let home = self.home.as_deref().and_then(|h| std::fs::canonicalize(h).ok()).filter(|h| h.parent().is_some());
        let offer = Offer {
            system_tools: prompt::own_tools(level),
            home: home.as_deref().and_then(Path::to_str).unwrap_or("").into(),
            allow: helper::told(level),
            tools: offered.to_vec(),
            tools_system: prompt::TOOLS.into(),
            tools_only: level == Level::Answer,
            level: level.name(),
        };
        let pictures = self.takes_pictures(&p.address);
        let pictured = self.conv.messages.last().is_some_and(|m| m.parts.iter().any(|x| matches!(x, Part::Image(_))));
        if pictured && !pictures {
            ui.log(&format!("provider={} picture left out: its helper does not say it takes pictures", p.id));
        }
        // The one prompt without the words about the tools, which go as `tools_system` to a helper that can hand
        // them on.
        let system = prompt::base(&cfg.name, level, self.origin);
        let request = |doors: &program::Doors| program::request(p, &system, &self.conv, &offer, doors, pictures);
        let ui = RefCell::new(ui);
        // Claude Code's own tools were allowed ahead for the level the answer began with: when Georg tightens
        // the level meanwhile, the helper is ended, since nothing else could hold those tools back.
        let tightened = std::cell::Cell::new(false);
        let looked = std::cell::Cell::new(Instant::now());
        let cancel = || {
            if !tightened.get() && looked.get().elapsed() >= LEVEL_LOOK {
                looked.set(Instant::now());
                tightened.set(self.level_now(cfg) < level);
            }
            tightened.get() || ui.borrow_mut().cancelled()
        };
        // `ai.timeout_s` is the longest silence here, not the longest answer: see [`Limits`].
        let limits = Limits { silence: cfg.timeout, budget: self.budget, cancel: &cancel };
        let mut host = HelperHost { ui: &ui, shown, provider: &p.id, level, session: self, cfg, offered, calls: 0 };
        let (text, stop) = program::run(path, &request, &limits, &mut host).map_err(|e| {
            ui.borrow_mut().log(&format!("provider={} helper failed: {e:?}", p.id));
            let api = |kind| Fail::Api(ApiError::new(kind, "helper program"));
            match e {
                program::Fail::Cancelled if tightened.get() => Fail::Helper(TIGHTENED.into()),
                program::Fail::Cancelled => api(ErrKind::Cancelled),
                program::Fail::Timeout => api(ErrKind::Timeout),
                program::Fail::TooBig => api(ErrKind::TooBig),
                program::Fail::Start(why) => fail(format!("ließ sich nicht starten ({}): {why}.", p.address)),
                program::Fail::Garbage(_) => fail("hat etwas ausgegeben, das nicht zu lesen war.".into()),
                program::Fail::Ended => fail("hat sich beendet, ohne die Antwort abzuschließen.".into()),
                program::Fail::Said(why) if why.is_empty() => fail("hat einen Fehler gemeldet, ohne ihn zu nennen.".into()),
                program::Fail::Said(why) => Fail::Helper(why),
            }
        })?;
        Ok(Turn { message: Message { role: Role::Assistant, parts: vec![Part::Text(text)], raw: None }, stop })
    }

    /// One request and its streamed answer.
    fn exchange(&self, cfg: &Config, p: &Provider, offered: &[ToolSpec], ui: &mut dyn Ui, shown: &mut bool) -> Result<Turn, Fail> {
        if p.kind == Kind::Program {
            return self.ask_helper(cfg, p, offered, ui, shown);
        }
        let api = |e: NetError| Fail::Api(net_error(e));
        let url = Url::parse(&p.address).map_err(|m| Fail::Api(ApiError::new(ErrKind::Protocol, m)))?;
        let adapter = p.kind.adapter();
        let req = adapter.request(p, &self.conv, offered);
        // The engine alone talks to the screen while the network waits: the cell hands `ui` to whichever of
        // the two (the cancel question, the text sink) runs at the moment; they never nest.
        let ui = RefCell::new(ui);
        let cancel = || ui.borrow_mut().cancelled();
        let ctl = Ctl { deadline: Instant::now() + cfg.timeout, budget: self.budget, cancel: &cancel };
        let addrs = conn::resolve(&url, &ctl).map_err(api)?;
        config::admit_addrs(p, url.https, cfg.local_only, &addrs).map_err(Fail::Refused)?;
        let conn = Conn::open(&url, &addrs, &ctl).map_err(api)?;
        let mut resp = http::post(conn, &url, &req.path, &req.headers, &req.body, &ctl).map_err(api)?;
        if resp.status != 200 {
            // A redirect is not followed: the key would travel to an address Georg did not set.
            if (300..400).contains(&resp.status) {
                return Err(Fail::Api(ApiError::http(ErrKind::Protocol, resp.status, format!("redirect to {:?}", resp.header("location").unwrap_or("")))));
            }
            let body = resp.read_all(&ctl).unwrap_or_default();
            let mut e = adapter.error(resp.status, &String::from_utf8_lossy(&body));
            e.status = Some(resp.status);
            e.retry_after = retry_after(resp.header("retry-after")).or(e.retry_after);
            return Err(Fail::Api(e));
        }
        let mut parser = adapter.parser(p);
        let mut sse = Sse::default();
        let mut used = 0usize;
        // A provider can repeat the key in its answer as it can in an error text: Georg's screen and the
        // conversation get it as little as the log does.
        let pieces = RefCell::new(Pieces::new(&p.key));
        let mut sink = |e: Event| {
            if let Event::Text(t) = e {
                *shown = true;
                let clear = pieces.borrow_mut().push(t);
                if !clear.is_empty() {
                    ui.borrow_mut().text(&clear);
                }
            }
        };
        loop {
            let piece = resp.read(&ctl).map_err(api)?;
            let events = if piece.is_empty() { sse.end() } else { sse.feed(&piece) };
            used = used.saturating_add(piece.len());
            if used > self.budget {
                return Err(api(NetError::TooBig));
            }
            for ev in &events {
                parser.event(&ev.event, &ev.data, &mut sink).map_err(Fail::Api)?;
            }
            if piece.is_empty() || parser.done() {
                break;
            }
        }
        let rest = pieces.borrow_mut().end();
        if !rest.is_empty() {
            ui.borrow_mut().text(&rest);
        }
        parser.finish().map_err(|mut e| {
            // Not a stream at all (a web page, a plain JSON answer): the address or the kind is wrong.
            let ctype = resp.header("content-type").unwrap_or("");
            if e.kind == ErrKind::Cut && !ctype.contains("event-stream") {
                e = ApiError::http(ErrKind::Protocol, 200, format!("not an event stream (content-type {ctype:?})"));
            }
            Fail::Api(e)
        })
    }
}
