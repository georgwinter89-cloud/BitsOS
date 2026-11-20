//! EINS-B (PLAN K1): the service's chats as the sidebar lists them (board 3: title, where it came from, when),
//! and the request lines that make, list and remove chats.

use mcpreg::Json;

use super::{Item, Model, ToolState, cut, line, text_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Sidebar,
    Popup,
    Console,
}

impl Origin {
    pub fn word(self) -> &'static str {
        match self {
            Origin::Sidebar => "sidebar",
            Origin::Popup => "popup",
            Origin::Console => "console",
        }
    }

    fn of(w: &str) -> Origin {
        match w {
            "popup" => Origin::Popup,
            "console" => Origin::Console,
            _ => Origin::Sidebar,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRow {
    pub chat: String,
    pub title: String,
    pub origin: Origin,
    pub app: Option<String>,
    /// Seconds since the epoch of its last turn.
    pub updated: i64,
    pub turns: u64,
}

impl ChatRow {
    /// The line under the title: "aus Win+F1 · Brave · 22:41", "Seitenleiste · heute 18:12".
    pub fn sub(&self, now: i64, offset_min: i32) -> String {
        let from = match (self.origin, self.app.as_deref().filter(|a| !a.is_empty())) {
            (Origin::Popup, Some(a)) => format!("aus Win+F1 · {a}"),
            (Origin::Popup, None) => "aus Win+F1".into(),
            (Origin::Console, _) => "Konsole".into(),
            (Origin::Sidebar, _) => "Seitenleiste".into(),
        };
        format!("{from} · {}", when(self.updated, now, offset_min))
    }
}

/// When a chat was last used, as the board says it: within the hour "22:41", today "heute 18:12", "gestern",
/// else the day "12.11.".
pub fn when(t: i64, now: i64, offset_min: i32) -> String {
    let off = i64::from(offset_min) * 60;
    let (lt, ln) = (t + off, now + off);
    let day = |x: i64| x.div_euclid(86_400);
    let hm = format!("{:02}:{:02}", lt.rem_euclid(86_400) / 3600, lt.rem_euclid(3600) / 60);
    if day(lt) == day(ln) {
        if ln - lt < 3600 { hm } else { format!("heute {hm}") }
    } else if day(lt) + 1 == day(ln) {
        "gestern".into()
    } else {
        // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
        let z = day(lt) + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        format!("{d:02}.{m:02}.")
    }
}

pub fn list_line() -> String {
    line(vec![("op", Json::str("chats"))])
}

pub fn new_line(origin: Origin, app: Option<&str>) -> String {
    let mut v = vec![("op", Json::str("new")), ("origin", Json::str(origin.word()))];
    if let Some(a) = app.filter(|a| !a.is_empty()) {
        v.push(("app", Json::str(a)));
    }
    line(v)
}

pub fn remove_line(chat: &str) -> String {
    line(vec![("op", Json::str("remove")), ("chat", Json::str(chat))])
}

/// A tool line of a chat's history, as the list shows it.
pub fn tool_item(t: &Json) -> Item {
    let state = match text_of(t, "state") {
        "refused" => ToolState::Refused,
        "asks" | "runs" => ToolState::Stopped,
        _ => ToolState::Done,
    };
    let text = match text_of(t, "line") {
        "" => text_of(t, "text"),
        l => l,
    };
    Item::Tool { name: cut(text_of(t, "name"), 200).to_string(), text: cut(text, 2000).to_string(), state }
}

/// The most chats the sidebar keeps in its list (the newest).
pub const LIST_MAX: usize = 200;

impl Model {
    /// The service's `chats` reply: newest first. A sidebar that shows no chat (it just started, the desktop came
    /// back) opens its newest own one, as it was before; not after Georg chose "+ Neuer Chat".
    pub(super) fn chats_in(&mut self, j: &Json, now: std::time::Duration) -> Vec<String> {
        let mut v: Vec<ChatRow> = j
            .get("chats")
            .and_then(Json::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|c| {
                let chat = c.get("chat").and_then(Json::as_str).filter(|s| !s.is_empty())?;
                Some(ChatRow {
                    chat: cut(chat, 120).to_string(),
                    title: cut(text_of(c, "title").trim(), 200).to_string(),
                    origin: Origin::of(text_of(c, "origin")),
                    app: c.get("app").and_then(Json::as_str).map(|a| cut(a, 80).to_string()),
                    updated: c.get("updated").and_then(Json::as_i64).unwrap_or(0),
                    turns: c.get("turns").and_then(Json::as_u64).unwrap_or(0),
                })
            })
            .collect();
        v.sort_by(|a, b| b.updated.cmp(&a.updated));
        v.truncate(LIST_MAX);
        self.chats = v;
        // A shown chat that is gone (removed elsewhere) leaves the view empty.
        if let Some(c) = self.chat.clone().filter(|c| !self.chats.iter().any(|r| r.chat == *c) && self.ask.is_none() && self.pending.is_none() && !self.items.is_empty()) {
            self.events.push(format!("assist: chat {c} is no longer in the list"));
        }
        let idle = self.chat.is_none() && self.items.is_empty() && self.ask.is_none() && self.pending.is_none() && !self.fresh;
        match self.chats.iter().find(|r| r.origin == Origin::Sidebar).map(|r| r.chat.clone()).filter(|_| idle) {
            Some(c) => self.open_chat(&c, now),
            None => Vec::new(),
        }
    }

    /// The title of a chat for its row and its removal question.
    pub fn chat_title(&self, chat: &str) -> String {
        self.chats.iter().find(|r| r.chat == chat).map(|r| if r.title.is_empty() { "Ohne Titel".to_string() } else { r.title.clone() }).unwrap_or_else(|| "dieser Chat".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_reads_as_the_board_says() {
        let now = 1_763_419_260; // 2025-11-17 22:41 UTC
        assert_eq!(when(now - 120, now, 0), "22:39");
        assert_eq!(when(now - 4 * 3600, now, 0), "heute 18:41");
        assert_eq!(when(now - 86_400, now, 0), "gestern");
        assert_eq!(when(now - 5 * 86_400, now, 0), "12.11.");
        assert_eq!(when(now - 120, now, 60), "23:39", "local time");
    }

    #[test]
    fn rows_say_where_they_came_from() {
        let r = ChatRow { chat: "c1".into(), title: "such nach Georg".into(), origin: Origin::Popup, app: Some("Brave".into()), updated: 1000, turns: 2 };
        assert_eq!(r.sub(1100, 0), "aus Win+F1 · Brave · 00:16");
        assert_eq!(new_line(Origin::Popup, Some("Brave")), r#"{"op":"new","origin":"popup","app":"Brave"}"#);
        assert_eq!(remove_line("c1"), r#"{"op":"remove","chat":"c1"}"#);
    }
}
