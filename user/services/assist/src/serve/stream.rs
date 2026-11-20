//! The screen of a question asked over the socket: what the engine shows becomes event lines for the asker,
//! each naming its chat and question (K1). Text is gathered and sent at most once per [`DELTA_GAP`]; whatever
//! else is sent takes the waiting text with it first, so the order on the wire is the order things happened. A
//! tool question waits here for `answer`, with a deadline, and counts as no in every case but a yes that came in
//! time. How each tool call ended is also written into the chat's file.

use super::chats::Record;
use super::events::{Level, q};
use super::out::Out;
use super::status::Look;
use super::{Ctx, proto};
use crate::engine::{Answer, ToolState, Ui};
use crate::json::Json;
use crate::provider::Place;
use crate::term::wrap;
use crate::tools::permit::By;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

pub const DELTA_GAP: Duration = Duration::from_millis(50);
/// How often a waiting tool question looks whether the question was stopped.
const TICK: Duration = Duration::from_millis(50);

pub struct Stream<'a> {
    ctx: &'a Ctx,
    out: Arc<Out>,
    chat: String,
    id: i64,
    cancel: Arc<AtomicBool>,
    /// Who asked: the view's card of a tool question says when it was not Georg's hand.
    by: By,
    /// Text of the answer not sent yet.
    waiting: String,
    sent: Option<Instant>,
    /// Cloud providers Georg was told about for this question.
    told: Vec<String>,
    /// The last tool question got no answer in time (said when the engine reports the call as not run).
    unanswered: bool,
}

impl<'a> Stream<'a> {
    pub fn new(ctx: &'a Ctx, out: Arc<Out>, chat: &str, id: i64, by: By, cancel: Arc<AtomicBool>) -> Stream<'a> {
        Stream { ctx, out, chat: chat.to_string(), id, cancel, by, waiting: String::new(), sent: None, told: Vec::new(), unanswered: false }
    }

    fn ev(&self, name: &str) -> Json {
        proto::ev(name, self.id).with("chat", self.chat.as_str())
    }

    /// An event of this question as it is (the desktop call goes out this way).
    pub fn event(&mut self, line: Json) {
        self.send(line);
    }

    pub fn line(&self, name: &str) -> Json {
        self.ev(name)
    }

    fn flush(&mut self) {
        if !self.waiting.is_empty() {
            let text = std::mem::take(&mut self.waiting);
            self.out.line(&self.ev("delta").with("text", text));
            self.sent = Some(Instant::now());
        }
    }

    fn send(&mut self, line: Json) {
        self.flush();
        self.out.line(&line);
    }

    pub fn start(&mut self, look: &Look) {
        let start = self.ev("start").with("provider", look.provider.clone()).with("place", look.place_name());
        self.send(start);
    }

    /// The one line that ends the question: `done`, or `error` with its German message.
    pub fn end(&mut self, error: Option<&str>) {
        let line = match error {
            None => self.ev("done"),
            Some(m) => self.ev("error").with("message", wrap::clean(m)),
        };
        self.send(line);
    }

    /// K1 calls the words `line`; `text` stays for a view of before K1.
    fn tool_line(&mut self, name: &str, state: &str, text: &str) -> Json {
        let t = wrap::clean(text);
        self.ev("tool").with("name", name).with("state", state).with("line", t.as_str()).with("text", t)
    }
}

impl Ui for Stream<'_> {
    fn text(&mut self, delta: &str) {
        self.waiting.push_str(&wrap::clean(delta));
        if self.sent.is_none_or(|t| t.elapsed() >= DELTA_GAP) {
            self.flush();
        }
    }

    fn note(&mut self, line: &str) {
        let n = self.ev("notice").with("text", wrap::clean(line));
        self.send(n);
    }

    /// The provider's technical lines are for the journal, not for the sidebar. The engine took the key out.
    fn log(&mut self, line: &str) {
        self.ctx.parts.events.emit(Level::Info, "ask.log", &format!("chat={} id={} line={}", self.chat, self.id, q(&wrap::inline(line, 300))));
    }

    fn ask(&mut self, question: &str) -> Answer {
        self.ask_tool("", question)
    }

    /// Also the moment text that waited through a pause of the stream goes out: the engine asks this while
    /// it waits for the provider.
    fn cancelled(&mut self) -> bool {
        if self.sent.is_none_or(|t| t.elapsed() >= DELTA_GAP) {
            self.flush();
        }
        // A client that is gone reads no answer: the question ends as if it had been stopped.
        if self.out.dead() {
            self.cancel.store(true, Ordering::SeqCst);
        }
        self.cancel.load(Ordering::SeqCst)
    }

    fn ask_tool(&mut self, name: &str, question: &str) -> Answer {
        self.ask_tool_while(name, question, &|| true)
    }

    fn ask_tool_while(&mut self, name: &str, question: &str, wanted: &dyn Fn() -> bool) -> Answer {
        let wait = self.ctx.parts.question_wait;
        let (tx, rx) = channel();
        let n = self.ctx.state.question(&self.chat, tx);
        let line = self.tool_line(name, "asks", question).with("ask_id", n).with("wait_s", wait.as_secs()).with("by", self.by.name());
        self.send(line);
        let until = Instant::now() + wait;
        let mut answer = loop {
            // Whoever wanted the answer is gone (a helper program that ended): the card is taken back.
            if self.cancelled() || !wanted() {
                break None;
            }
            match rx.recv_timeout(TICK.min(until.saturating_duration_since(Instant::now()))) {
                Ok(yes) => break Some(yes),
                Err(RecvTimeoutError::Timeout) if Instant::now() >= until => {
                    self.unanswered = true;
                    break None;
                }
                Err(RecvTimeoutError::Timeout) => {}
                // A new conversation or a client that left took the question away.
                Err(RecvTimeoutError::Disconnected) => break None,
            }
        };
        self.ctx.state.withdraw(n);
        // An answer that was taken in the last moment before the question was withdrawn still counts.
        if answer.is_none()
            && !self.cancel.load(Ordering::SeqCst)
            && let Ok(yes) = rx.try_recv()
        {
            (answer, self.unanswered) = (Some(yes), false);
        }
        let said = match answer {
            Some(true) => "yes",
            Some(false) => "no",
            None => "none",
        };
        self.ctx.parts.events.emit(Level::Info, "question", &format!("chat={} id={} ask_id={n} tool={} answer={said}", self.chat, self.id, q(name)));
        match answer {
            Some(true) => Answer::Yes,
            None if self.unanswered => Answer::Unanswered,
            _ => Answer::No,
        }
    }

    fn tool(&mut self, name: &str, state: ToolState, text: &str) {
        let line = match state {
            ToolState::Runs | ToolState::Confirmed => self.tool_line(name, "runs", text),
            ToolState::Done => self.tool_line(name, "done", text),
            ToolState::Failed => self.tool_line(name, "done", &format!("Hat nicht geklappt: {text}")).with("failed", true),
            ToolState::Refused => self.tool_line(name, "refused", text),
            ToolState::Declined if std::mem::take(&mut self.unanswered) => {
                let secs = self.ctx.parts.question_wait.as_secs();
                self.tool_line(name, "refused", &format!("Keine Antwort in {secs} Sekunden, deshalb nicht ausgeführt: {text}"))
            }
            ToolState::Declined => self.tool_line(name, "refused", &format!("Nicht ausgeführt: {text}")),
        };
        // What the chat keeps of it: how the call ended.
        if let Some(state) = line.str("state").filter(|s| matches!(*s, "done" | "refused") && self.ctx.state.meta(&self.chat).is_some()) {
            let r = Record::Tool { name: name.to_string(), state: state.to_string(), line: line.str("line").unwrap_or("").to_string() };
            if let Err(e) = self.ctx.store.add(&self.chat, &r) {
                self.ctx.parts.events.emit(Level::Warn, "chat.write", &format!("chat={} reason={}", self.chat, q(&e)));
            }
        }
        self.send(line);
    }

    fn provider(&mut self, id: &str, _place: Place, notice: bool) {
        if notice && !self.told.iter().any(|t| t == id) {
            self.told.push(id.to_string());
            let n = self.ev("notice").with("text", format!("geht an {id}"));
            self.send(n);
        }
    }
}
