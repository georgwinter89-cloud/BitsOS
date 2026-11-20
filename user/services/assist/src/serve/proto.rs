//! The lines of the protocol: answers to a request (`ok`) and the events of a running question (`ev`).

use crate::json::Json;

/// The message of the `error` event that ends a question Georg stopped.
pub const CANCELLED: &str = "abgebrochen";

pub fn ok(op: &str) -> Json {
    Json::obj().with("ok", true).with("op", op)
}

/// A refusal: `code` for the program, `message` (German) for Georg.
pub fn err(code: &str, message: impl Into<String>) -> Json {
    Json::obj().with("ok", false).with("error", code).with("message", message.into())
}

pub fn ev(name: &str, id: i64) -> Json {
    Json::obj().with("ev", name).with("id", id)
}

/// What everybody but the session's compositor is told, whatever they sent.
pub fn not_allowed() -> Json {
    err("not_allowed", "Mit dem Assistenten spricht nur die Oberfläche von BitsOS.")
}
