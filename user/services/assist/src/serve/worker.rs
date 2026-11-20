//! One question of one chat, on a thread of its own (K1: a worker per chat, at most three at once). It takes the
//! chat's conversation (from memory, else from its file), makes the tools (with the view's desktop tools), runs
//! the question through the engine the console chat uses, writes what is kept into the chat's file and gives the
//! conversation back. Every wait in here has the engine's deadlines and looks at the stop flag.
//!
//! A fault in here (a panic in the engine, a provider's adapter or a tool) must not leave the chat saying "busy"
//! for ever: the question ends with an error line and the chat goes on with what its file says.

use super::chats::{PICTURE_SENT, Record};
use super::desk::ServeDesk;
use super::events::{Level, q};
use super::out::Out;
use super::stream::Stream;
use super::{Ctx, Seat, proto, status};
use crate::conv::{Conversation, Image, Part};
use crate::engine::{Session, Ui};
use crate::settings::Settings;
use crate::tools::permit::By;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What Georg reads when the question ended in a fault of the assistant itself.
pub const FAULT: &str = "Im Assistenten ist ein Fehler aufgetreten, die Frage blieb ohne Antwort. Stell sie bitte noch einmal.";

/// The window a question carries (K1 `context`), as the model reads it and as the chat keeps it.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// The words before the question (window, list, extras); empty: none.
    pub text: String,
    pub image: Option<Image>,
}

/// One question, as a client thread hands it over (`State::begin` said yes to it).
pub struct Job {
    pub chat: String,
    pub id: i64,
    pub text: String,
    pub seen: Seen,
    pub by: By,
    pub out: Arc<Out>,
    pub cancel: Arc<AtomicBool>,
    pub conv: Option<Conversation>,
}

/// Starts the question's thread. False: no thread could be made (the chat is free again).
pub fn spawn(ctx: &Arc<Ctx>, job: Job) -> bool {
    let c = ctx.clone();
    let chat = job.chat.clone();
    let started = std::thread::Builder::new().name("assist-ask".into()).spawn(move || {
        let (chat, id, out) = (job.chat.clone(), job.id, job.out.clone());
        // `one` gives the chat back itself before its last line; only a fault leaves it to this.
        if catch_unwind(AssertUnwindSafe(|| one(&c, job))).is_err() {
            keep(&c, &chat, &Record::Failed(FAULT.into()));
            let conv = c.store.load(&chat).map(|l| l.conversation()).unwrap_or_default();
            c.state.finished(&chat, conv);
            c.parts.events.emit(Level::Error, "ask.end", &format!("chat={chat} id={id} outcome=fault"));
            out.line(&proto::ev("error", id).with("chat", chat.as_str()).with("message", FAULT));
        }
    });
    if started.is_err() {
        ctx.state.finished(&chat, ctx.store.load(&chat).map(|l| l.conversation()).unwrap_or_default());
    }
    started.is_ok()
}

/// Keeps the newest picture in the conversation and puts words in place of the older ones: a follow-up about
/// the same window still has it, and memory holds one picture per chat.
fn fewer_pictures(conv: &mut Conversation) {
    let mut seen_newest = false;
    for m in conv.messages.iter_mut().rev() {
        for p in m.parts.iter_mut().rev() {
            if matches!(p, Part::Image(_)) {
                if seen_newest {
                    *p = Part::Text(format!("\n{PICTURE_SENT}"));
                }
                seen_newest = true;
            }
        }
    }
}

fn keep(ctx: &Ctx, chat: &str, r: &Record) {
    // Removed while the question was at work: nothing of it is kept.
    if ctx.state.meta(chat).is_none() {
        return;
    }
    if let Err(e) = ctx.store.add(chat, r) {
        ctx.parts.events.emit(Level::Warn, "chat.write", &format!("chat={chat} reason={}", q(&e)));
    }
}

/// One question to its last line.
fn one(ctx: &Ctx, job: Job) {
    let settings: &dyn Settings = &*ctx.parts.settings;
    let meta = ctx.state.meta(&job.chat);
    let mut conv = job.conv.clone().or_else(|| ctx.store.load(&job.chat).ok().map(|l| l.conversation())).unwrap_or_default();
    let first = conv.messages.is_empty();
    keep(ctx, &job.chat, &Record::Georg { text: job.text.clone(), context: job.seen.text.clone(), picture: job.seen.image.is_some(), by: job.by.name().into() });
    ctx.state.update(&job.chat, |m| {
        m.turns += 1;
        m.updated = super::chats::now_ms();
        if !m.named && m.turns == 1 {
            m.title = super::chats::title_of(&job.text);
        }
    });
    let desk = ServeDesk { ctx, out: job.out.clone(), chat: job.chat.clone(), id: job.id, by: job.by, cancel: job.cancel.clone() };
    let tools = (ctx.parts.tools)(settings, Seat { chat: &job.chat, desk: Some(&desk) });
    let (note, log) = tools.greeting();
    if let Some(l) = log {
        ctx.parts.events.emit(Level::Info, "tools", &format!("line={}", q(&l)));
    }
    let mut ui = Stream::new(ctx, job.out.clone(), &job.chat, job.id, job.by, job.cancel.clone());
    let mut session = Session::new(settings, &*tools, (ctx.parts.budget)());
    session.origin = meta.map(|m| m.origin).unwrap_or_default();
    session.by = job.by;
    session.conv = std::mem::take(&mut conv);
    let look = status::look(settings);
    ctx.parts.events.emit(Level::Info, "ask", &format!("chat={} id={} by={} provider={} place={}", job.chat, job.id, job.by.name(), q(look.provider.as_deref().unwrap_or("-")), look.place_name().unwrap_or("-")));
    ui.start(&look);
    if let Some(g) = note.filter(|_| first) {
        ui.note(&g);
    }
    let mark = session.conv.messages.len();
    let asked_text = if job.seen.text.is_empty() { job.text.clone() } else { format!("{}\n\nGeorg's question: {}", job.seen.text, job.text) };
    let asked = match &look.why {
        Some(why) => Err(why.clone()),
        None => session.ask_with(&asked_text, job.seen.image.clone(), &mut ui).map(|_| ()),
    };
    let stopped = job.cancel.load(Ordering::SeqCst);
    let answer: String = session.conv.messages[mark.min(session.conv.messages.len())..]
        .iter()
        .filter(|m| m.role == crate::conv::Role::Assistant)
        .map(|m| m.text())
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let kept = session.conv.messages.len() > mark;
    let outcome = match &asked {
        Ok(()) if kept => {
            keep(ctx, &job.chat, &Record::Assistant(answer));
            "done"
        }
        Ok(()) => {
            keep(ctx, &job.chat, &Record::Failed("leer".into()));
            "done"
        }
        Err(_) if stopped => {
            keep(ctx, &job.chat, &Record::Failed(proto::CANCELLED.into()));
            "cancelled"
        }
        Err(why) => {
            keep(ctx, &job.chat, &Record::Failed(why.clone()));
            "error"
        }
    };
    let mut conv = std::mem::take(&mut session.conv);
    fewer_pictures(&mut conv);
    ctx.state.update(&job.chat, |m| m.updated = super::chats::now_ms());
    // Before the last line goes out: a client that asks again the moment it reads `done` is not told "busy".
    ctx.state.finished(&job.chat, conv);
    match asked {
        Ok(()) => ui.end(None),
        Err(_) if stopped => ui.end(Some(proto::CANCELLED)),
        Err(why) => ui.end(Some(&why)),
    }
    ctx.parts.events.emit(Level::Info, "ask.end", &format!("chat={} id={} outcome={outcome}", job.chat, job.id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conv::Message;

    #[test]
    fn only_the_newest_picture_stays_in_memory() {
        let pic = || Image { media_type: "image/png".into(), base64: "iVBO".into() };
        let mut c = Conversation::default();
        c.messages.push(Message::user_with("eins", Some(pic())));
        c.messages.push(Message::user_with("zwei", Some(pic())));
        fewer_pictures(&mut c);
        assert!(!c.messages[0].parts.iter().any(|p| matches!(p, Part::Image(_))) && c.messages[0].text().contains(PICTURE_SENT));
        assert!(c.messages[1].parts.iter().any(|p| matches!(p, Part::Image(_))));
    }
}
