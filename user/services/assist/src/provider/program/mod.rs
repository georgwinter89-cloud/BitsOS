//! Provider kind `program`: the answer comes from a helper program on this machine instead of an HTTP
//! service. The helper gets the conversation as one JSON line on its stdin and answers with JSON lines on its
//! stdout (HELPER.md beside this crate is the description for whoever writes one). It is a program like any
//! other and is trusted like a provider, which is not at all: what it prints is bounded and checked line by
//! line, it runs in a process group of its own, and that group is ended when the answer is complete, when it
//! falls silent for too long, when Georg presses Esc and when this program goes away.
//!
//! Since version 2 of the contract a helper may tell what it is doing (tool lines) and ask before it does
//! something ([`bell`]): the questions go through `ask`, which is the engine's permission step. Since version 3
//! the newest question may carry a picture, for a helper that announced it takes pictures ([`pictures`]). Since
//! version 4 the helper's model may call the tools assist offers for this answer, through a socket of the answer
//! ([`calls`]): the engine runs each call as it runs an HTTP provider's. Since version 5 (EINS) one level decides
//! for both kinds of tools, in every chat: the request names it, and at `answer` the helper's own tools are off.

pub mod ask;
pub mod bell;
pub mod calls;
pub mod line;
pub mod pictures;
mod pipes;
mod tree;

use super::{Adapter, ApiError, ErrKind, HttpRequest, Parser, Provider};
use crate::conv::{Conversation, Part, Role, Stop, Turn};
use crate::json::Json;
use crate::term::{sys, wrap};
use crate::tools::{Class, ToolAnswer, ToolSpec};
use ask::{Ask, Reply, Why};
use bell::Bell;
use calls::Calls;
use line::{Line, Step};
use pipes::{LOG_LINE, Msg, read_err, read_out};
use std::cell::RefCell;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

/// The version of the line protocol, sent with every request.
pub const PROTOCOL: i64 = 5;
/// How often a wait looks at the silence and at Esc.
const TICK: Duration = Duration::from_millis(50);
/// A helper that said `done` gets this long to end by itself (to write down what it keeps).
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// After SIGTERM, before SIGKILL: time for the helper's program to end what it started (a shell command
/// it runs) and write down what it keeps.
const TERM_GRACE: Duration = Duration::from_secs(3);
/// How long the helper's last words on stderr are waited for once it is gone (they are there at once
/// unless something it started and let go still holds the pipe).
const LAST_WORDS: Duration = Duration::from_millis(300);
/// The most characters of a status word (`note`) and of the helper's own error sentence on screen.
const NOTE_MAX: usize = 120;
const ERROR_MAX: usize = 400;
/// The most characters of a tool's name and of what a tool line says it does.
const TOOL_NAME_MAX: usize = 64;
const TOOL_TEXT_MAX: usize = 400;

/// What a helper's address must be: the full path of a regular file that may be run. Err: what is wrong (a
/// German half sentence).
pub fn check(address: &str) -> Result<&Path, String> {
    let path = Path::new(address);
    if !path.is_absolute() {
        return Err("es muss ein voller Pfad sein, der mit / beginnt".into());
    }
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "die Datei gibt es nicht".to_string(),
        _ => format!("die Datei lässt sich nicht ansehen ({e})"),
    })?;
    if !meta.is_file() {
        return Err("das ist keine Datei".into());
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err("die Datei ist nicht als Programm ausführbar".into());
    }
    Ok(path)
}

/// What a helper of version 2 is told besides the conversation.
#[derive(Clone, Debug, Default)]
pub struct Offer {
    /// The paragraph a helper adds to `system` when it gives its model tools (who asks Georg, what a no
    /// means).
    pub system_tools: String,
    /// The owner's home folder, inside which reading runs unasked.
    pub home: String,
    /// The owner's level per kind of action (`always`, `ask`, `never`): what a helper may leave unasked.
    pub allow: Vec<(&'static str, &'static str)>,
    /// Version 4: assist's tools for this answer, the same an HTTP provider would get; empty: none.
    pub tools: Vec<ToolSpec>,
    /// What the prompt of an HTTP provider says about these tools, for a helper that hands them on.
    pub tools_system: String,
    /// True: the helper's model may use no tools of its own, only these (the level `answer`): no bell, and the
    /// request says so (`own_tools: false`).
    pub tools_only: bool,
    /// Version 5: the level the question works under (`answer`, `ask`, `auto`); empty: not said.
    pub level: &'static str,
}

/// Where the helper reaches assist during this answer: the bell for its own tools' questions, the tool socket
/// for assist's tools. None: there is none this time.
#[derive(Clone, Copy, Debug, Default)]
pub struct Doors<'a> {
    pub bell: Option<&'a Path>,
    pub tools: Option<&'a Path>,
}

fn class_word(c: Class) -> &'static str {
    match c {
        Class::Read => "read",
        Class::Change => "change",
        Class::Other => "other",
    }
}

/// The offered tools as the request and the tool socket's list name them.
pub fn specs(tools: &[ToolSpec]) -> Json {
    let list: Vec<Json> = tools
        .iter()
        .map(|t| Json::obj().with("name", t.name.as_str()).with("description", t.description.as_str()).with("schema", t.schema.clone()).with("class", class_word(t.class)))
        .collect();
    Json::from(list)
}

/// The one line a helper reads: the conversation as roles and texts (tool calls and their results, which
/// only an HTTP provider of this crate can have made, are left out), the options of the request and, when
/// there is one, the bell for its questions. `pictures`: the helper announced it takes pictures; then the
/// newest question carries the newest picture of the conversation that fits, also on a follow-up: a helper
/// may keep nothing between requests (claude-abo runs picture questions without a saved session), so a
/// follow-up about the same window would otherwise know only the text. Only one picture per line, so the
/// line stays bounded.
pub fn request(p: &Provider, system: &str, conv: &Conversation, offer: &Offer, doors: &Doors, pictures: bool) -> String {
    let last = conv.messages.len().checked_sub(1);
    fn picture(m: &crate::conv::Message) -> Option<&crate::conv::Image> {
        m.parts.iter().find_map(|p| if let Part::Image(im) = p { Some(im) } else { None })
    }
    // The newest fitting picture of Georg's questions goes on the newest question.
    let newest = conv.messages.iter().rev().filter(|m| m.role == Role::User).filter_map(picture).find(|im| pictures::fits(im));
    let messages: Vec<Json> = conv
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let text = m.text();
            let mut j = Json::obj().with("role", if m.role == Role::User { "user" } else { "assistant" }).with("text", text.as_str());
            if let Some(image) = newest.filter(|_| pictures && Some(i) == last && m.role == Role::User) {
                j = j.with("image", Json::obj().with("media_type", image.media_type.as_str()).with("data", image.base64.as_str()));
            }
            (!text.trim().is_empty()).then_some(j)
        })
        .collect();
    let mut j = Json::obj().with("bitsos_helper", PROTOCOL).with("provider", p.id.as_str()).with("model", p.model.as_str()).with("max_tokens", p.max_tokens).with("system", system).with("messages", messages);
    if !offer.level.is_empty() {
        j = j.with("level", offer.level);
    }
    // Without a bell a helper cannot ask, so nothing about tools is said to it: it then is what a helper of
    // version 1 was.
    if let Some(bell) = doors.bell.and_then(Path::to_str).filter(|_| !offer.tools_only) {
        let allow = offer.allow.iter().fold(Json::obj(), |j, (kind, level)| j.with(kind, *level));
        j = j.with("bell", bell).with("system_tools", offer.system_tools.as_str()).with("home", offer.home.as_str()).with("allow", allow);
    }
    if offer.tools_only {
        j = j.with("own_tools", false);
    }
    if let Some(socket) = doors.tools.and_then(Path::to_str).filter(|_| !offer.tools.is_empty()) {
        j = j.with("tools", specs(&offer.tools)).with("tool_socket", socket);
        if !offer.tools_system.is_empty() {
            j = j.with("tools_system", offer.tools_system.as_str());
        }
    }
    j.to_text()
}

/// What `run` hands on while the helper works.
pub enum Piece<'a> {
    Text(&'a str),
    /// A status word for Georg.
    Note(&'a str),
    /// A technical line (the helper's stderr, the bell).
    Log(&'a str),
    /// A tool of the helper: its name, where it stands, what it does.
    Tool(&'a str, Step, &'a str),
}

/// What an answer may take.
pub struct Limits<'a> {
    /// The longest the helper may be silent: no line on its stdout, no question at the bell. An answer that
    /// works through tools for minutes has no total it must fit into; what bounds it is this silence, the
    /// memory budget and Georg's Esc. While a question waits for Georg the clock stands.
    pub silence: Duration,
    /// The most bytes of the helper's stdout for one answer.
    pub budget: usize,
    pub cancel: &'a dyn Fn() -> bool,
}

/// The screen and the permission step behind a running helper.
pub trait Host {
    fn piece(&mut self, piece: Piece);
    /// A question from the bell. `wanted` turns false when the asker went away or the helper ended: the
    /// question is then taken back.
    fn ask(&mut self, ask: &Ask, wanted: &dyn Fn() -> bool) -> Reply;
    /// Whether the helper's model may use tools of its own: the bell is made only then.
    fn own_tools(&self) -> bool {
        true
    }
    /// Version 4: the tools of assist offered for this answer; empty: no tool socket is made.
    fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    /// One call through the tool socket. `wanted` turns false when the caller went away or the helper ended: a
    /// question about the call is then taken back. Ok: its result for the model. Err: the answer must end here,
    /// with this German sentence for Georg (too many tool rounds).
    fn call(&mut self, name: &str, _input: &Json, _wanted: &dyn Fn() -> bool) -> Result<ToolAnswer, String> {
        Ok(Err(format!("unknown tool {name:?}")))
    }
    /// Version 5: the pictures the last call brought, for its answer on the tool socket.
    fn pictures(&mut self) -> Vec<crate::conv::Image> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// The helper could not be started.
    Start(String),
    /// It printed a line that is not of the protocol (the line, bounded).
    Garbage(String),
    /// It ended, or closed its output, before it said `done`.
    Ended,
    /// It reported an error itself: its sentence (may be empty).
    Said(String),
    Timeout,
    Cancelled,
    TooBig,
}

/// The running helper. Dropping it ends its whole process group and whatever else descends from it, on
/// every way out of `run`.
struct Running {
    child: Child,
    exited: bool,
    /// Its descendants as last seen ([`Running::census`]), those that left its group among them.
    family: Vec<tree::Proc>,
}

impl Running {
    /// True when the helper has ended (it is reaped then).
    fn ended(&mut self) -> bool {
        self.exited = self.exited || !matches!(self.child.try_wait(), Ok(None));
        self.exited
    }

    /// Writes down who descends from the helper now, while the way back to it is still there.
    fn census(&mut self) {
        if self.exited {
            return;
        }
        for p in tree::family(self.child.id()) {
            if !self.family.contains(&p) {
                self.family.push(p);
            }
        }
    }

    fn wait_end(&mut self, at_most: Duration) -> bool {
        let until = Instant::now() + at_most;
        while !self.ended() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.exited
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let group = self.child.id();
        self.census();
        // The group, not only the helper: what it started must not outlive the answer either, in its group
        // or out of it. Everybody gets the same time to end by itself, then nobody gets a second chance.
        sys::signal_group(group, false);
        let strays = |family: &[tree::Proc]| family.iter().filter(|p| tree::runs(p)).map(|p| p.pid).collect::<Vec<_>>();
        strays(&self.family).into_iter().for_each(|pid| sys::signal(pid, false));
        let until = Instant::now() + TERM_GRACE;
        // The helper is reaped as soon as it ends (a zombie would keep the group alive).
        while (!self.ended() || sys::group_lives(group) || !strays(&self.family).is_empty()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        // (The group's number was free again for a moment at most: nothing else can have taken it yet.)
        sys::signal_group(group, true);
        strays(&self.family).into_iter().for_each(|pid| sys::signal(pid, true));
        if !self.exited {
            let _ = self.child.wait();
        }
    }
}

/// Starts the helper at `path`, hands it the request `request` makes (given the bell's and the tool socket's
/// paths, where there are such) and reads its answer. Text, status words, tool lines and its stderr go to
/// `host` as they come, its questions and its calls of assist's tools too. Ok: the whole answer and how it ended.
pub fn run(path: &Path, request: &dyn Fn(&Doors) -> String, limits: &Limits, host: &mut dyn Host) -> Result<(String, Stop), Fail> {
    // The bell first: its name goes into the request. Without one (no place for it, or no tools of its own
    // wanted) the helper runs as a helper of version 1 did.
    let bell = if host.own_tools() {
        match Bell::open() {
            Ok(d) => Some(d),
            Err(e) => {
                host.piece(Piece::Log(&format!("helper: no bell for questions ({e}); the helper runs without tools of its own")));
                None
            }
        }
    } else {
        None
    };
    let offered = host.tools();
    let calls = if offered.is_empty() {
        None
    } else {
        match Calls::open() {
            Ok(c) => Some(c),
            Err(e) => {
                host.piece(Piece::Log(&format!("helper: no tool socket ({e}); the helper gets none of assist's tools")));
                None
            }
        }
    };
    let mut cmd = Command::new(path);
    // No shell, no arguments. A group of its own, so that it and everything it starts can be ended as one.
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    sys::end_with_parent(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| Fail::Start(e.to_string()))?;
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let head = child.id();
    let helper = RefCell::new(Running { child, exited: false, family: Vec::new() });
    let (Some(mut stdin), Some(stdout), Some(stderr)) = pipes else { return Err(Fail::Start("no pipes to the helper".into())) };
    let (tx, rx) = sync_channel(64);
    let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name(name.into()).spawn(f).map(drop).map_err(|e| Fail::Start(format!("no thread: {e}")));
    // Writing and reading on threads of their own: a helper that never reads, never writes or never stops
    // writing holds up one of them, not the wait below. They end when the helper's pipes close.
    let doors = Doors { bell: bell.as_ref().map(|d| d.path.as_path()), tools: calls.as_ref().map(|c| c.path.as_path()) };
    let line = format!("{}\n", request(&doors));
    spawn("helper-in", Box::new(move || drop(stdin.write_all(line.as_bytes()))))?;
    let (tx_out, budget) = (tx.clone(), limits.budget);
    spawn("helper-out", Box::new(move || read_out(stdout, tx_out, budget)))?;
    let tx_err = tx.clone();
    spawn("helper-err", Box::new(move || read_err(stderr, tx_err)))?;
    let uid = usys::geteuid().ok().and_then(|u| u32::try_from(u).ok());
    if let (Some(c), Some(uid)) = (&calls, uid) {
        c.hear(head, uid, specs(&offered), tx.clone(), Box::new(bell::seen), Box::new(bell::parent), Box::new(bell::group)).map_err(|e| Fail::Start(format!("no thread: {e}")))?;
    }
    if let (Some(d), Some(uid)) = (&bell, uid) {
        d.hear(head, uid, tx, Box::new(bell::seen), Box::new(bell::parent)).map_err(|e| Fail::Start(format!("no thread: {e}")))?;
    }

    let mut text = String::new();
    let mut err_open = true;
    let result = answer(&rx, &helper, limits, &mut text, &mut err_open, host);
    helper.borrow_mut().census();
    if result.is_ok() {
        helper.borrow_mut().wait_end(EXIT_GRACE);
    }
    // The bell and the tool socket before the helper: whoever still waits there gets its no while it can
    // still read it, and a call that comes now is refused.
    drop(calls);
    drop(bell);
    drop(helper);
    // What it still said on stderr while it ended: its last words often say why.
    let until = Instant::now() + LAST_WORDS;
    while err_open {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Msg::Err(l)) => host.piece(Piece::Log(&format!("helper: {l}"))),
            Ok(Msg::ErrEnd) | Err(_) => break,
            Ok(_) => {}
        }
    }
    result.map(|stop| (text, stop))
}

fn answer(rx: &Receiver<Msg>, helper: &RefCell<Running>, limits: &Limits, text: &mut String, err_open: &mut bool, host: &mut dyn Host) -> Result<Stop, Fail> {
    // The last sign of life: a line of the protocol, a question, the end of Georg's thinking about one.
    let mut alive = Instant::now();
    loop {
        match rx.recv_timeout(TICK) {
            Ok(Msg::Out(bytes)) => {
                let line = String::from_utf8_lossy(&bytes);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match line::parse(line) {
                    None => return Err(Fail::Garbage(wrap::inline(line, LOG_LINE))),
                    Some(Line::Text(t)) => {
                        text.push_str(&t);
                        host.piece(Piece::Text(&t));
                    }
                    Some(Line::Note(n)) => host.piece(Piece::Note(&wrap::inline(&n, NOTE_MAX))),
                    Some(Line::Tool(name, step, what)) => {
                        // A tool call may start a program: know it while it still descends from the helper.
                        helper.borrow_mut().census();
                        host.piece(Piece::Tool(&wrap::inline(&name, TOOL_NAME_MAX), step, &wrap::inline(&what, TOOL_TEXT_MAX)));
                    }
                    Some(Line::Done(Ok(stop))) => return Ok(stop),
                    Some(Line::Done(Err(said))) => return Err(Fail::Said(wrap::inline(said.trim(), ERROR_MAX))),
                    Some(Line::Unknown) => {}
                }
                alive = Instant::now();
            }
            Ok(Msg::Ring(ring)) => {
                helper.borrow_mut().census();
                let reply = if (limits.cancel)() {
                    Reply::No(Why::Stopped)
                } else {
                    host.ask(&ring.ask, &|| !ring.gone() && !helper.borrow_mut().ended())
                };
                let _ = ring.answer.send(reply);
                // The time Georg took is not the helper's silence.
                alive = Instant::now();
            }
            Ok(Msg::Call(call)) => {
                helper.borrow_mut().census();
                let answer = if (limits.cancel)() {
                    Err("not run: Georg pressed Esc".to_string())
                } else if call.gone() || helper.borrow_mut().ended() {
                    Err(calls::ENDED.to_string())
                } else {
                    match host.call(&call.name, &call.input, &|| !call.gone() && !helper.borrow_mut().ended()) {
                        Ok(a) => a,
                        Err(sentence) => {
                            let _ = call.answer.send(Err("not run: the limit of tool rounds is reached".to_string()).into());
                            return Err(Fail::Said(sentence));
                        }
                    }
                };
                let _ = call.answer.send(calls::Answered { answer, pictures: host.pictures() });
                // The time a call took (Georg's yes among it) is not the helper's silence.
                alive = Instant::now();
            }
            Ok(Msg::Bell(l)) => host.piece(Piece::Log(&format!("helper: {l}"))),
            Ok(Msg::Err(l)) => host.piece(Piece::Log(&format!("helper: {l}"))),
            Ok(Msg::ErrEnd) => *err_open = false,
            Ok(Msg::TooBig) => return Err(Fail::TooBig),
            Ok(Msg::End) | Err(RecvTimeoutError::Disconnected) => {
                // Usually it is ending by itself: let it, rather than end it in the middle of that.
                helper.borrow_mut().wait_end(TERM_GRACE);
                return Err(Fail::Ended);
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if (limits.cancel)() {
            return Err(Fail::Cancelled);
        }
        if alive.elapsed() >= limits.silence {
            return Err(Fail::Timeout);
        }
    }
}

/// `Kind::Program` has no HTTP side. The engine never asks for one; this stands where the other kinds have
/// their adapter and answers that nothing can be read, should anything ever ask.
pub struct NoHttp;

fn no_http() -> ApiError {
    ApiError::new(ErrKind::Protocol, "a provider of kind program is not spoken to over HTTP")
}

struct NoParser;

impl Parser for NoParser {
    fn event(&mut self, _event: &str, _data: &str, _out: &mut dyn FnMut(super::Event)) -> Result<(), ApiError> {
        Err(no_http())
    }
    fn done(&self) -> bool {
        true
    }
    fn finish(self: Box<Self>) -> Result<Turn, ApiError> {
        Err(no_http())
    }
}

impl Adapter for NoHttp {
    fn request(&self, _p: &Provider, _conv: &Conversation, _tools: &[ToolSpec]) -> HttpRequest {
        HttpRequest { path: String::new(), headers: Vec::new(), body: String::new() }
    }
    fn parser(&self, _p: &Provider) -> Box<dyn Parser> {
        Box::new(NoParser)
    }
    fn error(&self, _status: u16, _body: &str) -> ApiError {
        no_http()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_the_full_path_of_a_program() {
        let dir = std::env::temp_dir();
        assert!(check("helper").unwrap_err().contains("voller Pfad"));
        assert!(check("./helper").is_err() && check("").is_err());
        assert!(check("/no/such/helper-of-bitsos").unwrap_err().contains("gibt es nicht"));
        assert!(check(dir.to_str().unwrap()).unwrap_err().contains("keine Datei"));
        assert!(check("/proc/self/exe").is_ok());
        assert!(check("/etc/hostname").or_else(|_| check("/etc/passwd")).unwrap_err().contains("nicht als Programm"));
    }
}
