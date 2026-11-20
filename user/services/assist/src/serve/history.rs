//! A chat as a view shows it again (K1 `history`): Georg's questions, the assistant's answers, and between
//! them the tool lines of how each call ended, oldest first, from the chat's file.

use super::chats::{Loaded, Record};
use crate::json::Json;

fn at(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// Every entry: `{who:"georg"|"assistant"|"tool"|"error", ...}`.
pub fn entries(l: &Loaded) -> Vec<Json> {
    l.records
        .iter()
        .filter_map(|(r, t)| {
            let j = Json::obj().with("at", at(*t));
            Some(match r {
                Record::Georg { text, picture, by, .. } => j.with("who", "georg").with("text", text.as_str()).with("picture", *picture).with("by", by.as_str()),
                Record::Assistant(a) => j.with("who", "assistant").with("text", a.as_str()),
                Record::Tool { name, state, line } => j.with("who", "tool").with("name", name.as_str()).with("state", state.as_str()).with("line", line.as_str()),
                Record::Failed(why) => j.with("who", "error").with("text", why.as_str()),
                Record::Title(_) => return None,
            })
        })
        .collect()
}

/// What a view of before K1 reads: the conversation as it was kept, Georg's words and the answers alone,
/// `{who, text}`; a question that is at work or ended without an answer is not part of it.
pub fn plain(l: &Loaded) -> Vec<Json> {
    let mut out = Vec::new();
    let mut open: Option<&str> = None;
    for (r, _) in &l.records {
        match r {
            Record::Georg { text, .. } => open = Some(text),
            Record::Assistant(a) => {
                if let Some(q) = open.take() {
                    out.push(Json::obj().with("who", "georg").with("text", q));
                }
                out.push(Json::obj().with("who", "assistant").with("text", a.as_str()));
            }
            Record::Failed(_) => open = None,
            Record::Tool { .. } | Record::Title(_) => {}
        }
    }
    out
}
