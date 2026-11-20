//! Gemini `streamGenerateContent` (server-sent events, `alt=sse`) with function calling. A function call arrives
//! whole in one part, never in pieces; the stream has no end mark of its own, the last chunk carries
//! `finishReason`. The model's parts go back unchanged (`Message::raw`): a part may carry a `thoughtSignature`
//! that the next request must return with the call it belongs to.

use super::{Adapter, ApiError, ErrKind, Event, HttpRequest, Parser, Provider, bad_event, brief, cut, error_object, kind_of_status};
use crate::conv::{Conversation, Message, Part, Raw, Role, Stop, ToolCall, Turn};
use crate::json::{self, Json};
use crate::net::url::encode_segment;
use crate::tools::ToolSpec;
use std::time::Duration;

pub struct Gemini;

/// Call ids this client makes up when the model sends none; they never go back to the provider.
const LOCAL_ID: &str = "gemini-call-";

fn parts(m: &Message, provider: &str) -> Json {
    if let Some(Json::Arr(raw)) = m.raw_for(provider)
        && !raw.is_empty()
    {
        return Json::Arr(raw.clone());
    }
    let mut out = Vec::new();
    for p in &m.parts {
        match p {
            Part::Text(t) if t.is_empty() => {}
            Part::Text(t) => out.push(Json::obj().with("text", t.as_str())),
            Part::Call(c) => {
                let f = Json::obj().with("name", c.name.as_str()).with("args", c.args_or_empty());
                out.push(Json::obj().with("functionCall", if c.id.starts_with(LOCAL_ID) { f } else { f.with("id", c.id.as_str()) }));
            }
            Part::Image(i) => out.push(Json::obj().with("inlineData", Json::obj().with("mimeType", i.media_type.as_str()).with("data", i.base64.as_str()))),
            Part::Result(r) => {
                let response = Json::obj().with(if r.is_error { "error" } else { "result" }, r.content.as_str());
                let f = Json::obj().with("name", r.name.as_str()).with("response", response);
                out.push(Json::obj().with("functionResponse", if r.id.starts_with(LOCAL_ID) { f } else { f.with("id", r.id.as_str()) }));
            }
        }
    }
    if out.is_empty() {
        out.push(Json::obj().with("text", "(leer)"));
    }
    Json::Arr(out)
}

impl Adapter for Gemini {
    fn request(&self, p: &Provider, conv: &Conversation, tools: &[ToolSpec]) -> HttpRequest {
        let contents: Vec<Json> = conv
            .messages
            .iter()
            .map(|m| Json::obj().with("role", if m.role == Role::User { "user" } else { "model" }).with("parts", parts(m, &p.id)))
            .collect();
        let mut body = Json::obj();
        if !conv.system.is_empty() {
            body = body.with("systemInstruction", Json::obj().with("parts", vec![Json::obj().with("text", conv.system.as_str())]));
        }
        body = body.with("contents", contents);
        if !tools.is_empty() {
            // parametersJsonSchema takes the tools' JSON Schema as it is (the older `parameters` field knows
            // only a subset and no list of types).
            let decls: Vec<Json> = tools
                .iter()
                .map(|t| Json::obj().with("name", t.name.as_str()).with("description", t.description.as_str()).with("parametersJsonSchema", t.schema.clone()))
                .collect();
            body = body.with("tools", vec![Json::obj().with("functionDeclarations", decls)]);
        }
        HttpRequest {
            path: format!("/models/{}:streamGenerateContent?alt=sse", encode_segment(&p.model)),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "text/event-stream".into()),
                ("x-goog-api-key".into(), p.key.expose().to_string()),
            ],
            body: body.to_text(),
        }
    }

    fn parser(&self, p: &Provider) -> Box<dyn Parser> {
        Box::new(Stream { provider: p.id.clone(), raw: Vec::new(), parts: Vec::new(), finish: None, blocked: None, calls: 0 })
    }

    fn error(&self, status: u16, body: &str) -> ApiError {
        match error_object(body) {
            Some(e) => api_error(&e, status),
            None => ApiError::http(kind_of_status(status), status, brief(body)),
        }
    }
}

fn api_error(e: &Json, http: u16) -> ApiError {
    let status = e.str("status").unwrap_or("");
    let message = e.str("message").unwrap_or("");
    let code = if http == 0 { e.get("code").and_then(Json::as_i64).and_then(|c| u16::try_from(c).ok()).unwrap_or(500) } else { http };
    let kind = match status {
        "UNAUTHENTICATED" => ErrKind::Key,
        // A wrong key is an invalid argument here, told apart only by its message.
        "INVALID_ARGUMENT" | "PERMISSION_DENIED" if message.contains("API key") => ErrKind::Key,
        "PERMISSION_DENIED" => ErrKind::Forbidden,
        "NOT_FOUND" => ErrKind::ModelNotFound,
        "RESOURCE_EXHAUSTED" => ErrKind::RateLimit,
        "UNAVAILABLE" => ErrKind::Overloaded,
        "INTERNAL" | "DEADLINE_EXCEEDED" => ErrKind::Server,
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" => ErrKind::BadRequest,
        _ => kind_of_status(code),
    };
    let mut err = ApiError::http(kind, code, format!("{status}: {}", brief(message)));
    // google.rpc.RetryInfo in the details: "retryDelay": "34s".
    if let Some(Json::Arr(details)) = e.get("details") {
        err.retry_after = details
            .iter()
            .filter_map(|d| d.str("retryDelay"))
            .filter_map(|s| s.strip_suffix('s')?.parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(Duration::from_secs_f64)
            .next();
    }
    err
}

struct Stream {
    provider: String,
    raw: Vec<Json>,
    parts: Vec<Part>,
    finish: Option<String>,
    blocked: Option<String>,
    calls: usize,
}

/// A part that is nothing but visible text: these are joined so the transcript does not grow one part per chunk.
fn plain_text(p: &Json) -> Option<&str> {
    match p {
        Json::Obj(fields) if fields.len() == 1 && fields[0].0 == "text" => fields[0].1.as_str(),
        _ => None,
    }
}

impl Parser for Stream {
    fn event(&mut self, _event: &str, data: &str, out: &mut dyn FnMut(Event)) -> Result<(), ApiError> {
        let j = json::parse(data).map_err(|_| bad_event(data))?;
        if let Some(e) = j.get("error") {
            return Err(api_error(e, 0));
        }
        if let Some(b) = j.get("promptFeedback").and_then(|f| f.str("blockReason")) {
            self.blocked = Some(b.to_string());
        }
        let Some(Json::Arr(cands)) = j.get("candidates") else { return Ok(()) };
        let Some(cand) = cands.first() else { return Ok(()) };
        if let Some(Json::Arr(ps)) = cand.get("content").and_then(|c| c.get("parts")) {
            for p in ps {
                if let Some(f) = p.get("functionCall") {
                    self.calls += 1;
                    let id = f.str("id").map_or_else(|| format!("{LOCAL_ID}{}", self.calls), str::to_string);
                    let args = f.get("args").map_or_else(|| "{}".to_string(), Json::to_text);
                    let c = ToolCall::new(id, f.str("name").unwrap_or("").to_string(), args);
                    out(Event::Call(&c));
                    self.parts.push(Part::Call(c));
                    self.raw.push(p.clone());
                    continue;
                }
                let thought = p.get("thought").and_then(Json::as_bool) == Some(true);
                if let Some(t) = p.str("text").filter(|t| !t.is_empty() && !thought) {
                    out(Event::Text(t));
                    match self.parts.last_mut() {
                        Some(Part::Text(prev)) => prev.push_str(t),
                        _ => self.parts.push(Part::Text(t.to_string())),
                    }
                }
                match (plain_text(p), self.raw.last_mut()) {
                    (Some(t), Some(Json::Obj(prev))) if prev.len() == 1 && prev[0].0 == "text" => {
                        if let Json::Str(s) = &mut prev[0].1 {
                            s.push_str(t);
                        }
                    }
                    _ => self.raw.push(p.clone()),
                }
            }
        }
        if let Some(f) = cand.str("finishReason") {
            self.finish = Some(f.to_string());
        }
        Ok(())
    }

    fn done(&self) -> bool {
        self.finish.is_some()
    }

    fn finish(self: Box<Self>) -> Result<Turn, ApiError> {
        let stop = match (self.finish.as_deref(), &self.blocked) {
            (None, Some(_)) => Stop::Refusal,
            (None, None) => return Err(cut()),
            (Some("MAX_TOKENS"), _) => Stop::MaxTokens,
            (Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY"), _) => Stop::Refusal,
            (Some("STOP"), _) if self.calls > 0 => Stop::ToolUse,
            (Some("STOP"), _) => Stop::End,
            (Some(other), _) => Stop::Other(other.to_string()),
        };
        let raw = Some(Raw { provider: self.provider, value: Json::Arr(self.raw) });
        Ok(Turn { message: Message { role: Role::Assistant, parts: self.parts, raw }, stop })
    }
}
