//! EINS-B (PLAN K1): the one connection to `assist serve` carries the chats of both views. Requests go out in
//! order and the service answers them in order, so a `new` reply belongs to the oldest `new` still open (`news`).
//! Events carry their chat: they go to the popup when it shows that chat, to the sidebar when it does, `desk`
//! calls to the desktop's tools whatever shows them. A tool's question of a chat no view shows opens the popup on
//! it (the sidebar keeps what Georg had there). Every question's origin (`by`) is kept per chat: only a chat whose
//! question came from Georg's hand may act unasked.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use mcpreg::Json;

use crate::state::App;

/// Who asked for a chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewFor {
    /// The popup of this generation.
    Popup(u64),
    Sidebar,
}

/// A picture a desktop tool waits for: its call, the rectangle (physical px), the text that goes with it.
#[derive(Clone, Debug)]
pub struct Shot {
    pub call: String,
    pub rect: crate::layout::Rect,
    pub text: String,
    pub asked: Instant,
    /// Its pixels were taken; the PNG is on its way.
    pub taken: bool,
}

#[derive(Default)]
pub struct Route {
    news: VecDeque<NewFor>,
    /// Per chat: whether the question that runs there now came from Georg's own hand.
    pub hand: HashMap<String, bool>,
    /// Pictures the desktop tools wait for (`db_shot::take`).
    pub shots: Vec<Shot>,
    /// Settings the views loosened and settingsd has not taken yet: (key, the value before). A refusal puts the
    /// value back, so the gate never runs at more freedom than is stored.
    pub loosened: Vec<(String, String)>,
}

/// How long a picture waits for the next frame at most.
pub const SHOT_WAIT: Duration = Duration::from_millis(500);

fn op_of(line: &str) -> Option<Json> {
    Json::parse(line).ok().filter(|j| j.get("op").is_some())
}

impl Route {
    /// What one line to the service means for the routing: a `new` waits for its reply, an `ask` says its origin.
    pub fn sent(&mut self, line: &str, from: NewFor) {
        let Some(j) = op_of(line) else { return };
        match j.get("op").and_then(Json::as_str) {
            Some("new") => self.news.push_back(from),
            Some("ask") => {
                if let Some(chat) = j.get("chat").and_then(Json::as_str) {
                    self.hand.insert(chat.to_string(), j.get("by").and_then(Json::as_str) == Some("hand"));
                }
            }
            _ => {}
        }
    }

    /// The reply to the oldest open `new`.
    pub fn made(&mut self) -> Option<NewFor> {
        self.news.pop_front()
    }

    /// The connection went: no reply comes for what was open.
    pub fn reset(&mut self) {
        self.news.clear();
        self.hand.clear();
    }
}

/// A reply that names only a chat: the answer to `new`.
fn new_reply(j: &Json) -> Option<&str> {
    let only = j.get("turns").is_none() && j.get("chats").is_none() && j.get("ready").is_none() && j.get("ev").is_none();
    (j.get("ok").and_then(Json::as_bool) == Some(true) && only).then(|| j.get("chat").and_then(Json::as_str)).flatten()
}

impl App {
    /// Lines to the service, from a view; a line goes nowhere while the assistant is off.
    pub(crate) fn assist_out(&mut self, lines: Vec<String>, from: NewFor) {
        for l in lines {
            // A line while no connection is up goes nowhere: no reply will come for it.
            if !self.assist.up {
                eprintln!("bitscomp: assist: not connected, a request was not sent");
                continue;
            }
            self.assist_route.sent(&l, from);
            if let Some(c) = &self.assist_client {
                c.send(l);
            }
        }
    }

    /// A line of the service: its events to the view of their chat, its replies to who asked.
    pub(crate) fn assist_line_in(&mut self, j: &Json, now: Duration) -> Vec<String> {
        if let Some(ev) = j.get("ev").and_then(Json::as_str) {
            let chat = j.get("chat").and_then(Json::as_str).unwrap_or("").to_string();
            return self.assist_event(ev, &chat, j, now);
        }
        if let Some(chat) = new_reply(j).map(str::to_string) {
            return match self.assist_route.made() {
                Some(NewFor::Popup(g)) => {
                    self.db_chat_made(g, &chat);
                    Vec::new()
                }
                Some(NewFor::Sidebar) => self.assist.chat_made(&chat, now),
                None => {
                    eprintln!("bitscomp: assist: a chat {chat} nobody asked for");
                    Vec::new()
                }
            };
        }
        // A refusal while the popup waits for its chat: the popup hears it (the sidebar's ask is not started).
        if j.get("ok").and_then(Json::as_bool) == Some(false) && self.db.waits_for_chat() && !self.assist.waits_for_chat() {
            let _ = self.assist_route.made();
            self.db_refused(j);
            return Vec::new();
        }
        if j.get("ok").and_then(Json::as_bool) == Some(false) && self.assist.waits_for_chat() {
            let _ = self.assist_route.made();
        }
        self.assist.on(j, now)
    }

    fn assist_event(&mut self, ev: &str, chat: &str, j: &Json, now: Duration) -> Vec<String> {
        if ev == "desk" {
            self.desk_call(chat, j);
            return Vec::new();
        }
        let asks = ev == "tool" && j.get("state").and_then(Json::as_str) == Some("asks");
        let side = self.assist.chat.as_deref() == Some(chat) && self.assist_place().is_some();
        if asks && !side && !(self.db.open && self.db.chat.as_deref() == Some(chat)) {
            // Nobody shows the chat that asks Georg: the popup opens on it.
            self.db_open_on(chat, "a question of the assistant");
        }
        let popup = self.db.open && self.db.chat.as_deref() == Some(chat);
        if popup {
            self.db_event(ev, j);
        }
        let mut out = Vec::new();
        if self.assist.chat.as_deref() == Some(chat) {
            out = self.assist.on(j, now);
        }
        if matches!(ev, "done" | "error") {
            self.assist_route.hand.remove(chat);
            self.db_act_end(chat);
            // The list's titles and times follow the chats.
            out.push(crate::assist::list_line());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_reply_goes_to_who_asked_first_and_asks_say_their_hand() {
        let mut r = Route::default();
        r.sent(r#"{"op":"new","origin":"popup"}"#, NewFor::Popup(3));
        r.sent(r#"{"op":"new","origin":"sidebar"}"#, NewFor::Sidebar);
        r.sent(r#"{"op":"ask","chat":"c1","id":1,"text":"x","by":"hand"}"#, NewFor::Sidebar);
        r.sent(r#"{"op":"ask","chat":"c2","id":1,"text":"x","by":"remote"}"#, NewFor::Sidebar);
        r.sent("kein json", NewFor::Sidebar);
        assert_eq!((r.made(), r.made(), r.made()), (Some(NewFor::Popup(3)), Some(NewFor::Sidebar), None));
        assert_eq!((r.hand.get("c1"), r.hand.get("c2")), (Some(&true), Some(&false)));
        let j = |s: &str| Json::parse(s).unwrap();
        assert_eq!(new_reply(&j(r#"{"ok":true,"chat":"c9"}"#)), Some("c9"));
        assert_eq!(new_reply(&j(r#"{"ok":true,"chat":"c9","turns":[]}"#)), None, "a history is no new chat");
        assert_eq!(new_reply(&j(r#"{"ok":false,"chat":"c9"}"#)), None);
    }
}
