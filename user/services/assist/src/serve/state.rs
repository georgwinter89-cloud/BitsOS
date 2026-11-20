//! What the client threads and the workers share: the chats (each with its conversation while no question
//! works on it), the questions at work (at most [`RUNNING_MAX`] at once, one per chat), the tool questions that
//! wait for Georg and the desktop calls that wait for the view. Every method takes the lock for a few
//! instructions and returns; nothing waits while holding it.

use super::chats::Meta;
use crate::conv::Conversation;
use crate::tools::desk::DeskAnswer;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// How many questions work at once (K1: one worker per chat, at most 3).
pub const RUNNING_MAX: usize = 3;

struct Running {
    id: i64,
    /// The client that asked (its number from `accept`).
    client: u64,
    cancel: Arc<AtomicBool>,
}

struct Chat {
    meta: Meta,
    /// None while a question works on it, or before it was read from its file.
    conv: Option<Conversation>,
    running: Option<Running>,
}

#[derive(Default)]
struct Inner {
    chats: BTreeMap<String, Chat>,
    /// The chat a request without one means (the sidebar of before K1).
    default: Option<String>,
    questions: BTreeMap<u64, (String, Sender<bool>)>,
    asked: u64,
    calls: BTreeMap<u64, Sender<DeskAnswer>>,
    called: u64,
}

#[derive(Default)]
pub struct State(Mutex<Inner>);

/// A question a worker may start.
pub struct Begun {
    pub cancel: Arc<AtomicBool>,
    /// The chat's conversation, when it was in memory.
    pub conv: Option<Conversation>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NotBegun {
    NoChat,
    /// This chat is at work.
    Busy,
    /// [`RUNNING_MAX`] questions are at work.
    Full,
}

impl State {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn add(&self, meta: Meta) {
        let mut s = self.lock();
        s.chats.insert(meta.id.clone(), Chat { meta, conv: None, running: None });
    }

    /// The chats, the newest first.
    pub fn list(&self) -> Vec<Meta> {
        let mut v: Vec<Meta> = self.lock().chats.values().map(|c| c.meta.clone()).collect();
        v.sort_by(|a, b| b.updated.cmp(&a.updated).then(b.id.cmp(&a.id)));
        v
    }

    pub fn meta(&self, chat: &str) -> Option<Meta> {
        self.lock().chats.get(chat).map(|c| c.meta.clone())
    }

    pub fn update(&self, chat: &str, f: impl FnOnce(&mut Meta)) {
        if let Some(c) = self.lock().chats.get_mut(chat) {
            f(&mut c.meta);
        }
    }

    pub fn default_chat(&self) -> Option<String> {
        let s = self.lock();
        s.default.clone().filter(|d| s.chats.contains_key(d))
    }

    pub fn set_default(&self, chat: &str) {
        self.lock().default = Some(chat.to_string());
    }

    /// Takes question `id` of `client` into `chat`.
    pub fn begin(&self, chat: &str, id: i64, client: u64) -> Result<Begun, NotBegun> {
        let mut s = self.lock();
        let running = s.chats.values().filter(|c| c.running.is_some()).count();
        let c = s.chats.get_mut(chat).ok_or(NotBegun::NoChat)?;
        if c.running.is_some() {
            return Err(NotBegun::Busy);
        }
        if running >= RUNNING_MAX {
            return Err(NotBegun::Full);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        c.running = Some(Running { id, client, cancel: cancel.clone() });
        Ok(Begun { cancel, conv: c.conv.take() })
    }

    /// The worker is done with its question; `conv` is the chat's conversation after it.
    pub fn finished(&self, chat: &str, conv: Conversation) {
        let mut s = self.lock();
        if let Some(c) = s.chats.get_mut(chat) {
            c.running = None;
            c.conv = Some(conv);
        }
        s.questions.retain(|_, (q, _)| q != chat);
    }

    /// Stops the question at work in `chat`, or the one with number `id` anywhere. Some: (chat, id) stopped.
    pub fn cancel(&self, chat: Option<&str>, id: Option<i64>) -> Option<(String, i64)> {
        let s = self.lock();
        let (name, c) = s.chats.iter().find(|(name, c)| c.running.as_ref().is_some_and(|r| chat.is_none_or(|w| w == name.as_str()) && id.is_none_or(|i| i == r.id)))?;
        let r = c.running.as_ref()?;
        r.cancel.store(true, Ordering::SeqCst);
        Some((name.clone(), r.id))
    }

    /// `client` is gone: nobody reads the answers to its questions, and nobody answers for them.
    pub fn left(&self, client: u64) -> Vec<(String, i64)> {
        let mut s = self.lock();
        let mut out = Vec::new();
        for (name, c) in &s.chats {
            if let Some(r) = c.running.as_ref().filter(|r| r.client == client) {
                r.cancel.store(true, Ordering::SeqCst);
                out.push((name.clone(), r.id));
            }
        }
        s.questions.retain(|_, (q, _)| !out.iter().any(|(c, _)| c == q));
        out
    }

    /// Takes `chat` out of the list. A question at work in it is stopped. False: no such chat.
    pub fn remove(&self, chat: &str) -> bool {
        let mut s = self.lock();
        let Some(c) = s.chats.remove(chat) else { return false };
        if let Some(r) = c.running {
            r.cancel.store(true, Ordering::SeqCst);
        }
        s.questions.retain(|_, (q, _)| q != chat);
        if s.default.as_deref() == Some(chat) {
            s.default = None;
        }
        true
    }

    /// The worker of `chat` asks Georg: the number the answer must name.
    pub fn question(&self, chat: &str, answer: Sender<bool>) -> u64 {
        let mut s = self.lock();
        s.asked += 1;
        let n = s.asked;
        s.questions.insert(n, (chat.to_string(), answer));
        n
    }

    /// The worker waits no longer for question `n`: an answer that comes now finds nothing.
    pub fn withdraw(&self, n: u64) {
        self.lock().questions.remove(&n);
    }

    /// Georg's answer to question `n`. False: no such question waits (answered, timed out, stopped).
    pub fn answer(&self, n: u64, yes: bool) -> bool {
        match self.lock().questions.remove(&n) {
            Some((_, tx)) => tx.send(yes).is_ok(),
            None => false,
        }
    }

    /// A desktop call goes out: the number its result must name.
    pub fn call(&self, result: Sender<DeskAnswer>) -> u64 {
        let mut s = self.lock();
        s.called += 1;
        let n = s.called;
        s.calls.insert(n, result);
        n
    }

    pub fn call_done(&self, n: u64) {
        self.lock().calls.remove(&n);
    }

    /// The view's result of call `n`. False: no such call waits.
    pub fn desk_result(&self, n: u64, a: DeskAnswer) -> bool {
        match self.lock().calls.remove(&n) {
            Some(tx) => tx.send(a).is_ok(),
            None => false,
        }
    }

    /// (questions at work, chats)
    pub fn view(&self) -> (usize, usize) {
        let s = self.lock();
        (s.chats.values().filter(|c| c.running.is_some()).count(), s.chats.len())
    }

    pub fn running(&self, chat: &str) -> bool {
        self.lock().chats.get(chat).is_some_and(|c| c.running.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Origin;
    use std::sync::mpsc::channel;

    fn meta(id: &str, updated: u64) -> Meta {
        Meta { id: id.into(), origin: Origin::Sidebar, app: None, title: id.into(), named: true, created: 0, updated, turns: 0 }
    }

    #[test]
    fn one_question_per_chat_three_at_once_and_answers_only_for_what_waits() {
        let s = State::default();
        for (i, c) in ["a", "b", "c", "d"].iter().enumerate() {
            s.add(meta(c, i as u64));
        }
        assert_eq!(s.list().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["d", "c", "b", "a"]);
        let a = s.begin("a", 1, 7).unwrap();
        assert_eq!(s.begin("a", 2, 7).err(), Some(NotBegun::Busy));
        s.begin("b", 3, 7).unwrap();
        s.begin("c", 4, 8).unwrap();
        assert_eq!(s.begin("d", 5, 8).err(), Some(NotBegun::Full));
        assert_eq!(s.begin("x", 6, 8).err(), Some(NotBegun::NoChat));
        let (tx, rx) = channel();
        let q = s.question("a", tx);
        assert!(!s.answer(q + 1, true) && rx.try_recv().is_err());
        assert!(s.answer(q, true) && rx.try_recv() == Ok(true));
        assert!(!s.answer(q, true), "once");
        assert_eq!(s.cancel(Some("a"), None), Some(("a".into(), 1)));
        assert!(a.cancel.load(Ordering::SeqCst));
        assert_eq!(s.cancel(None, Some(4)), Some(("c".into(), 4)));
        assert_eq!(s.cancel(Some("d"), None), None);
        assert_eq!(s.left(7).len(), 2, "the client's two questions");
        s.finished("a", Conversation::default());
        assert!(s.begin("d", 5, 8).is_ok(), "a place came free");
        assert!(s.remove("b") && !s.remove("b"));
        let (tx, rx) = channel();
        let n = s.call(tx);
        assert!(s.desk_result(n, DeskAnswer::default()) && rx.try_recv().is_ok() && !s.desk_result(n, DeskAnswer::default()));
    }
}
