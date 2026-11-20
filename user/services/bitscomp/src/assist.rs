//! CL-3b: the conversation with the assistant as the desktop shows it. Pure: lines of `assist serve`'s socket
//! (/run/user/<uid>/bitsos-assist.sock, one JSON object per line; the protocol is in plan/spec/assistant.md §3) and
//! Georg's input come in, request lines and the state come out (the list, the ring, the question card). The
//! socket is `assist_client`'s, the glue `assist_app`'s; the main loop only draws from this model (B4). Every
//! request has a deadline here (`due`, `tick`): a silent assistant is "<name> antwortet nicht", never a wait.
//! The stream's events are in `assist_stream`, a tool's question and the rules of its answer in `assist_card`.

use std::time::Duration;

use mcpreg::Json;

use crate::address::Edit;

#[path = "assist_card.rs"]
mod card;
#[path = "assist_chats.rs"]
mod chats;
#[path = "assist_stream.rs"]
mod stream;
#[path = "assist_types.rs"]
mod types;

pub use card::{ARM, Card, CardFor, Hand, REMOTE_ASK, REMOTE_CARD};
pub use chats::{ChatRow, Origin as ChatOrigin, list_line, new_line, remove_line};
pub use stream::{DELETE_TITLE, DELETE_TOOL};
pub use types::{Gate, Item, Phase, Status, ToolState, Who};

/// A reply to `status` or `history`.
pub const STATUS_WAIT: Duration = Duration::from_secs(3);
/// From an `ask` to its `start` line.
pub const ASK_START: Duration = Duration::from_secs(10);
/// Between two lines of a running answer (a slow model thinks, a tool runs up to a minute).
pub const ASK_IDLE: Duration = Duration::from_secs(180);
/// How long the socket may be missing before the sidebar says so (the helper is started with the session).
pub const LINK_GRACE: Duration = Duration::from_secs(5);
/// While the assistant is busy with an answer this sidebar did not ask for, and after a missed reply.
pub const POLL: Duration = Duration::from_secs(2);
/// While the assistant cannot reach the settings (it started before settingsd at boot), the status is asked again
/// after this, doubling up to `RETRY_MAX`: nothing else would ask before Georg touches the sidebar.
pub const RETRY_FIRST: Duration = Duration::from_millis(500);
pub const RETRY_MAX: Duration = Duration::from_secs(8);
/// assist's sentence for "settings not reachable" (serve/status.rs), for an assist that does not send
/// `settings_down` yet.
const UNREACHABLE: &str = "Die Einstellungen sind gerade nicht erreichbar.";

/// Where a provider is chosen.
pub const SETTINGS: &str = "einstellungen/ki";

/// What the compositor holds and wraps on its main loop, not limits of the conversation (that is the
/// assistant's): whoever answers on the socket must not be able to fill bitscomp's memory or stall its frames.
/// The list shows at most `LIST_MAX` bytes (the newest; a longer conversation goes on in a new one). An item
/// of the list is at most `PIECE` long: a longer text goes on in the next item, so a growing answer wraps
/// one piece again per line that arrives, never all of it (measured: 64 KiB in about 5 ms, 1 MiB in 80 ms).
/// The input line takes `INPUT_MAX` (it is wrapped at every look: 32 KiB in about 3 ms).
pub const PIECE: usize = 64 << 10;
pub const LIST_MAX: usize = 4 << 20;
pub const INPUT_MAX: usize = 32 << 10;
/// What an item costs besides its text.
const ITEM_COST: usize = 64;

#[derive(Clone, Debug, PartialEq)]
struct Ask {
    id: u64,
    asked: Duration,
    last: Duration,
    started: bool,
    /// The item the answer's text grows in, and the open tool's.
    turn: Option<usize>,
    tool: Option<usize>,
    /// Injected input asked (MCP, `bitscomp ctl`): its questions to Georg say so.
    remote: bool,
    /// Georg cancelled: by when the assistant's "abgebrochen" must have come.
    cancelling: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Status,
    History,
}

pub struct Model {
    /// EINS-B (PLAN K1): the chat this sidebar shows (None: a new one is made with the first question).
    pub chat: Option<String>,
    /// A question waiting for the service's `new` reply: its text, whether injected input asked, its context.
    pending: Option<(String, bool, Option<Json>)>,
    /// The service's chats, newest first (`assist_chats`).
    pub chats: Vec<ChatRow>,
    /// The level pill's three choices are open.
    pub level_open: bool,
    /// Georg chose "+ Neuer Chat": the newest chat is not opened for him.
    fresh: bool,
    /// A card that is not the engine's (a step of `handeln`, removing a chat) was answered: what and how.
    pub decided: Option<(CardFor, bool)>,
    /// After a delete (board 5): the line above the input with the way to the trash's listing.
    pub trash: Option<String>,
    /// The assistant's name: the status' once it came, the setting's before.
    pub name: String,
    pub up: bool,
    /// The socket answered once this session (the Bühne shows the conversation only then).
    pub ever_up: bool,
    down_since: Option<Duration>,
    silent: bool,
    stranger: bool,
    pub status: Option<Status>,
    pub items: Vec<Item>,
    /// The lowest item changed since the drawing last looked (`assist_layout::Wraps`).
    pub stale_from: usize,
    ask: Option<Ask>,
    pub phase: Phase,
    /// "geht an <Anbieter>" while the answer runs.
    pub notice: Option<String>,
    /// The running tool's line.
    pub tool: Option<String>,
    pub card: Option<Card>,
    /// A line above the input (text, is an error): why nothing was sent.
    pub note: Option<(String, bool)>,
    pub edit: Edit,
    pub focus: bool,
    /// Georg's own click gave the input line the keys; only then his Enter can say yes to a question.
    pub focus_physical: bool,
    /// Injected input wrote into the input line: what is sent from it is marked as asked from outside.
    pub typed_remote: bool,
    /// The list's size as counted against `LIST_MAX`.
    bytes: usize,
    /// Logical pixels the list is scrolled up from its end.
    pub scroll: i32,
    next_id: u64,
    waits: Vec<(Wait, Duration)>,
    poll_at: Option<Duration>,
    /// The next wait before the status is asked again while the settings are not reachable.
    retry: Option<Duration>,
    /// Lines for bitscomp's log (B6).
    pub events: Vec<String>,
}

impl Model {
    pub fn new(name: &str) -> Model {
        Model {
            chat: None,
            pending: None,
            chats: Vec::new(),
            level_open: false,
            fresh: false,
            decided: None,
            trash: None,
            name: name.to_string(),
            up: false,
            ever_up: false,
            down_since: None,
            silent: false,
            stranger: false,
            status: None,
            items: Vec::new(),
            stale_from: 0,
            ask: None,
            phase: Phase::Idle,
            notice: None,
            tool: None,
            card: None,
            note: None,
            edit: Edit { text: String::new(), cursor: 0, pick: None, tile: None },
            focus: false,
            focus_physical: false,
            typed_remote: false,
            bytes: 0,
            scroll: 0,
            next_id: 1,
            waits: Vec::new(),
            poll_at: None,
            retry: None,
            events: Vec::new(),
        }
    }

    pub fn gate(&self) -> Gate {
        match &self.status {
            _ if self.stranger => Gate::Stranger,
            _ if self.silent => Gate::Silent,
            None => Gate::Starting,
            Some(s) if !s.ready => Gate::NotReady(if s.why.is_empty() { format!("{} ist nicht eingerichtet.", self.name) } else { s.why.clone() }),
            Some(_) => Gate::Ready,
        }
    }

    /// An answer of the shown chat runs.
    pub fn busy(&self) -> bool {
        self.ask.is_some()
    }

    /// A line of the sidebar's own in the list (the trash's listing).
    pub fn note_item(&mut self, text: &str, error: bool) {
        if self.room(text.len()) {
            self.push(Item::Note { text: text.to_string(), error });
        }
    }

    /// A question waits for the service to make this sidebar's chat.
    pub fn waits_for_chat(&self) -> bool {
        self.pending.is_some()
    }

    pub fn ask_id(&self) -> Option<u64> {
        self.ask.as_ref().map(|a| a.id)
    }

    pub fn pending(&self) -> usize {
        self.waits.len()
    }

    pub fn silent_line(&self) -> String {
        format!("{} antwortet nicht.", self.name)
    }

    pub fn stranger_line(&self) -> String {
        format!("Ein anderes Programm antwortet an der Stelle von {}.", self.name)
    }

    fn touch(&mut self, i: usize) {
        self.stale_from = self.stale_from.min(i);
    }

    fn room(&self, more: usize) -> bool {
        self.bytes + more + ITEM_COST <= LIST_MAX
    }

    fn push(&mut self, item: Item) -> usize {
        self.bytes += item.text().len() + ITEM_COST;
        self.items.push(item);
        self.touch(self.items.len() - 1);
        self.items.len() - 1
    }

    fn want(&mut self, w: Wait, now: Duration) -> String {
        self.waits.retain(|(k, _)| *k != w);
        self.waits.push((w, now + STATUS_WAIT));
        match (w, &self.chat) {
            (Wait::Status, _) => line(vec![("op", Json::str("status"))]),
            (Wait::History, Some(c)) => line(vec![("op", Json::str("history")), ("chat", Json::str(c))]),
            (Wait::History, None) => line(vec![("op", Json::str("chats"))]),
        }
    }

    /// Asks the status again (a setting changed, the sidebar came into view).
    pub fn refresh(&mut self, now: Duration) -> Vec<String> {
        if !self.up { Vec::new() } else { vec![self.want(Wait::Status, now)] }
    }

    /// The socket is connected: what is there, the chats, and the shown chat so far.
    pub fn link_up(&mut self, now: Duration) -> Vec<String> {
        (self.up, self.ever_up, self.down_since, self.silent, self.stranger) = (true, true, None, false, false);
        let mut out = vec![self.want(Wait::Status, now), chats::list_line()];
        if self.chat.is_some() {
            out.push(self.want(Wait::History, now));
        }
        out
    }

    /// The socket is gone (or was never there): a running answer ends, the sidebar says so after the grace.
    pub fn link_down(&mut self, why: &str, now: Duration) {
        if self.up || self.down_since.is_none() {
            self.events.push(format!("assist: no connection ({why})"));
        }
        (self.up, self.stranger) = (false, false);
        self.down_since.get_or_insert(now);
        self.waits.clear();
        (self.poll_at, self.retry) = (None, None);
        self.status = None;
        if self.ask.is_some() {
            let l = self.silent_line();
            self.end_ask(Some((l, true)));
        }
    }

    /// Somebody else answers on the socket: said at once, and nothing is sent there.
    pub fn link_stranger(&mut self, why: &str, now: Duration) {
        self.events.push(format!("assist: the socket's other end is not the assistant's helper ({why}): not used"));
        self.link_down(why, now);
        self.stranger = true;
    }

    /// The earliest deadline, if any.
    pub fn due(&self) -> Option<Duration> {
        let ask = self.ask.as_ref().and_then(|a| match (a.started, self.phase) {
            (false, _) => Some(a.asked + ASK_START),
            // Georg's answer has the assistant's own deadline, said in the stream.
            (true, Phase::Waiting) => None,
            (true, _) => Some(a.last + ASK_IDLE),
        });
        let cancel = self.ask.as_ref().and_then(|a| a.cancelling);
        let arm = self.card.as_ref().filter(|c| !c.armed).map(|c| c.since + ARM);
        let link = self.down_since.filter(|_| !self.silent).map(|t| t + LINK_GRACE);
        self.waits.iter().map(|(_, t)| *t).chain(ask).chain(cancel).chain(arm).chain(link).chain(self.poll_at).min()
    }

    /// Deadlines that passed by `now`.
    pub fn tick(&mut self, now: Duration) -> Vec<String> {
        let mut out = Vec::new();
        let open = self.waits.len();
        self.waits.retain(|(_, t)| *t > now);
        if self.waits.len() != open {
            self.events.push("assist: no reply within the deadline".into());
            self.silent = true;
            self.poll_at = Some(now + POLL);
        }
        if self.down_since.is_some_and(|t| !self.silent && t + LINK_GRACE <= now) {
            self.silent = true;
        }
        if let Some(c) = self.card.as_mut().filter(|c| !c.armed && c.since + ARM <= now) {
            // Ja counts from now on: the drawing shows it.
            c.armed = true;
        }
        if self.ask.as_ref().and_then(|a| a.cancelling).is_some_and(|t| t <= now) {
            // The assistant did not say "abgebrochen" (its provider hangs): for Georg it is over all the same.
            // Whether the helper is still busy with it shows in the status asked now.
            let id = self.ask.as_ref().map_or(0, |a| a.id);
            self.events.push(format!("assist: ask {id} cancelled, no word of the assistant within {} s: ended here", STATUS_WAIT.as_secs()));
            self.end_ask(Some(("abgebrochen".into(), false)));
            out.extend(self.refresh(now));
        }
        let late = self.ask.as_ref().is_some_and(|a| match (a.started, self.phase) {
            (false, _) => a.asked + ASK_START <= now,
            (true, Phase::Waiting) => false,
            (true, _) => a.last + ASK_IDLE <= now,
        });
        if late {
            let id = self.ask.as_ref().map_or(0, |a| a.id);
            self.events.push(format!("assist: ask {id} got no line within its deadline: cancelled"));
            let l = self.silent_line();
            // A question still waiting for its chat runs nowhere: nothing to cancel at the service.
            let chat = self.chat.clone().filter(|_| self.pending.is_none());
            self.end_ask(Some((l, true)));
            if let Some(chat) = chat {
                out.push(line(vec![("op", Json::str("cancel")), ("chat", Json::Str(chat)), ("id", Json::Num(id as f64))]));
            }
        }
        if self.poll_at.is_some_and(|t| t <= now) {
            self.poll_at = None;
            out.extend(self.refresh(now));
        }
        out
    }

    /// The input line's question; what is sent. Not sent: `note` says why and the input keeps its text.
    /// `injected`: the Enter or click was not Georg's own.
    pub fn submit(&mut self, now: Duration, injected: bool) -> Vec<String> {
        self.submit_with(now, injected, None)
    }

    /// `submit` with the window in front as the question's context (`assistant.sidebar_sees`, PLAN K4).
    pub fn submit_with(&mut self, now: Duration, injected: bool, context: Option<Json>) -> Vec<String> {
        let text = self.edit.text.clone();
        let out = self.ask_with(&text, now, injected || self.typed_remote, context);
        if !out.is_empty() {
            self.edit.text.clear();
            (self.edit.cursor, self.typed_remote) = (0, false);
        }
        out
    }

    /// A question; what is sent. One that injected input asked is marked in the list, and so are the
    /// questions its tools put to Georg (he did not ask, he must be able to see that).
    pub fn ask(&mut self, text: &str, now: Duration, injected: bool) -> Vec<String> {
        self.ask_with(text, now, injected, None)
    }

    pub fn ask_with(&mut self, text: &str, now: Duration, injected: bool, context: Option<Json>) -> Vec<String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        let refuse = match self.gate() {
            Gate::Stranger => Some(self.stranger_line()),
            Gate::Silent => Some(self.silent_line()),
            Gate::Starting => Some(format!("{} ist noch nicht bereit.", self.name)),
            Gate::NotReady(why) => Some(why),
            Gate::Ready if self.ask.is_some() || self.pending.is_some() => Some(format!("{} antwortet noch. Esc bricht ab.", self.name)),
            Gate::Ready if !self.room(text.len() + 2 * ITEM_COST) => Some("Das Gespräch ist zu lang geworden. Beginne ein neues Gespräch.".into()),
            Gate::Ready => None,
        };
        if let Some(why) = refuse {
            self.note = Some((why, true));
            return Vec::new();
        }
        let id = self.next_id;
        self.next_id += 1;
        self.push(Item::Turn { who: Who::Georg, text: text.clone() });
        if injected {
            self.push(Item::Note { text: REMOTE_ASK.into(), error: false });
            self.events.push(format!("assist: ask {id} from injected input"));
        }
        self.ask = Some(Ask { id, asked: now, last: now, started: false, turn: None, tool: None, remote: injected, cancelling: None });
        self.phase = Phase::Thinking;
        (self.note, self.scroll, self.level_open) = (None, 0, false);
        match self.chat.clone() {
            Some(chat) => vec![ask_line(&chat, id, &text, injected, context)],
            None => {
                // The chat is made first; the question follows the service's answer (`chat_made`).
                self.pending = Some((text, injected, context));
                vec![chats::new_line(chats::Origin::Sidebar, None)]
            }
        }
    }

    /// The service made the chat this sidebar asked for: the question waiting for it goes.
    pub fn chat_made(&mut self, chat: &str, now: Duration) -> Vec<String> {
        (self.chat, self.fresh) = (Some(chat.to_string()), false);
        let Some((text, injected, context)) = self.pending.take() else { return vec![chats::list_line()] };
        let Some(a) = self.ask.as_mut() else { return vec![chats::list_line()] };
        // The start's deadline counts from now: the chat took a moment.
        (a.asked, a.last) = (now, now);
        vec![ask_line(chat, a.id, &text, injected, context)]
    }

    /// Opens a chat of the list: its history is read (with the tool lines).
    pub fn open_chat(&mut self, chat: &str, now: Duration) -> Vec<String> {
        if self.chat.as_deref() == Some(chat) && !self.items.is_empty() {
            return Vec::new();
        }
        let mut out = self.cancel_own(now);
        self.end_ask(None);
        self.pending = None;
        self.items.clear();
        (self.stale_from, self.scroll, self.note, self.bytes) = (0, 0, None, 0);
        self.chat = Some(chat.to_string());
        out.push(self.want(Wait::History, now));
        out
    }

    /// Esc or "Abbrechen": the running answer ends with the assistant's "abgebrochen", or here when that
    /// does not come within `STATUS_WAIT` (`tick`).
    pub fn cancel(&mut self, now: Duration) -> Vec<String> {
        if self.ask.is_some() && self.pending.take().is_some() {
            // Asked before the chat was there: nothing runs at the service yet.
            self.end_ask(Some(("abgebrochen".into(), false)));
            return Vec::new();
        }
        let chat = self.chat.clone().unwrap_or_default();
        match self.ask.as_mut() {
            Some(a) => {
                a.cancelling.get_or_insert(now + STATUS_WAIT);
                vec![line(vec![("op", Json::str("cancel")), ("chat", Json::Str(chat)), ("id", Json::Num(a.id as f64))])]
            }
            None => Vec::new(),
        }
    }

    /// The running answer of the shown chat is cancelled because the view goes elsewhere.
    fn cancel_own(&mut self, now: Duration) -> Vec<String> {
        if self.ask.is_some() { self.cancel(now) } else { Vec::new() }
    }

    /// "+ Neuer Chat": the view is empty; the chat is made with its first question (no empty chats in the
    /// list). A running answer of the chat shown before goes on there (it is in the list).
    pub fn new_conversation(&mut self, now: Duration) -> Vec<String> {
        if !self.up {
            self.note = Some((if self.stranger { self.stranger_line() } else { self.silent_line() }, true));
            return Vec::new();
        }
        if self.pending.is_some() {
            self.cancel(now);
        }
        self.ask = None;
        (self.phase, self.notice, self.tool, self.card) = (Phase::Idle, None, None, None);
        self.items.clear();
        (self.chat, self.fresh) = (None, true);
        (self.stale_from, self.scroll, self.note, self.bytes) = (0, 0, None, 0);
        vec![self.want(Wait::Status, now)]
    }

    /// A line of the socket.
    pub fn on(&mut self, j: &Json, now: Duration) -> Vec<String> {
        self.silent = false;
        if let Some(ev) = j.get("ev").and_then(Json::as_str) {
            return self.event(ev, j, now);
        }
        match j.get("ok").and_then(Json::as_bool) {
            Some(false) => self.refused(j),
            Some(true) if j.get("chats").and_then(Json::as_array).is_some() => return self.chats_in(j, now),
            Some(true) if j.get("turns").and_then(Json::as_array).is_some() => {
                let theirs = j.get("chat").and_then(Json::as_str);
                if theirs.is_none() || theirs == self.chat.as_deref() {
                    self.history(j)
                }
            }
            Some(true) if j.get("ready").is_some() => return self.status_in(j, now),
            Some(true) => {}
            None => self.events.push("assist: a line that is neither an event nor a reply".into()),
        }
        Vec::new()
    }

    fn refused(&mut self, j: &Json) {
        let msg = match text_of(j, "message") {
            "" => format!("{} hat die Anfrage abgelehnt.", self.name),
            m => m.to_string(),
        };
        self.events.push(format!("assist: refused ({})", text_of(j, "error")));
        if self.ask.as_ref().is_some_and(|a| !a.started) {
            // The question never started (busy, not ready, no chat made): it is over, with the reason in the list.
            self.pending = None;
            return self.end_ask(Some((msg, true)));
        }
        // Which request was refused the reply does not say: a status or history still open keeps its
        // deadline (one that is never answered ends in "antwortet nicht", not in a wait without end).
        self.note = Some((msg, true));
    }

    fn history(&mut self, j: &Json) {
        self.waits.retain(|(k, _)| *k != Wait::History);
        if self.ask.is_some() {
            return;
        }
        // The newest turns that fit the list, each in pieces of an item's size.
        let (mut items, mut bytes) = (Vec::new(), 0);
        for t in j.get("turns").and_then(Json::as_array).unwrap_or(&[]).iter().rev() {
            // A tool line of the history (the engine's `{who:"tool", name, line, state}`; `{"tool":{...}}` too) is
            // shown as it was.
            if let Some(tl) = t.get("tool").or_else(|| (text_of(t, "who") == "tool").then_some(t)) {
                let item = chats::tool_item(tl);
                let cost = item.text().len() + ITEM_COST;
                if bytes + cost > LIST_MAX {
                    break;
                }
                bytes += cost;
                items.push(item);
                continue;
            }
            let who = if text_of(t, "who") == "georg" { Who::Georg } else { Who::Assistant };
            let (mut parts, mut rest) = (Vec::new(), text_of(t, "text"));
            loop {
                let (take, more) = piece(rest, PIECE);
                parts.push(take);
                if more.is_empty() {
                    break;
                }
                rest = more;
            }
            let cost: usize = parts.iter().map(|p| p.len() + ITEM_COST).sum();
            if bytes + cost > LIST_MAX {
                self.events.push("assist: the history is larger than the list holds: its oldest turns are not shown".into());
                break;
            }
            bytes += cost;
            items.extend(parts.into_iter().rev().map(|p| Item::Turn { who, text: p.to_string() }));
        }
        items.reverse();
        (self.items, self.bytes) = (items, bytes);
        (self.stale_from, self.scroll) = (0, 0);
    }

    fn status_in(&mut self, j: &Json, now: Duration) -> Vec<String> {
        self.waits.retain(|(k, _)| *k != Wait::Status);
        // Short texts of the settings; cut so that no reply can be a page of its own.
        let opt = |k: &str| j.get(k).and_then(Json::as_str).map(|s| cut(s, 200).to_string());
        let flag = |k: &str| j.get(k).and_then(Json::as_bool).unwrap_or(false);
        let s = Status {
            provider: opt("provider"),
            place: opt("place"),
            local_only: flag("local_only"),
            ready: flag("ready"),
            why: cut(text_of(j, "why"), 2000).to_string(),
            busy: flag("busy"),
            turns: j.get("turns").and_then(Json::as_u64).unwrap_or(0),
        };
        if let Some(n) = opt("name").filter(|n| !n.trim().is_empty()) {
            self.name = n.trim().to_string();
        }
        let busy = s.busy;
        let down = !s.ready && (flag("settings_down") || s.why == UNREACHABLE);
        self.status = Some(s);
        let before = self.retry;
        self.retry = down.then(|| before.map_or(RETRY_FIRST, |d| (d * 2).min(RETRY_MAX)));
        if let Some(d) = self.retry.filter(|d| before != Some(*d)) {
            self.events.push(format!("assist: the assistant cannot reach the settings yet: asked again in {} ms", d.as_millis()));
        }
        if self.ask.is_some() {
            return Vec::new();
        }
        // EINS-B: the service is busy with any of its chats; this sidebar shows only its own chat's lines (they
        // come as events of that chat, `assist_route`), so a busy status alone turns no ring.
        let _ = busy;
        self.poll_at = self.retry.map(|d| now + d);
        Vec::new()
    }

    /// One character into the input line; false when the line is full (said above it).
    pub fn insert(&mut self, c: char, injected: bool) -> bool {
        if self.edit.text.len() + c.len_utf8() > INPUT_MAX {
            self.note = Some(("Mehr passt nicht in eine Frage.".into(), true));
            return false;
        }
        self.edit.insert(c);
        self.typed_remote |= injected;
        true
    }

    /// Text from the clipboard or `ctl assist paste`: line ends stay, other control characters go; what does
    /// not fit the input line is left out, and the line says so.
    pub fn paste(&mut self, text: &str, injected: bool) {
        self.note = None;
        for c in text.replace("\r\n", "\n").chars() {
            let c = match c {
                '\n' => '\n',
                '\t' | '\r' => ' ',
                c if !c.is_control() => c,
                _ => continue,
            };
            if !self.insert(c, injected) {
                self.note = Some(("Der Text war zu lang: nur sein Anfang ist eingefügt.".into(), true));
                break;
            }
        }
    }
}

fn line(pairs: Vec<(&str, Json)>) -> String {
    Json::obj(pairs).to_string()
}

/// K1 `ask`: `by` says whether Georg's own hand asked (`hand`) or injected input (`remote`).
pub fn ask_line(chat: &str, id: u64, text: &str, remote: bool, context: Option<Json>) -> String {
    let mut v = vec![
        ("op", Json::str("ask")),
        ("chat", Json::str(chat)),
        ("id", Json::Num(id as f64)),
        ("text", Json::str(text)),
        ("by", Json::str(if remote { "remote" } else { "hand" })),
    ];
    if let Some(c) = context {
        v.push(("context", c));
    }
    line(v)
}

fn text_of<'a>(j: &'a Json, key: &str) -> &'a str {
    j.get(key).and_then(Json::as_str).unwrap_or("")
}

/// The start of `t` that fits `room` bytes and what follows it: cut at the last line end where there is one
/// (the line end goes: the next item is a paragraph of its own anyway), else at a character's start.
fn piece(t: &str, room: usize) -> (&str, &str) {
    if t.len() <= room {
        return (t, "");
    }
    // Room for one character at least, so every call takes something.
    let head = cut(t, room.max(4));
    match head.rfind('\n') {
        Some(nl) => (&t[..nl], &t[nl + 1..]),
        None => (head, &t[head.len()..]),
    }
}

/// `s` cut to at most `max` bytes, at a character's start.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
#[path = "assist_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "assist_card_tests.rs"]
mod card_tests;
