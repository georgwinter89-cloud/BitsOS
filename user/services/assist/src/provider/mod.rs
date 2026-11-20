//! AI providers and the one interface every provider kind sits behind. An adapter is pure: it turns a
//! conversation into an HTTP request, the answer's events into text, tool calls and a stop reason, and an error
//! status into one of [`ErrKind`]. The engine owns the connection, so adapters are tested against recorded
//! streams without a socket.

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod program;

use crate::conv::{Conversation, ToolCall, Turn};
use crate::json::Json;
use crate::secret::Secret;
use crate::tools::ToolSpec;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Anthropic,
    OpenAi,
    OpenAiCompatible,
    Gemini,
    /// A helper program on this machine answers (`program`): no HTTP, no key.
    Program,
}

impl Kind {
    pub const NAMES: [&'static str; 5] = ["anthropic", "openai", "gemini", "openai_compatible", "program"];

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "anthropic" => Kind::Anthropic,
            "openai" => Kind::OpenAi,
            "openai_compatible" => Kind::OpenAiCompatible,
            "gemini" => Kind::Gemini,
            "program" => Kind::Program,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Anthropic => "anthropic",
            Kind::OpenAi => "openai",
            Kind::OpenAiCompatible => "openai_compatible",
            Kind::Gemini => "gemini",
            Kind::Program => "program",
        }
    }
    pub fn adapter(self) -> &'static dyn Adapter {
        match self {
            Kind::Anthropic => &anthropic::Anthropic,
            Kind::OpenAi | Kind::OpenAiCompatible => &openai::OpenAi,
            Kind::Gemini => &gemini::Gemini,
            Kind::Program => &program::NoHttp,
        }
    }
}

/// Where a provider runs. Georg sets it; nothing derives it from the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Cloud,
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    pub kind: Kind,
    /// The URL of the service; for `Kind::Program` the full path of the helper.
    pub address: String,
    pub model: String,
    pub key: Secret,
    pub place: Place,
    /// `ai.max_tokens`, for the kinds whose interface requires a limit.
    pub max_tokens: u64,
}

/// What goes on the wire. No `Debug`: a header holds the key.
pub struct HttpRequest {
    /// Appended to the path of the provider's address.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrKind {
    /// The key is missing, wrong or revoked.
    Key,
    Forbidden,
    Billing,
    RateLimit,
    Overloaded,
    ModelNotFound,
    BadRequest,
    /// The conversation is longer than the model takes.
    TooLarge,
    Server,
    Dns,
    Network,
    Tls,
    Clock,
    Timeout,
    /// The stream ended in the middle of the answer.
    Cut,
    Protocol,
    /// The answer is larger than this machine can hold.
    TooBig,
    Cancelled,
}

impl ErrKind {
    /// The provider is away or busy, not wrong: worth another try, and worth the fallback provider.
    pub fn away(self) -> bool {
        matches!(self, ErrKind::RateLimit | ErrKind::Overloaded | ErrKind::Server | ErrKind::Dns | ErrKind::Network | ErrKind::Timeout | ErrKind::Cut)
    }
    /// The same request may succeed a moment later.
    pub fn retryable(self) -> bool {
        matches!(self, ErrKind::RateLimit | ErrKind::Overloaded | ErrKind::Server)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    pub kind: ErrKind,
    pub status: Option<u16>,
    /// The provider's own words (error type and message) for the log; never shown as the explanation.
    pub raw: String,
    pub retry_after: Option<Duration>,
}

impl ApiError {
    pub fn new(kind: ErrKind, raw: impl Into<String>) -> ApiError {
        ApiError { kind, status: None, raw: raw.into(), retry_after: None }
    }
    pub fn http(kind: ErrKind, status: u16, raw: impl Into<String>) -> ApiError {
        ApiError { kind, status: Some(status), raw: raw.into(), retry_after: None }
    }
    /// One line for the log: `http=401 kind=Key raw="..."`. The caller scrubs the key out of it.
    pub fn log_line(&self) -> String {
        let status = self.status.map_or("-".to_string(), |s| s.to_string());
        format!("http={status} kind={:?} raw={:?}", self.kind, self.raw)
    }
}

/// What an adapter reports while the answer streams in.
pub enum Event<'a> {
    Text(&'a str),
    /// A complete tool call (its arguments have all arrived).
    Call(&'a ToolCall),
}

pub trait Adapter: Sync {
    /// The request that sends `conv` with `tools` to `p` and asks for a streamed answer.
    fn request(&self, p: &Provider, conv: &Conversation, tools: &[ToolSpec]) -> HttpRequest;
    /// The reader of one streamed answer.
    fn parser(&self, p: &Provider) -> Box<dyn Parser>;
    /// What an answer with a status other than 200 means.
    fn error(&self, status: u16, body: &str) -> ApiError;
}

pub trait Parser {
    /// One server-sent event. Text goes to `out` at once, a tool call when it is complete.
    fn event(&mut self, event: &str, data: &str, out: &mut dyn FnMut(Event)) -> Result<(), ApiError>;
    /// The provider said the answer is complete: no need to wait for the connection to close.
    fn done(&self) -> bool;
    /// The whole answer and why it stopped; `ErrKind::Cut` when the stream ended before the provider said so.
    fn finish(self: Box<Self>) -> Result<Turn, ApiError>;
}

/// The usual meaning of a status when the body names nothing better.
pub fn kind_of_status(status: u16) -> ErrKind {
    match status {
        401 => ErrKind::Key,
        402 => ErrKind::Billing,
        403 => ErrKind::Forbidden,
        404 => ErrKind::ModelNotFound,
        413 => ErrKind::TooLarge,
        429 => ErrKind::RateLimit,
        503 | 529 => ErrKind::Overloaded,
        500..=599 => ErrKind::Server,
        _ => ErrKind::BadRequest,
    }
}

/// `{"error":{...}}` of an error body, when it is JSON; adapters read their own fields from it.
pub(crate) fn error_object(body: &str) -> Option<Json> {
    crate::json::parse(body.trim()).ok()?.get("error").cloned()
}

/// A provider's message as one log line (line breaks and control characters become spaces).
pub(crate) fn brief(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

pub(crate) fn cut() -> ApiError {
    ApiError::new(ErrKind::Cut, "the stream ended before the provider finished the answer")
}

pub(crate) fn bad_event(data: &str) -> ApiError {
    ApiError::new(ErrKind::Protocol, format!("event is not JSON: {}", brief(data)))
}
