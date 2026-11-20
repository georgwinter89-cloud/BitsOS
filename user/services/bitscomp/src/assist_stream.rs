//! CL-3b: the lines of a running answer (`ev`: start, notice, delta, tool, done, error) into the model: the
//! list grows, the ring's phase follows, a tool's question becomes the card. What the socket's other end sends
//! is bounded here (`PIECE`, `LIST_MAX`): a long answer goes on item by item, one that does not fit the list
//! ends, cancelled, with a sentence.

use std::time::Duration;

use mcpreg::Json;

use super::{Card, ITEM_COST, Item, Model, PIECE, Phase, ToolState, Who, cut, line, piece, text_of};

/// PLAN K3/K6: the engine's tool that moves files to the trash; it asks at every level (F4).
pub const DELETE_TOOL: &str = "loeschen";
/// Board 5: the line above a delete question.
pub const DELETE_TITLE: &str = "Löschen fragt immer";
/// After a delete: where it went (board 5's sentence) and the way to look.
pub const TRASH_NOTE: &str = "Im Papierkorb, bis du ihn leerst.";

impl Model {
    pub(super) fn event(&mut self, ev: &str, j: &Json, now: Duration) -> Vec<String> {
        let id = j.get("id").and_then(Json::as_u64);
        // EINS-B: the App hands this model only its chat's lines. One that runs without this view's question
        // (the chat came from the popup, or this view opened on it) is shown as it comes.
        let asks = ev == "tool" && text_of(j, "state") == "asks";
        // Only a beginning (or a question that needs Georg): a late line of an answer that is over stays over.
        if self.ask.is_none() && (ev == "start" || asks) && self.pending.is_none() {
            self.ask = Some(super::Ask { id: id.unwrap_or(0), asked: now, last: now, started: true, turn: None, tool: None, remote: j.get("by").and_then(Json::as_str) == Some("remote"), cancelling: None });
            self.phase = Phase::Thinking;
        }
        let Some(a) = self.ask.as_mut().filter(|a| id.is_none() || a.id == 0 || Some(a.id) == id) else {
            // A line of an answer that is over here (cancelled, late): not shown.
            return Vec::new();
        };
        (a.last, a.started) = (now, true);
        match ev {
            "start" => {}
            "notice" => self.notice = Some(cut(text_of(j, "text"), 200).to_string()).filter(|t| !t.is_empty()),
            "delta" => {
                let mut t = text_of(j, "text");
                if !self.room(t.len() + (t.len() / PIECE + 1) * ITEM_COST) {
                    return self.overflow(now);
                }
                // The item that grows takes text up to `PIECE`; what is more goes on in a new item.
                while !t.is_empty() {
                    let open = self.ask.as_ref().and_then(|a| a.turn).filter(|i| self.items.get(*i).is_some_and(|it| it.text().len() < PIECE));
                    let i = open.unwrap_or_else(|| self.push(Item::Turn { who: Who::Assistant, text: String::new() }));
                    let had = self.items.get(i).map_or(0, |it| it.text().len());
                    let (take, rest) = piece(t, PIECE.saturating_sub(had));
                    if let Some(Item::Turn { text, .. }) = self.items.get_mut(i) {
                        text.push_str(take);
                        self.bytes += take.len();
                    }
                    self.touch(i);
                    if let Some(a) = self.ask.as_mut() {
                        a.turn = rest.is_empty().then_some(i);
                    }
                    t = rest;
                }
                self.phase = Phase::Answering;
            }
            "tool" => return self.tool_event(j, now),
            "done" => {
                self.end_ask(None);
                return self.refresh(now);
            }
            "error" => {
                let m = match text_of(j, "message") {
                    "" => "Fehler ohne Text".to_string(),
                    m => cut(m, 2000).to_string(),
                };
                let error = m != "abgebrochen";
                self.end_ask(Some((m, error)));
                return self.refresh(now);
            }
            other => self.events.push(format!("assist: unknown event {other:?}")),
        }
        Vec::new()
    }

    /// More than the list holds: the answer ends here and is cancelled at the assistant.
    fn overflow(&mut self, now: Duration) -> Vec<String> {
        let id = self.ask.as_ref().map_or(0, |a| a.id);
        self.events.push(format!("assist: ask {id}: the answer is larger than the list holds: cancelled"));
        self.end_ask(Some(("Die Antwort ist zu lang für die Liste. Beginne ein neues Gespräch.".into(), true)));
        let chat = self.chat.clone().unwrap_or_default();
        let mut out = vec![line(vec![("op", Json::str("cancel")), ("chat", Json::Str(chat)), ("id", Json::Num(id as f64))])];
        out.extend(self.refresh(now));
        out
    }

    fn tool_event(&mut self, j: &Json, now: Duration) -> Vec<String> {
        // A tool's line is one sentence; its name is for the log.
        // K1 calls the tool's sentence `line` (the first protocol `text`).
        let said = match text_of(j, "line") {
            "" => text_of(j, "text"),
            l => l,
        };
        let (name, text) = (cut(text_of(j, "name"), 200).to_string(), cut(said, 2000).to_string());
        let state = match text_of(j, "state") {
            "asks" => ToolState::Asks,
            "runs" => ToolState::Runs,
            "done" => ToolState::Done,
            "refused" => ToolState::Refused,
            other => {
                self.events.push(format!("assist: unknown tool state {other:?}"));
                return Vec::new();
            }
        };
        let deleted = state == ToolState::Done && name == DELETE_TOOL;
        let title = (name == DELETE_TOOL).then(|| DELETE_TITLE.to_string());
        let open = self.ask.as_ref().and_then(|a| a.tool).filter(|i| matches!(self.items.get(*i), Some(Item::Tool { name: n, .. }) if *n == name));
        let i = match open {
            Some(i) => {
                if let Some(Item::Tool { text: t, state: s, .. }) = self.items.get_mut(i) {
                    // A closing line without words keeps the tool's own.
                    if !text.is_empty() {
                        *t = text.clone();
                    }
                    *s = state;
                }
                self.touch(i);
                i
            }
            None if !self.room(text.len()) => return self.overflow(now),
            None => self.push(Item::Tool { name, text: text.clone(), state }),
        };
        let last = matches!(state, ToolState::Done | ToolState::Refused);
        if deleted {
            // F4: what was deleted lies in the trash; the way to its listing stays above the input.
            self.trash = Some(TRASH_NOTE.into());
        }
        let mut remote = false;
        if let Some(a) = self.ask.as_mut() {
            // Text after a tool is a new paragraph of the list.
            (a.tool, a.turn) = ((!last).then_some(i), None);
            remote = a.remote;
        }
        match state {
            ToolState::Asks => {
                let ask_id = j.get("ask_id").and_then(Json::as_u64).unwrap_or(0);
                self.card = Some(Card { ask_id, of: super::CardFor::Tool, title, text, yes_picked: true, note: None, remote, since: now, armed: false });
                (self.phase, self.tool) = (Phase::Waiting, None);
            }
            ToolState::Runs => (self.phase, self.tool, self.card) = (Phase::Working, Some(text), None),
            _ => (self.phase, self.tool, self.card) = (Phase::Thinking, None, None),
        }
        Vec::new()
    }

    /// The running answer is over; `note` goes into the list.
    pub(super) fn end_ask(&mut self, note: Option<(String, bool)>) {
        // A question that waited for its chat is over too.
        self.pending = None;
        if let Some(i) = self.ask.take().and_then(|a| a.tool) {
            if let Some(Item::Tool { state, .. }) = self.items.get_mut(i) {
                *state = ToolState::Stopped;
            }
            self.touch(i);
        }
        (self.phase, self.notice, self.tool, self.card) = (Phase::Idle, None, None, None);
        if let Some((text, error)) = note {
            self.push(Item::Note { text, error });
        }
    }
}
