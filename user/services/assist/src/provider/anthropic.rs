//! The Anthropic Messages API, streaming (written after the `claude-api` reference of 2026-11: raw HTTP,
//! `anthropic-version: 2026-10-21`). What matters beyond the obvious:
//! - no sampling parameters and no `thinking` field: current models think adaptively by default and refuse the
//!   old switches, and the model is Georg's setting, not this file's;
//! - the answer's content blocks go back unchanged in the next request (`Message::raw`): reasoning blocks carry
//!   a signature bound to the conversation and may not be rebuilt or dropped;
//! - a tool input arrives as `input_json_delta` pieces and is parsed strictly at `content_block_stop`;
//! - `stop_reason` `max_tokens` and `refusal` can cut a tool call short, so the engine never runs tools then.

use super::{Adapter, ApiError, ErrKind, Event, HttpRequest, Parser, Provider, bad_event, brief, cut, error_object, kind_of_status};
use crate::conv::{Conversation, Message, Part, Raw, Role, Stop, ToolCall, Turn};
use crate::json::{self, Json};
use crate::net::url::Url;
use crate::tools::ToolSpec;
use std::collections::BTreeMap;

pub const VERSION: &str = "2026-10-21";
/// The vendor's own endpoint. Only compared, never contacted from here: tool inputs stream eagerly there, and
/// stay buffered behind any other address (a gateway may not know the field).
const OWN_HOST: &str = "api.anthropic.com";

pub struct Anthropic;

fn text_block(t: &str) -> Json {
    Json::obj().with("type", "text").with("text", t)
}

fn content(m: &Message, provider: &str) -> Json {
    if let Some(Json::Arr(raw)) = m.raw_for(provider)
        && !raw.is_empty()
    {
        return Json::Arr(raw.clone());
    }
    let mut blocks = Vec::new();
    for p in &m.parts {
        match p {
            // The API refuses empty text blocks.
            Part::Text(t) if t.is_empty() => {}
            Part::Text(t) => blocks.push(text_block(t)),
            Part::Call(c) => blocks.push(
                Json::obj().with("type", "tool_use").with("id", c.id.as_str()).with("name", c.name.as_str()).with("input", c.args_or_empty()),
            ),
            Part::Image(i) => {
                let src = Json::obj().with("type", "base64").with("media_type", i.media_type.as_str()).with("data", i.base64.as_str());
                blocks.push(Json::obj().with("type", "image").with("source", src));
            }
            Part::Result(r) => {
                let b = Json::obj().with("type", "tool_result").with("tool_use_id", r.id.as_str()).with("content", r.content.as_str());
                blocks.push(if r.is_error { b.with("is_error", true) } else { b });
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(text_block("(leer)"));
    }
    Json::Arr(blocks)
}

impl Adapter for Anthropic {
    fn request(&self, p: &Provider, conv: &Conversation, tools: &[ToolSpec]) -> HttpRequest {
        let eager = Url::parse(&p.address).is_ok_and(|u| u.https && u.host == OWN_HOST);
        let messages: Vec<Json> = conv
            .messages
            .iter()
            .map(|m| {
                let role = if m.role == Role::User { "user" } else { "assistant" };
                Json::obj().with("role", role).with("content", content(m, &p.id))
            })
            .collect();
        let mut body = Json::obj().with("model", p.model.as_str()).with("max_tokens", p.max_tokens).with("stream", true);
        if !conv.system.is_empty() {
            body = body.with("system", conv.system.as_str());
        }
        if !tools.is_empty() {
            let list: Vec<Json> = tools
                .iter()
                .map(|t| {
                    let j = Json::obj().with("name", t.name.as_str()).with("description", t.description.as_str());
                    let j = if eager { j.with("eager_input_streaming", true) } else { j };
                    j.with("input_schema", t.schema.clone())
                })
                .collect();
            body = body.with("tools", list);
        }
        HttpRequest {
            path: "/v1/messages".into(),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "text/event-stream".into()),
                ("x-api-key".into(), p.key.expose().to_string()),
                ("anthropic-version".into(), VERSION.into()),
            ],
            body: body.with("messages", messages).to_text(),
        }
    }

    fn parser(&self, p: &Provider) -> Box<dyn Parser> {
        Box::new(Stream { provider: p.id.clone(), blocks: BTreeMap::new(), stop: None, done: false })
    }

    fn error(&self, status: u16, body: &str) -> ApiError {
        match error_object(body) {
            Some(e) => {
                let ty = e.str("type").unwrap_or("");
                let raw = format!("{ty}: {}", brief(e.str("message").unwrap_or("")));
                ApiError::http(kind_of_type(ty).unwrap_or_else(|| kind_of_status(status)), status, raw)
            }
            None => ApiError::http(kind_of_status(status), status, brief(body)),
        }
    }
}

fn kind_of_type(ty: &str) -> Option<ErrKind> {
    Some(match ty {
        "authentication_error" => ErrKind::Key,
        "permission_error" => ErrKind::Forbidden,
        "billing_error" => ErrKind::Billing,
        "not_found_error" => ErrKind::ModelNotFound,
        "request_too_large" => ErrKind::TooLarge,
        "rate_limit_error" => ErrKind::RateLimit,
        "overloaded_error" => ErrKind::Overloaded,
        "api_error" | "timeout_error" => ErrKind::Server,
        "invalid_request_error" => ErrKind::BadRequest,
        _ => return None,
    })
}

enum Block {
    Text(String),
    Tool { id: String, name: String, input: String, call: Option<ToolCall> },
    Thinking { start: Json, thinking: String, signature: String },
    /// A block kind this client does not read; it goes back as it came.
    Other(Json),
}

struct Stream {
    provider: String,
    blocks: BTreeMap<i64, Block>,
    stop: Option<String>,
    done: bool,
}

impl Parser for Stream {
    fn event(&mut self, _event: &str, data: &str, out: &mut dyn FnMut(Event)) -> Result<(), ApiError> {
        let j = json::parse(data).map_err(|_| bad_event(data))?;
        let index = j.get("index").and_then(Json::as_i64).unwrap_or(-1);
        match j.str("type").unwrap_or("") {
            "content_block_start" => {
                let b = j.get("content_block").cloned().unwrap_or_else(Json::obj);
                let block = match b.str("type").unwrap_or("") {
                    "text" => {
                        let t = b.str("text").unwrap_or("").to_string();
                        if !t.is_empty() {
                            out(Event::Text(&t));
                        }
                        Block::Text(t)
                    }
                    "tool_use" => Block::Tool {
                        id: b.str("id").unwrap_or("").to_string(),
                        name: b.str("name").unwrap_or("").to_string(),
                        input: String::new(),
                        call: None,
                    },
                    "thinking" => Block::Thinking {
                        thinking: b.str("thinking").unwrap_or("").to_string(),
                        signature: b.str("signature").unwrap_or("").to_string(),
                        start: b,
                    },
                    _ => Block::Other(b),
                };
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let d = j.get("delta").cloned().unwrap_or_else(Json::obj);
                match (self.blocks.get_mut(&index), d.str("type").unwrap_or("")) {
                    (Some(Block::Text(t)), "text_delta") => {
                        let piece = d.str("text").unwrap_or("");
                        t.push_str(piece);
                        out(Event::Text(piece));
                    }
                    (Some(Block::Tool { input, .. }), "input_json_delta") => input.push_str(d.str("partial_json").unwrap_or("")),
                    (Some(Block::Thinking { thinking, .. }), "thinking_delta") => thinking.push_str(d.str("thinking").unwrap_or("")),
                    (Some(Block::Thinking { signature, .. }), "signature_delta") => signature.push_str(d.str("signature").unwrap_or("")),
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(Block::Tool { id, name, input, call }) = self.blocks.get_mut(&index) {
                    let c = ToolCall::new(id.clone(), name.clone(), input.clone());
                    out(Event::Call(&c));
                    *call = Some(c);
                }
            }
            "message_delta" => {
                if let Some(s) = j.get("delta").and_then(|d| d.str("stop_reason")) {
                    self.stop = Some(s.to_string());
                }
            }
            "message_stop" => self.done = true,
            "error" => {
                let e = j.get("error").cloned().unwrap_or_else(Json::obj);
                let ty = e.str("type").unwrap_or("");
                return Err(ApiError::new(kind_of_type(ty).unwrap_or(ErrKind::Server), format!("{ty}: {}", brief(e.str("message").unwrap_or("")))));
            }
            // message_start, ping and events added later.
            _ => {}
        }
        Ok(())
    }

    fn done(&self) -> bool {
        self.done
    }

    fn finish(self: Box<Self>) -> Result<Turn, ApiError> {
        if !self.done {
            return Err(cut());
        }
        let (mut parts, mut raw) = (Vec::new(), Vec::new());
        for block in self.blocks.into_values() {
            match block {
                Block::Text(t) if t.is_empty() => {}
                Block::Text(t) => {
                    raw.push(text_block(&t));
                    parts.push(Part::Text(t));
                }
                Block::Tool { id, name, input, call } => {
                    // A block without its stop event (max_tokens, refusal) is kept as what arrived.
                    let c = call.unwrap_or_else(|| ToolCall::new(id, name, input));
                    raw.push(Json::obj().with("type", "tool_use").with("id", c.id.as_str()).with("name", c.name.as_str()).with("input", c.args_or_empty()));
                    parts.push(Part::Call(c));
                }
                // Without its signature a reasoning block cannot be sent back (the stream stopped inside it).
                Block::Thinking { signature, .. } if signature.is_empty() => {}
                Block::Thinking { start, thinking, signature } => {
                    let Json::Obj(mut fields) = start else { continue };
                    fields.retain(|(k, _)| k != "thinking" && k != "signature");
                    raw.push(Json::Obj(fields).with("thinking", thinking).with("signature", signature));
                }
                Block::Other(b) => raw.push(b),
            }
        }
        let stop = match self.stop.as_deref() {
            Some("end_turn" | "stop_sequence") => Stop::End,
            Some("tool_use") => Stop::ToolUse,
            Some("max_tokens") => Stop::MaxTokens,
            Some("refusal") => Stop::Refusal,
            Some(other) => Stop::Other(other.to_string()),
            None => Stop::Other("no stop reason".into()),
        };
        let raw = Some(Raw { provider: self.provider, value: Json::Arr(raw) });
        Ok(Turn { message: Message { role: Role::Assistant, parts, raw }, stop })
    }
}
