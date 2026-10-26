//! The line protocol on `/run/bitsos/mcp.sock` (MC-6): JSON-RPC 2.0, one message per line (`\n`), both ways.
//! It is a small part of MCP itself, so a service's author knows it already:
//!
//! service -> kernel: `register` {service, tools: [{name, description, inputSchema, safety}]}, `unregister`
//! {tools: [names]}, `ping`; the notification `notifications/progress` {progressToken, message} (text for
//! `output`). The kernel answers each request with `result` or `error` (-32602 names the first bad tool and why).
//! kernel -> service: `tools/call` {name, arguments, _meta: {progressToken, timeout_ms}} (id = progressToken),
//! answered with MCP's CallToolResult {content: [{type: text|image, ...}], isError}; the notification
//! `notifications/cancelled` {requestId, reason} when the call's deadline passed or `stop` asked.

use crate::rpc::{ANSWER_MAX, escaped_len};
use crate::{Json, Safety, ToolError, ToolOutput, ToolResult, b64, json};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Longest line taken from a service (a whole 4 MB answer, base64 pictures and escapes included). A longer
/// one ends the service's connection: the stream cannot be resynchronised.
pub const MAX_LINE: usize = 2 * ANSWER_MAX;
/// Tool names: MCP's usual alphabet; the length a client shows comfortably.
pub const TOOL_NAME_MAX: usize = 64;
pub const SERVICE_NAME_MAX: usize = 32;
/// Bounds against a hostile service on one tool's texts (bytes, not counts).
pub const DESC_MAX: usize = 16 * 1024;
pub const SCHEMA_MAX: usize = 64 * 1024;
/// All services' tools together take at most this much of `tools/list`'s answer (bytes): the rest of
/// `ANSWER_MAX` stays for the kernel's own tools, so the lifeline's list always fits.
pub const LIST_BUDGET: usize = ANSWER_MAX / 2;
/// Room for a listed tool's fixed parts (keys, annotations, owner, pid).
const LIST_FIXED: usize = 256;
/// MC-7 lists program actions as `app.<name>.<action>`; services do not take names there.
pub const RESERVED_PREFIX: &str = "app.";
/// Service and tool names only services of uid 0 take (settingsd's `settings_*`, started by init) until PM-1
/// decides who may register what (B3): no other program can squat them before settingsd starts.
pub const ROOT_PREFIX: &str = "settings";

pub fn root_only(name: &str) -> bool {
    name.starts_with(ROOT_PREFIX)
}

/// A tool a service asks to register, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTool {
    pub name: String,
    pub description: String,
    /// The schema as the kernel lists it (its own serialisation of the parsed schema).
    pub schema: String,
    pub safety: Safety,
    /// Bytes it adds to `tools/list`.
    pub listed: usize,
}

fn printable(s: &str) -> String {
    s.chars().take(TOOL_NAME_MAX).map(|c| if c.is_control() { '?' } else { c }).collect()
}

pub fn service_name_ok(s: &str) -> Result<(), String> {
    let first = s.bytes().next();
    if s.is_empty() || s.len() > SERVICE_NAME_MAX || !first.is_some_and(|b| b.is_ascii_lowercase()) || !s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') {
        return Err(format!("service name \"{}\": 1 to {SERVICE_NAME_MAX} characters a-z, 0-9, _ and -, starting with a letter", printable(s)));
    }
    Ok(())
}

pub fn tool_name_ok(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > TOOL_NAME_MAX || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.') {
        return Err(format!("name \"{}\": 1 to {TOOL_NAME_MAX} characters A-Z, a-z, 0-9, _, - and .", printable(s)));
    }
    if s.starts_with(RESERVED_PREFIX) {
        return Err(format!("name \"{s}\": names starting with {RESERVED_PREFIX} are program actions (MC-7)"));
    }
    Ok(())
}

pub fn safety_of(s: &str) -> Option<Safety> {
    match s {
        "read" => Some(Safety::Read),
        "change" => Some(Safety::Change),
        "delete_overwrite" => Some(Safety::DeleteOverwrite),
        _ => None,
    }
}

/// An argument schema the kernel can list and check: an object of `type` "object", `properties` an object of
/// objects, `required` an array of strings (MCP's `inputSchema`).
pub fn check_schema(s: &Json) -> Result<(), String> {
    let Json::Obj(_) = s else { return Err(String::from("inputSchema must be an object")) };
    if s.get("type").and_then(Json::as_str) != Some("object") {
        return Err(String::from("inputSchema: type must be \"object\""));
    }
    match s.get("properties") {
        None => {}
        Some(Json::Obj(props)) => {
            if let Some((k, _)) = props.iter().find(|(_, v)| !matches!(v, Json::Obj(_))) {
                return Err(format!("inputSchema: property \"{}\" must be an object", printable(k)));
            }
        }
        Some(_) => return Err(String::from("inputSchema: properties must be an object")),
    }
    match s.get("required") {
        None => {}
        Some(Json::Arr(a)) if a.iter().all(|v| v.as_str().is_some()) => {}
        Some(_) => return Err(String::from("inputSchema: required must be an array of strings")),
    }
    Ok(())
}

fn text_ok(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
}

fn escaped(s: &str) -> usize {
    s.chars().map(escaped_len).sum::<usize>() + 2
}

/// One entry of `register`'s `tools`.
pub fn parse_tool(t: &Json) -> Result<NewTool, String> {
    let name = t.get("name").and_then(Json::as_str).ok_or("a tool without a name (string)")?;
    tool_name_ok(name)?;
    let fail = |why: &str| format!("tool {name}: {why}");
    let description = match t.get("description") {
        None | Some(Json::Null) => "",
        Some(Json::Str(d)) => d,
        Some(_) => return Err(fail("description must be a string")),
    };
    if description.len() > DESC_MAX || !text_ok(description) {
        return Err(fail(&format!("description: at most {DESC_MAX} bytes, no control characters but newline and tab")));
    }
    let schema = t.get("inputSchema").ok_or_else(|| fail("no inputSchema"))?;
    check_schema(schema).map_err(|e| fail(&e))?;
    let schema = schema.to_string();
    if schema.len() > SCHEMA_MAX {
        return Err(fail(&format!("inputSchema longer than {SCHEMA_MAX} bytes")));
    }
    let safety = t.get("safety").and_then(Json::as_str).and_then(safety_of).ok_or_else(|| fail("safety must be \"read\", \"change\" or \"delete_overwrite\" (B5)"))?;
    let listed = escaped(name) + escaped(description) + schema.len() + LIST_FIXED;
    Ok(NewTool { name: String::from(name), description: String::from(description), schema, safety, listed })
}

/// `register`'s `tools`: every one valid and no name twice, or the first problem.
pub fn parse_tools(params: &Json) -> Result<Vec<NewTool>, String> {
    let list = match params.get("tools") {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Arr(a)) => a,
        Some(_) => return Err(String::from("tools must be an array")),
    };
    let mut out: Vec<NewTool> = Vec::new();
    for (i, t) in list.iter().enumerate() {
        let n = parse_tool(t).map_err(|e| format!("tools[{i}]: {e}"))?;
        if out.iter().any(|o| o.name == n.name) {
            return Err(format!("tools[{i}]: {} is named twice", n.name));
        }
        out.push(n);
    }
    Ok(out)
}

/// A service's answer to `tools/call` as the tool's result. `who` names the service in errors.
pub fn result_of(msg: &Json, who: &str) -> ToolResult {
    if let Some(e) = msg.get("error") {
        let text = e.get("message").and_then(Json::as_str).unwrap_or("no message");
        let text: String = text.chars().take(4096).collect();
        return Err(match e.get("code").and_then(Json::as_i64) {
            Some(-32602) => ToolError::BadArgument(text),
            _ => ToolError::Failed(format!("{who}: {text}")),
        });
    }
    let bad = |why: &str| Err(ToolError::Failed(format!("{who} answered with {why}")));
    let Some(r @ Json::Obj(_)) = msg.get("result") else { return bad("no result object") };
    let Some(content) = r.get("content").and_then(Json::as_array) else { return bad("no content array") };
    let mut out = Vec::new();
    for c in content {
        match c.get("type").and_then(Json::as_str) {
            Some("text") => match c.get("text").and_then(Json::as_str) {
                Some(t) => out.push(ToolOutput::Text(String::from(t))),
                None => return bad("a text item without text"),
            },
            Some("image") => {
                if c.get("mimeType").and_then(Json::as_str) != Some("image/png") {
                    return bad("an image that is not image/png");
                }
                match c.get("data").and_then(Json::as_str).and_then(b64::decode) {
                    Some(png) => out.push(ToolOutput::Png(png)),
                    None => return bad("an image without base64 data"),
                }
            }
            // Other MCP content types (audio, resources) pass as their JSON text.
            Some(_) => out.push(ToolOutput::Text(c.to_string())),
            None => return bad("a content item without type"),
        }
    }
    if r.get("isError").and_then(Json::as_bool) == Some(true) {
        let text: Vec<&str> = out.iter().filter_map(|o| if let ToolOutput::Text(t) = o { Some(t.as_str()) } else { None }).collect();
        return Err(ToolError::Failed(text.join("\n")));
    }
    Ok(out)
}

/// The kernel's `tools/call` to a service.
pub fn call_line(id: u64, tool: &str, args: &Json, timeout_ms: u64) -> String {
    let mut s = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":");
    json::escape_into(tool, &mut s);
    s.push_str(",\"arguments\":");
    args.write_to(&mut s);
    s.push_str(&format!(",\"_meta\":{{\"progressToken\":{id},\"timeout_ms\":{timeout_ms}}}}}}}\n"));
    s
}

pub fn cancel_line(id: u64, reason: &str) -> String {
    let mut s = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{{\"requestId\":{id},\"reason\":");
    json::escape_into(reason, &mut s);
    s.push_str("}}\n");
    s
}

/// An answer to a service's request; `id` is its raw JSON text (sent back byte for byte).
pub fn ok_line(id: &str, result: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}\n")
}

pub fn err_line(id: &str, code: i32, msg: &str) -> String {
    let mut s = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":");
    json::escape_into(msg, &mut s);
    s.push_str("}}\n");
    s
}

/// `{"tools":["a","b"]}` for register/unregister answers.
pub fn names_result(key: &str, names: &[String]) -> String {
    let mut s = format!("{{\"{key}\":[");
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        json::escape_into(n, &mut s);
    }
    s.push_str("]}");
    s
}

/// Every printable form of an untrusted short string for events and errors.
pub fn shown(s: &str) -> String {
    printable(s).to_string()
}

#[cfg(test)]
mod tests;
