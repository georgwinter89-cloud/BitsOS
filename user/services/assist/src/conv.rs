//! A conversation in the form every adapter reads: text, tool calls and tool results. An assistant message also
//! keeps the provider's own form of itself (`raw`): some providers bind hidden parts (reasoning blocks with a
//! signature) to the exact message they sent, so the same provider gets it back unchanged.

use crate::json::Json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The argument text exactly as the model wrote it.
    pub args_text: String,
    /// The arguments when the text is a JSON object; None is reported back to the model, never run.
    pub args: Option<Json>,
}

impl ToolCall {
    pub fn new(id: String, name: String, args_text: String) -> ToolCall {
        let t = args_text.trim();
        let args = match crate::json::parse(if t.is_empty() { "{}" } else { t }) {
            Ok(j @ Json::Obj(_)) => Some(j),
            _ => None,
        };
        ToolCall { id, name, args_text, args }
    }
    /// What a request carries for this call: the arguments, or an empty object when they were not valid.
    pub fn args_or_empty(&self) -> Json {
        self.args.clone().unwrap_or_else(Json::obj)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

/// A picture in a question (DB3, Durchblick): its media type and its bytes in base64, as every provider takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub media_type: String,
    pub base64: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    Call(ToolCall),
    Result(ToolResult),
    Image(Image),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Raw {
    /// The provider id (`ai.provider.<id>`) that sent it; no other provider gets it.
    pub provider: String,
    pub value: Json,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    pub raw: Option<Raw>,
}

impl Message {
    pub fn user(text: &str) -> Message {
        Message { role: Role::User, parts: vec![Part::Text(text.to_string())], raw: None }
    }
    /// A question with a picture after its text.
    pub fn user_with(text: &str, image: Option<Image>) -> Message {
        let mut m = Message::user(text);
        m.parts.extend(image.map(Part::Image));
        m
    }
    pub fn results(results: Vec<ToolResult>) -> Message {
        Message { role: Role::User, parts: results.into_iter().map(Part::Result).collect(), raw: None }
    }
    pub fn text(&self) -> String {
        self.parts.iter().filter_map(|p| if let Part::Text(t) = p { Some(t.as_str()) } else { None }).collect()
    }
    pub fn calls(&self) -> Vec<&ToolCall> {
        self.parts.iter().filter_map(|p| if let Part::Call(c) = p { Some(c) } else { None }).collect()
    }
    /// The provider's own form, when `provider` is the one that sent this message.
    pub fn raw_for(&self, provider: &str) -> Option<&Json> {
        self.raw.as_ref().filter(|r| r.provider == provider).map(|r| &r.value)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Conversation {
    pub system: String,
    pub messages: Vec<Message>,
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    End,
    ToolUse,
    /// The answer hit the length limit; tool calls in it are incomplete and never run.
    MaxTokens,
    /// The provider declined to answer.
    Refusal,
    Other(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub message: Message,
    pub stop: Stop,
}
