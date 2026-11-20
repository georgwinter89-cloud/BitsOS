//! The lines a helper writes on its stdout (HELPER.md, "The answer: lines on stdout").

use crate::conv::Stop;
use crate::json::{self, Json};

/// Where a tool of the helper stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Runs,
    Done,
    Failed,
}

pub(super) enum Line {
    Text(String),
    Note(String),
    /// `{"tool":NAME,"state":...,"text":...}`: the helper tells what it is doing.
    Tool(String, Step, String),
    Done(Result<Stop, String>),
    /// An object without a word this version knows: a newer helper may say more. It still shows that the
    /// helper is alive.
    Unknown,
}

pub(super) fn parse(line: &str) -> Option<Line> {
    let Ok(j @ Json::Obj(_)) = json::parse(line) else { return None };
    if j.get("done").and_then(Json::as_bool) == Some(true) {
        return Some(Line::Done(match j.str("stop").unwrap_or("end") {
            "end" => Ok(Stop::End),
            "length" => Ok(Stop::MaxTokens),
            "refused" => Ok(Stop::Refusal),
            "error" => Err(j.str("error").unwrap_or("").to_string()),
            other => Ok(Stop::Other(other.to_string())),
        }));
    }
    if let Some(name) = j.str("tool") {
        let step = match j.str("state") {
            Some("runs") => Step::Runs,
            Some("done") => Step::Done,
            Some("failed") => Step::Failed,
            // A state of a later version: not shown, and no reason to end the answer.
            _ => return Some(Line::Unknown),
        };
        return Some(Line::Tool(name.to_string(), step, j.str("text").unwrap_or("").to_string()));
    }
    Some(match (j.str("text"), j.str("note")) {
        (Some(t), _) => Line::Text(t.to_string()),
        (None, Some(n)) => Line::Note(n.to_string()),
        (None, None) => Line::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lines_of_the_protocol() {
        let stop = |l: &str| match parse(l) {
            Some(Line::Done(s)) => s,
            _ => panic!("{l}"),
        };
        assert!(matches!(parse(r#"{"text":"Hal"}"#), Some(Line::Text(t)) if t == "Hal"));
        assert!(matches!(parse(r#"{"note":"denkt nach","more":1}"#), Some(Line::Note(n)) if n == "denkt nach"));
        assert!(matches!(parse(r#"{"usage":{"in":3}}"#), Some(Line::Unknown)));
        assert!(matches!(parse("{}"), Some(Line::Unknown)));
        assert!(matches!(parse(r#"{"done":false,"text":"x"}"#), Some(Line::Text(_))));
        assert_eq!(stop(r#"{"done":true}"#), Ok(Stop::End));
        assert_eq!(stop(r#"{"done":true,"stop":"end","error":null}"#), Ok(Stop::End));
        assert_eq!(stop(r#"{"done":true,"stop":"length"}"#), Ok(Stop::MaxTokens));
        assert_eq!(stop(r#"{"done":true,"stop":"refused"}"#), Ok(Stop::Refusal));
        assert_eq!(stop(r#"{"done":true,"stop":"pause"}"#), Ok(Stop::Other("pause".into())));
        assert_eq!(stop(r#"{"done":true,"stop":"error","error":"Nicht angemeldet."}"#), Err("Nicht angemeldet.".into()));
        assert_eq!(stop(r#"{"done":true,"stop":"error"}"#), Err(String::new()));
        for garbage in ["Hallo", "[1]", "7", "{\"text\":", "\"text\""] {
            assert!(parse(garbage).is_none(), "{garbage}");
        }
    }

    #[test]
    fn a_tool_line_names_the_tool_its_state_and_what_it_does() {
        assert!(matches!(parse(r#"{"tool":"Bash","state":"runs","text":"ls -la"}"#), Some(Line::Tool(n, Step::Runs, t)) if n == "Bash" && t == "ls -la"));
        assert!(matches!(parse(r#"{"tool":"Bash","state":"done"}"#), Some(Line::Tool(_, Step::Done, t)) if t.is_empty()));
        assert!(matches!(parse(r#"{"tool":"Bash","state":"failed","text":"exit 2"}"#), Some(Line::Tool(_, Step::Failed, _))));
        // A tool line is never taken for text of the answer, whatever else it carries.
        assert!(matches!(parse(r#"{"tool":"Bash","state":"runs","text":"x","note":"y"}"#), Some(Line::Tool(..))));
        assert!(matches!(parse(r#"{"tool":"Bash","state":"waits","text":"x"}"#), Some(Line::Unknown)));
        assert!(matches!(parse(r#"{"tool":"Bash","text":"x"}"#), Some(Line::Unknown)));
    }
}
