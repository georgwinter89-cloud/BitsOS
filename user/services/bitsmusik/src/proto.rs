//! The socket's requests (spec/sofa-mode.md 3.5): one JSON object per line with `op`, or the op as a bare word
//! (for a person at a shell). Keys are English; `text` in an error answer is the German screen sentence.

use settingsd::json::{self, Json};

/// Bytes of one request line (input bound; a longer line is refused and the connection closed).
pub const LINE_MAX: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Every track of the library, in path order.
    All,
    List(String),
    Album(String),
    /// A track by its path, else by its title (case ignored).
    Track(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Status,
    Library,
    Play { pick: Pick, index: usize },
    Pause,
    Resume,
    Next,
    Prev,
    Stop,
    Rescan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub error: &'static str,
    pub text: &'static str,
}

const BAD: Refusal = Refusal {
    error: "bad request",
    text: "Unbekannte Anfrage",
};

pub fn parse(line: &str) -> Result<Request, Refusal> {
    let line = line.trim();
    let j = if line.starts_with('{') {
        json::parse(line).map_err(|_| BAD)?
    } else {
        Json::obj().with("op", line)
    };
    let op = j.str("op").ok_or(BAD)?;
    Ok(match op {
        "status" => Request::Status,
        "library" => Request::Library,
        "pause" => Request::Pause,
        "resume" => Request::Resume,
        "next" => Request::Next,
        "prev" => Request::Prev,
        "stop" => Request::Stop,
        "rescan" => Request::Rescan,
        "play" => {
            let index = match j.get("index") {
                None | Some(Json::Null) => 0,
                Some(v) => v
                    .as_i64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or(BAD)?,
            };
            let named = |k: &str| j.get(k).map(|v| v.as_str().map(str::to_string).ok_or(BAD));
            let pick = match (named("list"), named("album"), named("track")) {
                (None, None, None) => Pick::All,
                (Some(l), None, None) => Pick::List(l?),
                (None, Some(a), None) => Pick::Album(a?),
                (None, None, Some(t)) => Pick::Track(t?),
                _ => return Err(BAD),
            };
            Request::Play { pick, index }
        }
        _ => return Err(BAD),
    })
}

pub fn refusal(r: &Refusal) -> Json {
    Json::obj()
        .with("ok", false)
        .with("error", r.error)
        .with("text", r.text)
}
