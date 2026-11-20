//! The desktop tools of a question (K3) as the service runs them: the call goes to the view that asked, as the
//! event `desk{chat, id, call_id, name, args, level, by}`, and the view's `desk_result{call_id, content,
//! is_error?, image?}` answers it. A view that went away, a question that was stopped and a view that does not
//! answer in time all give an error result: the question never hangs on the desktop.

use super::events::{Level as Ev, q};
use super::out::Out;
use super::{Ctx, proto};
use crate::json::Json;
use crate::tools::desk::{ACT, Desk, DeskAnswer};
use crate::tools::permit::{By, Level};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(50);

pub struct ServeDesk<'a> {
    pub ctx: &'a Ctx,
    pub out: Arc<Out>,
    pub chat: String,
    pub id: i64,
    pub by: By,
    pub cancel: Arc<AtomicBool>,
}

fn failed(content: &str) -> DeskAnswer {
    DeskAnswer { content: content.into(), is_error: true, image: None }
}

impl Desk for ServeDesk<'_> {
    fn run(&self, name: &str, args: &Json, level: Level, stop: &dyn Fn() -> bool) -> DeskAnswer {
        let (tx, rx) = channel();
        let n = self.ctx.state.call(tx);
        let line = proto::ev("desk", self.id)
            .with("chat", self.chat.as_str())
            .with("call_id", n)
            .with("name", name)
            .with("args", args.clone())
            .with("level", level.name())
            .with("by", self.by.name());
        if !self.out.line(&line) {
            self.ctx.state.call_done(n);
            return failed("the desktop is not there (the view that asked went away); nothing ran");
        }
        // A step may wait for Georg's yes at the desktop first.
        let wait = if name == ACT { self.ctx.parts.question_wait + self.ctx.parts.desk_wait } else { self.ctx.parts.desk_wait };
        let until = Instant::now() + wait;
        let answer = loop {
            if stop() || self.cancel.load(Ordering::SeqCst) {
                break failed("not run to its end: Georg stopped the question");
            }
            if self.out.dead() {
                break failed("the desktop went away during the call");
            }
            match rx.recv_timeout(TICK.min(until.saturating_duration_since(Instant::now()))) {
                Ok(a) => break a,
                Err(RecvTimeoutError::Timeout) if Instant::now() >= until => break failed(&format!("the desktop did not answer within {} s", wait.as_secs())),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break failed("the desktop went away during the call"),
            }
        };
        self.ctx.state.call_done(n);
        self.ctx.parts.events.emit(Ev::Info, "desk", &format!("chat={} id={} call_id={n} name={} error={}", self.chat, self.id, q(name), answer.is_error));
        answer
    }
}
