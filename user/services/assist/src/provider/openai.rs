//! OpenAI chat completions with streaming and tool calls. The same adapter serves `openai_compatible`
//! (llama-server, Ollama, LM Studio, Strata), so it reads leniently: a missing call id gets one, a stream that
//! closes after its `finish_reason` without `[DONE]` is complete, `finish_reason: "stop"` with tool calls is a
//! tool turn. Arguments arrive as pieces of one JSON text per call, keyed by `index`.

use super::{Adapter, ApiError, ErrKind, Event, HttpRequest, Parser, Provider, bad_event, brief, cut, error_object, kind_of_status};
use crate::conv::{Conversation, Message, Part, Role, Stop, ToolCall, Turn};
use crate::json::{self, Json};
use crate::tools::ToolSpec;
use std::collections::BTreeMap;

pub struct OpenAi;

fn push_message(out: &mut Vec<Json>, m: &Message) {
    let text = m.text();
    if m.role == Role::Assistant {
        let calls: Vec<Json> = m
            .calls()
            .iter()
            .map(|c| {
                let f = Json::obj().with("name", c.name.as_str()).with("arguments", c.args_or_empty().to_text());
                Json::obj().with("id", c.id.as_str()).with("type", "function").with("function", f)
            })
            .collect();
        // null is this interface's "no text" beside tool calls; a turn with neither must still carry a string.
        let content = if text.is_empty() && !calls.is_empty() { Json::Null } else { Json::from(text) };
        let mut j = Json::obj().with("role", "assistant").with("content", content);
        if !calls.is_empty() {
            j = j.with("tool_calls", calls);
        }
        out.push(j);
        return;
    }
    // Tool results are messages of their own here, before any text of the same turn.
    for p in &m.parts {
        if let Part::Result(r) = p {
            let content = if r.is_error { format!("ERROR: {}", r.content) } else { r.content.clone() };
            out.push(Json::obj().with("role", "tool").with("tool_call_id", r.id.as_str()).with("content", content));
        }
    }
    let images: Vec<Json> = m
        .parts
        .iter()
        .filter_map(|p| if let Part::Image(i) = p { Some(i) } else { None })
        .map(|i| Json::obj().with("type", "image_url").with("image_url", Json::obj().with("url", format!("data:{};base64,{}", i.media_type, i.base64))))
        .collect();
    if !images.is_empty() {
        // A picture makes the content a list of parts: the text first, then the pictures.
        let mut parts = vec![Json::obj().with("type", "text").with("text", text)];
        parts.extend(images);
        out.push(Json::obj().with("role", "user").with("content", parts));
    } else if !text.is_empty() {
        out.push(Json::obj().with("role", "user").with("content", text));
    }
}

impl Adapter for OpenAi {
    fn request(&self, p: &Provider, conv: &Conversation, tools: &[ToolSpec]) -> HttpRequest {
        let mut messages = Vec::new();
        if !conv.system.is_empty() {
            messages.push(Json::obj().with("role", "system").with("content", conv.system.as_str()));
        }
        for m in &conv.messages {
            push_message(&mut messages, m);
        }
        let mut body = Json::obj().with("model", p.model.as_str()).with("stream", true).with("messages", messages);
        if !tools.is_empty() {
            let list: Vec<Json> = tools
                .iter()
                .map(|t| {
                    let f = Json::obj().with("name", t.name.as_str()).with("description", t.description.as_str()).with("parameters", t.schema.clone());
                    Json::obj().with("type", "function").with("function", f)
                })
                .collect();
            body = body.with("tools", list);
        }
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string()), ("Accept".to_string(), "text/event-stream".to_string())];
        // A local engine usually has no key; an empty bearer would be refused by some.
        if !p.key.is_empty() {
            headers.push(("Authorization".into(), format!("Bearer {}", p.key.expose())));
        }
        HttpRequest { path: "/chat/completions".into(), headers, body: body.to_text() }
    }

    fn parser(&self, _p: &Provider) -> Box<dyn Parser> {
        Box::new(Stream { text: String::new(), calls: BTreeMap::new(), finish: None, done: false, emitted: false })
    }

    fn error(&self, status: u16, body: &str) -> ApiError {
        match error_object(body) {
            Some(e) => ApiError::http(kind_of(&e, status), status, raw(&e)),
            None => ApiError::http(kind_of_status(status), status, brief(body)),
        }
    }
}

fn field(e: &Json, name: &str) -> String {
    match e.get(name) {
        Some(Json::Str(s)) => s.clone(),
        Some(Json::Int(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn raw(e: &Json) -> String {
    match e {
        Json::Str(s) => brief(s),
        _ => format!("{} {}: {}", field(e, "type"), field(e, "code"), brief(&field(e, "message"))).trim().to_string(),
    }
}

fn kind_of(e: &Json, status: u16) -> ErrKind {
    match (field(e, "code").as_str(), field(e, "type").as_str()) {
        ("invalid_api_key", _) | (_, "authentication_error") => ErrKind::Key,
        ("insufficient_quota", _) | (_, "insufficient_quota") => ErrKind::Billing,
        ("model_not_found", _) => ErrKind::ModelNotFound,
        ("context_length_exceeded", _) => ErrKind::TooLarge,
        ("rate_limit_exceeded", _) => ErrKind::RateLimit,
        _ => kind_of_status(status),
    }
}

#[derive(Default)]
struct Pending {
    id: String,
    name: String,
    args: String,
}

struct Stream {
    text: String,
    calls: BTreeMap<i64, Pending>,
    finish: Option<String>,
    done: bool,
    emitted: bool,
}

impl Stream {
    /// The place of a call piece that came without `index` (some openai_compatible engines send each call
    /// whole and unnumbered): a piece with another id than the newest call's, or with a name when that call
    /// has one already, starts a new call; anything else continues the newest.
    fn unnumbered(&self, piece: &Json) -> i64 {
        let Some((&last, p)) = self.calls.iter().next_back() else { return 0 };
        let id = piece.str("id").filter(|s| !s.is_empty());
        let name = piece.get("function").and_then(|f| f.str("name")).filter(|s| !s.is_empty());
        let new_id = id.is_some_and(|id| !p.id.is_empty() && id != p.id);
        let new_name = name.is_some() && !p.name.is_empty();
        if new_id || new_name { last.saturating_add(1) } else { last }
    }

    fn tool_calls(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .map(|(i, p)| {
                let id = if p.id.is_empty() { format!("call_{i}") } else { p.id.clone() };
                ToolCall::new(id, p.name.clone(), p.args.clone())
            })
            .collect()
    }
}

impl Parser for Stream {
    fn event(&mut self, _event: &str, data: &str, out: &mut dyn FnMut(Event)) -> Result<(), ApiError> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let j = json::parse(data).map_err(|_| bad_event(data))?;
        if let Some(e) = j.get("error") {
            let status = e.get("code").and_then(Json::as_i64).and_then(|c| u16::try_from(c).ok()).unwrap_or(0);
            return Err(ApiError::new(if status == 0 { kind_of(e, 500) } else { kind_of(e, status) }, raw(e)));
        }
        let Some(Json::Arr(choices)) = j.get("choices") else { return Ok(()) };
        let Some(choice) = choices.first() else { return Ok(()) };
        if let Some(d) = choice.get("delta") {
            if let Some(t) = d.str("content")
                && !t.is_empty()
            {
                self.text.push_str(t);
                out(Event::Text(t));
            }
            if let Some(Json::Arr(calls)) = d.get("tool_calls") {
                for c in calls {
                    let index = c.get("index").and_then(Json::as_i64).unwrap_or_else(|| self.unnumbered(c));
                    let p = self.calls.entry(index).or_default();
                    if let Some(id) = c.str("id").filter(|s| !s.is_empty()) {
                        p.id = id.to_string();
                    }
                    if let Some(f) = c.get("function") {
                        if let Some(name) = f.str("name").filter(|s| !s.is_empty()) {
                            p.name = name.to_string();
                        }
                        p.args.push_str(f.str("arguments").unwrap_or(""));
                    }
                }
            }
        }
        if let Some(f) = choice.str("finish_reason") {
            self.finish = Some(f.to_string());
            // This interface has no end mark per call: the finish reason is the first moment they are whole.
            if !self.emitted {
                self.emitted = true;
                for c in self.tool_calls() {
                    out(Event::Call(&c));
                }
            }
        }
        Ok(())
    }

    fn done(&self) -> bool {
        self.done
    }

    fn finish(self: Box<Self>) -> Result<Turn, ApiError> {
        let Some(finish) = self.finish.clone() else { return Err(cut()) };
        let calls = self.tool_calls();
        let stop = match finish.as_str() {
            "length" => Stop::MaxTokens,
            "content_filter" => Stop::Refusal,
            _ if !calls.is_empty() => Stop::ToolUse,
            "stop" => Stop::End,
            other => Stop::Other(other.to_string()),
        };
        let mut parts = Vec::new();
        if !self.text.is_empty() {
            parts.push(Part::Text(self.text));
        }
        parts.extend(calls.into_iter().map(Part::Call));
        Ok(Turn { message: Message { role: Role::Assistant, parts, raw: None }, stop })
    }
}
