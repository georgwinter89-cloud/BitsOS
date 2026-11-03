//! JSON-RPC 2.0 and the MCP methods, V1's rules (`mcp/rpc*.asm`): one message per body (no batches), an id
//! goes back byte for byte, protocol 2025-06-18 is answered whatever the client asks (newer versions are
//! accepted, the client decides), `initialize`, `notifications/*` (202), `ping`, `tools/list`, `tools/call`.
//!
//! Generic over the tool set (`ToolSet`) and what the host does around a call (`Host`: events, the policy
//! hook), so the kernel's server and the user-space service `bitsmcp` build the same answers from one code.

use crate::{Json, Safety, ToolError, ToolOutput, ToolResult, b64, json, schema};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Answers longer than this are replaced by an error result (V1: 4 MB for answers).
pub const ANSWER_MAX: usize = 4 << 20;
pub const PROTOCOL: &str = "2025-06-18";
/// A call's deadline when it brings no `timeout_ms` (V1: 10 s).
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const MAX_TIMEOUT_MS: u64 = 86_400_000;
/// Longest id kept, as JSON text (V1's `RPC_ID_MAX`; a bound against hostile input).
const ID_MAX: usize = 256;
/// Names kept for the log and error messages.
const NAME_MAX: usize = 64;

/// One tool as `tools/list` and `tools/call` see it.
pub trait ToolMeta {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// The argument schema as JSON text.
    fn input_schema(&self) -> &str;
    /// The lane (built-in) or the service's name (B5).
    fn owner(&self) -> &str;
    fn safety(&self) -> Safety;
    /// The pid of the service behind a registered tool.
    fn service_pid(&self) -> Option<u32>;
    /// Whether the listed safety class is vouched for: built-in tools and services of uid 0 (MC-6).
    fn safety_verified(&self) -> bool;
    /// Registered by a user-space service (its schema was checked and serialised by the registry).
    fn is_service(&self) -> bool;
}

/// The tools a server offers.
pub trait ToolSet {
    type Tool: ToolMeta;
    fn find(&self, name: &str) -> Option<Self::Tool>;
    /// Every tool, sorted by name.
    fn sorted(&self) -> Vec<Self::Tool>;
}

/// What the host does around the protocol. The defaults do nothing and refuse nothing.
pub trait Host {
    /// A tool's schema is not JSON (`tools/list` lists it as `{"type":"object"}`).
    fn bad_schema(&self, _tool: &str, _owner: &str) {}
    /// An answer was cut to `ANSWER_MAX`.
    fn answer_cut(&self, _bytes: usize, _limit: usize) {}
    /// The policy hook before a call runs: Err(reason) refuses it ("Refused: reason").
    fn check(&self, _tool: &str) -> Result<(), String> {
        Ok(())
    }
    /// A tool answered with `isError: true`.
    fn tool_failed(&self, _tool: &str) {}
}

/// What `initialize` says about the server.
#[derive(Clone, Copy, Debug)]
pub struct Info<'a> {
    pub version: &'a str,
    pub instructions: &'a str,
}

/// What one request body is answered with.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub status: u16,
    /// Empty for 202.
    pub body: String,
    /// The method and tool for the log line ("" when there is none).
    pub method: String,
    pub tool: String,
    /// A tool result with `isError: true`.
    pub tool_failed: bool,
}

fn printable(s: &str) -> String {
    s.chars().take(NAME_MAX).map(|c| if c.is_control() { '?' } else { c }).collect()
}

fn envelope(id: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},")
}

fn fail(status: u16, id: &str, code: i32, msg: &str) -> Answer {
    let mut b = envelope(id);
    b.push_str(&format!("\"error\":{{\"code\":{code},\"message\":"));
    json::escape_into(msg, &mut b);
    b.push_str("}}");
    Answer { status, body: b, method: String::new(), tool: String::new(), tool_failed: false }
}

fn result(id: &str, value: &str) -> Answer {
    let mut b = envelope(id);
    b.push_str("\"result\":");
    b.push_str(value);
    b.push('}');
    Answer { status: 200, body: b, method: String::new(), tool: String::new(), tool_failed: false }
}

fn accepted() -> Answer {
    Answer { status: 202, body: String::new(), method: String::new(), tool: String::new(), tool_failed: false }
}

/// An id is a string (at most `ID_MAX` bytes as JSON) or an integer from -2^63 to 2^63-1, written plainly.
pub fn id_ok(raw: &str) -> bool {
    if raw.starts_with('"') {
        return raw.len() <= ID_MAX;
    }
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let plain = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && (digits == "0" || !digits.starts_with('0'));
    plain && raw.parse::<i128>().is_ok_and(|n| n >= i64::MIN as i128 && n <= i64::MAX as i128)
}

/// Runs a tool for `tools/call` until the call's absolute deadline (ns, the second argument). `Err` is the text
/// of an `isError` result (e.g. "Still running ...").
pub type Runner<'a, T> = dyn FnMut(&T, u64, &Json) -> Result<ToolResult, String> + 'a;

/// Handles one request body. `now_ns` starts a call's deadline; `run` runs a tool.
pub fn handle_with<S: ToolSet>(body: &[u8], now_ns: u64, tools: &S, host: &dyn Host, info: &Info<'_>, run: &mut Runner<'_, S::Tool>) -> Answer {
    let parse_error = || fail(400, "null", -32700, "Parse error");
    let Ok(text) = core::str::from_utf8(body) else { return parse_error() };
    let Ok((msg, members)) = Json::parse_with_members(text) else { return parse_error() };
    match &msg {
        Json::Arr(_) => return fail(400, "null", -32600, "Batches are not supported"),
        Json::Obj(_) => {}
        _ => return fail(400, "null", -32600, "Invalid request: not an object"),
    }
    let Some(method) = msg.get("method") else {
        if msg.get("result").is_some() || msg.get("error").is_some() {
            return accepted();
        }
        return fail(400, "null", -32600, "Invalid request: no method");
    };
    let valid = || -> Result<&str, &'static str> {
        if msg.get("jsonrpc").and_then(Json::as_str) != Some("2.0") {
            return Err("Invalid request: jsonrpc must be \"2.0\"");
        }
        method.as_str().ok_or("Invalid request: method must be a string")
    };
    let Some(id) = members.iter().rev().find(|m| m.key == "id").map(|m| &text[m.start..m.end]) else {
        // A notification: any method is taken, nothing is answered.
        return match valid() {
            Ok(name) => Answer { method: printable(name), ..accepted() },
            Err(why) => fail(400, "null", -32600, why),
        };
    };
    if !id_ok(id) {
        return fail(400, "null", -32600, "Invalid request: id must be a string or an integer");
    }
    let name = match valid() {
        Ok(n) => n,
        Err(why) => return fail(400, id, -32600, why),
    };
    let mut a = match name {
        "initialize" => result(id, &initialize(info)),
        "ping" => result(id, "{}"),
        "tools/list" => result(id, &tools_list(tools, host)),
        "tools/call" => tools_call(id, msg.get("params"), now_ns, tools, host, run),
        other => fail(200, id, -32601, &format!("Method not found: {}", printable(other))),
    };
    a.method = printable(name);
    a
}

fn initialize(info: &Info<'_>) -> String {
    let mut s = format!(
        "{{\"protocolVersion\":\"{PROTOCOL}\",\"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},\"serverInfo\":{{\"name\":\"BitsOS\",\"version\":\"{}\"}},\"instructions\":",
        info.version
    );
    json::escape_into(info.instructions, &mut s);
    s.push('}');
    s
}

pub fn safety_name(s: Safety) -> &'static str {
    match s {
        Safety::Read => "read",
        Safety::Change => "change",
        Safety::DeleteOverwrite => "delete_overwrite",
    }
}

/// The safety class from its name in `tools/list`.
pub fn safety_from(s: &str) -> Option<Safety> {
    match s {
        "read" => Some(Safety::Read),
        "change" => Some(Safety::Change),
        "delete_overwrite" => Some(Safety::DeleteOverwrite),
        _ => None,
    }
}

fn tools_list<S: ToolSet>(tools: &S, host: &dyn Host) -> String {
    let mut s = String::from("{\"tools\":[");
    for (i, t) in tools.sorted().into_iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"name\":");
        json::escape_into(t.name(), &mut s);
        s.push_str(",\"description\":");
        json::escape_into(t.description(), &mut s);
        s.push_str(",\"inputSchema\":");
        // The registry ktest checks every built-in schema and `svc` every registered one (it keeps its own
        // serialisation); one that slipped through must not break the whole list.
        if t.is_service() || Json::parse(t.input_schema()).is_ok() {
            s.push_str(t.input_schema());
        } else {
            host.bad_schema(t.name(), t.owner());
            s.push_str("{\"type\":\"object\"}");
        }
        // MC-6: a service of a uid other than 0 only claims its safety class; until PM-1 checks such claims
        // its tools are listed with the cautious hints (not read-only, maybe destructive) and verified:false.
        let verified = t.safety_verified();
        s.push_str(&format!(
            ",\"annotations\":{{\"readOnlyHint\":{},\"destructiveHint\":{},\"owner\":",
            verified && t.safety() == Safety::Read,
            !verified || t.safety() == Safety::DeleteOverwrite
        ));
        json::escape_into(t.owner(), &mut s);
        s.push_str(&format!(",\"safety\":\"{}\"", safety_name(t.safety())));
        if let Some(pid) = t.service_pid() {
            s.push_str(&format!(",\"service_pid\":{pid},\"verified\":{verified}"));
        }
        s.push_str("}}");
    }
    s.push_str("]}");
    s
}

fn tool_result(id: &str, outputs: &[ToolOutput], is_error: bool) -> Answer {
    let mut c = String::from("{\"content\":[");
    for (i, o) in outputs.iter().enumerate() {
        if i > 0 {
            c.push(',');
        }
        match o {
            ToolOutput::Text(t) => {
                c.push_str("{\"type\":\"text\",\"text\":");
                json::escape_into(t, &mut c);
                c.push('}');
            }
            ToolOutput::Json(j) => {
                c.push_str("{\"type\":\"text\",\"text\":");
                json::escape_into(&j.to_string(), &mut c);
                c.push('}');
            }
            ToolOutput::Png(p) => {
                c.push_str("{\"type\":\"image\",\"data\":\"");
                b64::encode_into(p, &mut c);
                c.push_str("\",\"mimeType\":\"image/png\"}");
            }
        }
    }
    c.push_str(&format!("],\"isError\":{is_error}}}"));
    Answer { tool_failed: is_error, ..result(id, &c) }
}

fn text_result(id: &str, text: String, is_error: bool) -> Answer {
    tool_result(id, &[ToolOutput::Text(text)], is_error)
}

pub const TOO_BIG: &str = "The answer does not fit into BitsOS's 4 MB for answers";
/// Room kept for the note at a cut and the envelope's id (at most `ID_MAX`).
const NOTE_ROOM: usize = 160;

/// Bytes `c` takes inside a JSON string (`json::escape_into`'s rules).
pub fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 || c == '\u{7f}' => 6,
        c => c.len_utf8(),
    }
}

/// The longest start of `t` whose escaped form takes at most `budget` bytes: (its byte length, escaped size).
fn fitting_prefix(t: &str, budget: usize) -> (usize, usize) {
    let mut used = 0;
    for (i, c) in t.char_indices() {
        let n = escaped_len(c);
        if used + n > budget {
            return (i, used);
        }
        used += n;
    }
    (t.len(), used)
}

/// A tool's result as an answer that fits into `ANSWER_MAX`: text items are cut at the end (a note says how much
/// is missing), in order, so a long log or program output still answers with its start; pictures cannot be cut,
/// so a result that does not fit without its text is the `TOO_BIG` error.
fn fitted(id: &str, outputs: &[ToolOutput], is_error: bool, host: &dyn Host) -> Answer {
    let a = tool_result(id, outputs, is_error);
    if a.body.len() <= ANSWER_MAX {
        return a;
    }
    let texts: usize = outputs.iter().map(|o| if let ToolOutput::Text(t) = o { t.chars().map(escaped_len).sum() } else { 0 }).sum();
    let fixed = a.body.len() - texts;
    let notes = NOTE_ROOM * outputs.len();
    let Some(mut budget) = ANSWER_MAX.checked_sub(fixed + notes) else {
        return text_result(id, String::from(TOO_BIG), true);
    };
    let cut: Vec<ToolOutput> = outputs
        .iter()
        .map(|o| match o {
            ToolOutput::Text(t) => {
                let (keep, used) = fitting_prefix(t, budget);
                budget -= used;
                if keep == t.len() {
                    return o.clone();
                }
                let mut s = String::from(&t[..keep]);
                s.push_str(&format!("\n[... cut: the last {} bytes of this text are not shown; BitsOS answers are at most 4 MB]", t.len() - keep));
                ToolOutput::Text(s)
            }
            other => other.clone(),
        })
        .collect();
    host.answer_cut(a.body.len(), ANSWER_MAX);
    let a = tool_result(id, &cut, is_error);
    if a.body.len() > ANSWER_MAX { text_result(id, String::from(TOO_BIG), true) } else { a }
}

/// A call's deadline without `timeout_ms`: the tool's schema may give its own default
/// (`properties.timeout_ms.default`, e.g. `run`'s 30 s), else `DEFAULT_TIMEOUT_MS`.
fn default_timeout_ms(schema: Option<&Json>) -> u64 {
    schema
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.get("timeout_ms"))
        .and_then(|t| t.get("default"))
        .and_then(Json::as_u64)
        .filter(|ms| (1..=MAX_TIMEOUT_MS).contains(ms))
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn tools_call<S: ToolSet>(id: &str, params: Option<&Json>, now_ns: u64, tools: &S, host: &dyn Host, run: &mut Runner<'_, S::Tool>) -> Answer {
    let bad = |msg: String| fail(200, id, -32602, &msg);
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Json::as_str) else {
        return bad(String::from("tools/call needs params with a tool name"));
    };
    let shown = printable(name);
    let with_tool = |mut a: Answer| {
        a.tool = shown.clone();
        a
    };
    let Some(tool) = tools.find(name) else { return with_tool(bad(format!("Unknown tool: {shown} (tools of user-space services come and go: call tools/list again)"))) };
    let empty = Json::Obj(Vec::new());
    let args = match params.and_then(|p| p.get("arguments")) {
        None | Some(Json::Null) => &empty,
        Some(a @ Json::Obj(_)) => a,
        Some(_) => return with_tool(bad(String::from("arguments must be an object"))),
    };
    let tool_schema = Json::parse(tool.input_schema()).ok();
    if let Some(s) = &tool_schema
        && let Err(msg) = schema::check_args(s, args)
    {
        return with_tool(bad(msg));
    }
    let timeout_ms = match args.get("timeout_ms") {
        None | Some(Json::Null) => default_timeout_ms(tool_schema.as_ref()),
        Some(v) => match v.as_u64() {
            Some(ms) if (1..=MAX_TIMEOUT_MS).contains(&ms) => ms,
            _ => return with_tool(text_result(id, format!("ERROR: timeout_ms must be 1 to {MAX_TIMEOUT_MS}"), true)),
        },
    };
    if let Err(reason) = host.check(tool.name()) {
        return with_tool(text_result(id, format!("Refused: {reason}"), true));
    }
    let a = match run(&tool, now_ns.saturating_add(timeout_ms * 1_000_000), args) {
        Err(text) => fitted(id, &[ToolOutput::Text(text)], true, host),
        Ok(r) => finished(id, r, host),
    };
    if a.tool_failed {
        host.tool_failed(tool.name());
    }
    with_tool(a)
}

fn finished(id: &str, r: ToolResult, host: &dyn Host) -> Answer {
    let bad = |msg: String| fail(200, id, -32602, &msg);
    match r {
        Ok(out) => fitted(id, &out, false, host),
        Err(ToolError::BadArgument(what)) => bad(format!("Bad argument: {what}")),
        Err(ToolError::Refused(why)) => fitted(id, &[ToolOutput::Text(format!("Refused: {why}"))], true, host),
        Err(ToolError::Failed(why)) => fitted(id, &[ToolOutput::Text(format!("ERROR: {why}"))], true, host),
    }
}
