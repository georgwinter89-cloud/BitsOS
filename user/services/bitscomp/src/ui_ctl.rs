//! DB1: the control socket's `ui` commands (contract C2). `ui` = every window as C1 JSON, elements only for the
//! focused one; `ui <window>|focus` = that window with its elements; `ui text [<window>|focus]` = the same as
//! indented lines for people; `ui-act <element-id> <action> [text]` = ok or a German reason (`\\n` in the text
//! is Enter); `ui-history [<seconds>]` = the recent actions as JSON lines.

use crate::state::App;
use crate::ui_reg::Full;

pub fn ctl(app: &mut App, words: &[&str], line: &str) -> Option<Result<String, String>> {
    Some(match *words.first()? {
        "ui" => {
            let (text, rest) = if words.get(1) == Some(&"text") { (true, &words[2..]) } else { (false, &words[1..]) };
            let full = match rest.first().copied() {
                None => Full::Focused,
                Some("focus") => Full::One(app.ui_focused_id()),
                Some(w) => Full::One(w.to_string()),
            };
            let mut list = app.ui_windows(&full);
            if let Full::One(w) = &full {
                list.retain(|x| &x.window == w);
                if list.is_empty() {
                    return Some(Err(format!("Ein Fenster {w} gibt es nicht.")));
                }
            }
            if text {
                Ok(uilist::render(&list).trim_end().to_string())
            } else if matches!(full, Full::One(_)) {
                serde_json::to_string(&list[0]).map_err(|e| e.to_string())
            } else {
                serde_json::to_string(&list).map_err(|e| e.to_string())
            }
        }
        "ui-act" => {
            let (Some(id), Some(action)) = (words.get(1), words.get(2)) else {
                return Some(Err("ui-act <element> press|type|choose|toggle|scroll|focus [text]".into()));
            };
            // The text is everything after the action word, spaces kept; a command is one line, so `\n` stands
            // for Enter there (MCP's text carries real line breaks).
            let text = line.trim_start().splitn(4, char::is_whitespace).nth(3).map(str::trim_start).filter(|t| !t.is_empty()).map(|t| t.replace("\\n", "\n"));
            app.ui_act(id, action, text.as_deref())
        }
        "ui-history" => {
            let secs = match words.get(1) {
                Some(s) => match s.parse::<u64>() {
                    Ok(n) => Some(n),
                    Err(_) => return Some(Err(format!("„{s}“ ist keine Zahl von Sekunden."))),
                },
                None => None,
            };
            Ok(crate::ui_history::with(|h| h.lines(secs, crate::ui_history::now_s())).trim_end().to_string())
        }
        _ => return None,
    })
}
